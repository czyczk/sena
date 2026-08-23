//! senaenc CLI.

use sena_core::{account, Profile, MIN_TOTAL_KBPS, SENAV_THRESHOLD_KBPS};
use sena_enc::{encode, EncoderConfig};
use std::path::{Path, PathBuf};

fn usage() -> ! {
    eprintln!(
        "usage: senaenc [--profile 300|600] [--opus-original|--opus-senav] \
         <bitrate_kbps> <in.wav> <out.sena>\n\
         bitrate >= {MIN_TOTAL_KBPS} kbit/s (below that, use plain Opus)\n\
         default profile: 600; default opus: original <= {SENAV_THRESHOLD_KBPS}k, senav above"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut profile = Profile::At600;
    let mut opus_mode: Option<bool> = None; // None = auto
    let keep_workdir = args.iter().any(|a| a == "--keep-workdir");
    let mut rest = vec![];
    let mut i = 0;
    while i < args.len() {
        match args[i].as_str() {
            "--profile" => {
                i += 1;
                profile = match args.get(i).map(|s| s.as_str()) {
                    Some("300") => Profile::At300,
                    Some("600") => Profile::At600,
                    _ => usage(),
                };
            }
            "--opus-original" => opus_mode = Some(false),
            "--opus-senav" => opus_mode = Some(true),
            "--keep-workdir" => {}
            a if a.starts_with('-') && rest.is_empty() => usage(),
            a => rest.push(a.to_string()),
        }
        i += 1;
    }
    if rest.len() != 3 {
        usage();
    }
    let kbps: u32 = rest[0].parse().unwrap_or_else(|_| usage());
    let input = PathBuf::from(&rest[1]);
    let output = PathBuf::from(&rest[2]);

    if account(kbps, profile).is_none() {
        eprintln!("error: total bitrate {kbps} kbit/s is below the Sena minimum ({MIN_TOTAL_KBPS} kbit/s); use plain Opus.");
        std::process::exit(3);
    }
    let (xhe_k, opus_k) = account(kbps, profile).unwrap();
    let use_senav = opus_mode.unwrap_or(kbps > SENAV_THRESHOLD_KBPS);
    eprintln!(
        "senaenc: profile @{}  total {}k -> xHE-AAC {}k (deducted) + Opus {}k ({})",
        profile.crossover_hz(),
        kbps,
        xhe_k,
        opus_k,
        if use_senav { "opusenc-senav" } else { "opusenc (original)" }
    );

    // locate binaries: same dir as the executable first, then PATH
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_path_buf()))
        .unwrap_or_default();
    let find = |name: &str| -> Option<String> {
        let cand = exe_dir.join(name);
        if cand.exists() {
            return Some(cand.to_string_lossy().into_owned());
        }
        which_crate(name)
    };
    let exhale = find("exhale").unwrap_or_else(|| {
        eprintln!("error: exhale (>= 1.2.2) not found next to senaenc or on PATH");
        std::process::exit(4);
    });
    let opus_name = if use_senav { "opusenc-senav" } else { "opusenc" };
    let opusenc = find(opus_name).unwrap_or_else(|| {
        eprintln!("error: {opus_name} not found next to senaenc or on PATH");
        std::process::exit(4);
    });

    let wd = std::env::temp_dir().join(format!("senaenc-{}", std::process::id()));
    let cfg = EncoderConfig {
        profile,
        total_kbps: kbps,
        use_senav,
        exhale: &exhale,
        opusenc: &opusenc,
        workdir: &wd,
    };
    if let Err(e) = encode(&cfg, &input, &output) {
        eprintln!("error: {e}");
        let _ = std::fs::remove_dir_all(&wd);
        std::process::exit(1);
    }
    if keep_workdir {
        eprintln!("senaenc: workdir kept at {}", wd.display());
    } else {
        let _ = std::fs::remove_dir_all(&wd);
    }
    eprintln!("senaenc: wrote {}", output.display());
}

/// Minimal PATH lookup without extra deps.
fn which_crate(name: &str) -> Option<String> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let cand = Path::new(&dir).join(name);
        if cand.is_file() {
            return Some(cand.to_string_lossy().into_owned());
        }
        // windows .exe suffix
        #[cfg(windows)]
        {
            let cand = cand.with_extension("exe");
            if cand.is_file() {
                return Some(cand.to_string_lossy().into_owned());
            }
        }
    }
    None
}
