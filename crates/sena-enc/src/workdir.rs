//! Workdir selection and lifecycle: unique subdirs under a base dir, a
//! preflight free-space estimate with fallback, and stale-sweep cleanup
//! of leftovers from unperceivable exits (SIGKILL / TerminateProcess).

use super::Error;

/// Create `base/senaenc-<pid>-<rand>` atomically (create_dir fails on
/// collision -> retry with a fresh suffix; safe for concurrent instances).
pub fn create_unique_dir(base: &std::path::Path) -> Result<std::path::PathBuf, Error> {
    for _ in 0..8 {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.subsec_nanos())
            .unwrap_or(0);
        let dir = base.join(format!("senaenc-{}-{nanos:08x}", std::process::id()));
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(Error::Io(e)),
        }
    }
    Err(Error::Format("could not allocate a unique workdir name".into()))
}

pub fn free_space(dir: &std::path::Path) -> Option<u64> {
    fs2::free_space(dir).ok()
}

/// Conservative byte estimate of the workdir footprint: the spooled LF
/// WAV (s16) is exact given the input length; the encoder outputs scale
/// with the total bitrate (VBR overshoot and fs metadata are absorbed by
/// the 1.25x headroom — concurrent-instance races are handled by the
/// ENOSPC spill path instead, not by this margin).
pub fn estimate_bytes(
    lf_rate: u32,
    in_rate: u32,
    frames_in: u64,
    total_kbps: u32,
) -> u64 {
    let secs = frames_in as f64 / in_rate as f64;
    let lf = frames_in * lf_rate as u64 / in_rate as u64 * 2 * 2 + 1_000_000;
    let outs = (total_kbps as f64 * 1000.0 / 8.0 * secs * 1.25) as u64;
    ((lf + outs) as f64 * 1.25) as u64
}

/// Delete `base/senaenc-*` subdirs whose mtime is older than one hour —
/// leftovers of processes killed without cleanup (a running encode's dir
/// mtime stays fresh because it keeps writing). Best effort, no symlink
/// following, never touches non-senaenc entries.
pub fn sweep_stale(base: &std::path::Path) {
    let cutoff = match std::time::SystemTime::now()
        .checked_sub(std::time::Duration::from_secs(3600))
    {
        Some(t) => t,
        None => return,
    };
    let rd = match std::fs::read_dir(base) {
        Ok(r) => r,
        Err(_) => return,
    };
    for e in rd.flatten() {
        let name = e.file_name();
        if !name.to_string_lossy().starts_with("senaenc-") {
            continue;
        }
        let md = match e.metadata() {
            Ok(m) => m,
            Err(_) => continue,
        };
        if !md.is_dir() || md.file_type().is_symlink() {
            continue;
        }
        match md.modified() {
            Ok(t) if t < cutoff => {
                let _ = std::fs::remove_dir_all(e.path());
            }
            _ => {}
        }
    }
}

/// Shared registry of workdirs to remove on exit. The encoder registers
/// each dir as it is created; the CLI installs signal handlers that clean
/// the registry, and its guard cleans it on normal/error/panic exits
/// unless disarmed (--keep-workdir).
#[derive(Clone, Default)]
pub struct Cleanup {
    paths: std::sync::Arc<std::sync::Mutex<Vec<std::path::PathBuf>>>,
}

impl Cleanup {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, p: std::path::PathBuf) {
        self.paths.lock().expect("cleanup registry").push(p);
    }

    pub fn clean_now(&self) {
        let paths: Vec<_> = std::mem::take(&mut *self.paths.lock().expect("cleanup registry"));
        for p in paths {
            let _ = std::fs::remove_dir_all(p);
        }
    }
}
