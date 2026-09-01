//! senaenc CLI.

use sena_core::{account, Profile, MIN_TOTAL_KBPS, SENAV_THRESHOLD_KBPS};
use sena_enc::{check_version, exhale_version, opusenc_version, version_ge, EncoderConfig};
use std::path::{Path, PathBuf};

fn usage() -> ! {
    eprintln!(
        "usage: senaenc [--profile 300|600] [--opus-original|--opus-senav] \
         <bitrate_kbps> <in.wav|-> <out.sena>\n\
         senaenc doctor                              check required encoder tools\n\
         bitrate >= {MIN_TOTAL_KBPS} kbit/s (below that, use plain Opus)\n\
         default profile: 600; default opus: original <= {SENAV_THRESHOLD_KBPS}k, senav above"
    );
    std::process::exit(2);
}

/// Name candidates to probe in one directory: the name as-is, then the
/// Windows executable extensions (.exe plus PATHEXT). On Windows a bare
/// `exhale` path never resolves to `exhale.exe`, so the end-to-end lookup
/// next to `senaenc.exe` used to report an adjacent `exhale.exe` as missing.
fn tool_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    push(dir.join(name));
    #[cfg(windows)]
    {
        push(dir.join(format!("{name}.exe")));
        if let Ok(pathext) = std::env::var("PATHEXT") {
            for e in pathext.split(';') {
                let e = e.trim_start_matches('.').trim();
                if !e.is_empty() {
                    push(dir.join(format!("{name}.{e}")));
                }
            }
        }
    }
    out
}

struct ToolFinder {
    exe_dir: PathBuf,
}

impl ToolFinder {
    fn new() -> Self {
        let exe_dir = std::env::current_exe()
            .ok()
            .and_then(|p| p.parent().map(|d| d.to_path_buf()))
            .unwrap_or_default();
        Self { exe_dir }
    }

    /// Search next to the executable first, then on PATH.
    /// Returns (path, source) where source is "next to senaenc" or "PATH".
    fn find(&self, name: &str) -> Option<(PathBuf, &'static str)> {
        for cand in tool_candidates(&self.exe_dir, name) {
            if cand.is_file() {
                return Some((cand, "next to senaenc"));
            }
        }
        let path_var = std::env::var_os("PATH")?;
        for dir in std::env::split_paths(&path_var) {
            for cand in tool_candidates(&dir, name) {
                if cand.is_file() {
                    return Some((cand, "PATH"));
                }
            }
        }
        None
    }
}

/// `senaenc doctor`: verify the encoder tools without encoding anything.
/// exhale and opusenc are required (missing/unrunnable/too old = fatal);
/// opusenc-senav is optional (missing only warns and limits useful modes).
/// Returns the process exit code (4 on fatal problems, 0 otherwise).
fn doctor() -> i32 {
    let finder = ToolFinder::new();
    let mut fatal = 0u32;
    let mut warnings = 0u32;
    eprintln!("senaenc doctor: checking encoder tools");
    eprintln!(
        "  search order: {} (next to senaenc), then PATH",
        finder.exe_dir.display()
    );

    // exhale: required, >= 1.2.2
    match finder.find("exhale") {
        Some((p, src)) => match exhale_version(&p.to_string_lossy()) {
            Ok(v) if version_ge(&v, "1.2.2") => {
                eprintln!("  [ok] exhale {v} ({}, {src})", p.display());
            }
            Ok(v) => {
                fatal += 1;
                eprintln!("  [fatal] exhale {v} at {} ({src}): older than required 1.2.2", p.display());
            }
            Err(e) => {
                fatal += 1;
                eprintln!("  [fatal] exhale at {} ({src}): {e}", p.display());
            }
        },
        None => {
            fatal += 1;
            eprintln!("  [fatal] exhale (>= 1.2.2) not found next to senaenc or on PATH");
            eprintln!("         impact: the xHE-AAC low band cannot be encoded at all.");
            eprintln!("         fix: put exhale.exe next to senaenc.exe ({}) or add its folder to PATH", finder.exe_dir.display());
        }
    }

    // opusenc: required (original mode; also the fallback when senav is absent)
    match finder.find("opusenc") {
        Some((p, src)) => match opusenc_version(&p.to_string_lossy()) {
            Ok(v) => {
                eprintln!("  [ok] opusenc ({v}) ({}, {src})", p.display());
            }
            Err(e) => {
                fatal += 1;
                eprintln!("  [fatal] opusenc at {} ({src}): {e}", p.display());
            }
        },
        None => {
            fatal += 1;
            eprintln!("  [fatal] opusenc not found next to senaenc or on PATH");
            eprintln!("         impact: the high band cannot be encoded at all.");
            eprintln!("         fix: put opusenc.exe next to senaenc.exe ({}) or add its folder to PATH", finder.exe_dir.display());
        }
    }

    // opusenc-senav: optional
    match finder.find("opusenc-senav") {
        Some((p, src)) => match check_version(&p.to_string_lossy(), Some("Opus SenaV"), "opusenc-senav") {
            Ok(()) => {
                eprintln!("  [ok] opusenc-senav (SenaV build) ({}, {src})", p.display());
            }
            Err(e) => {
                warnings += 1;
                eprintln!("  [warn] opusenc-senav at {} ({src}): {e}", p.display());
                eprintln!("         impact: treated as absent - automatic senav selection (bitrate > {SENAV_THRESHOLD_KBPS} kbit/s) and --opus-senav will fail.");
            }
        },
        None => {
            warnings += 1;
            eprintln!("  [warn] opusenc-senav not found (optional)");
            eprintln!("         impact: --opus-senav and automatic senav selection (bitrate > {SENAV_THRESHOLD_KBPS} kbit/s) are unavailable;");
            eprintln!("         encoding still works up to {SENAV_THRESHOLD_KBPS} kbit/s (auto) or at any rate with --opus-original.");
            eprintln!("         fix: optional - put opusenc-senav.exe next to senaenc.exe ({}) or add its folder to PATH", finder.exe_dir.display());
        }
    }

    if fatal > 0 {
        eprintln!("doctor: {fatal} fatal problem(s) - fix them before encoding");
        4
    } else if warnings > 0 {
        eprintln!("doctor: ok (required tools present), {warnings} warning(s)");
        0
    } else {
        eprintln!("doctor: ok (all tools present)");
        0
    }
}

fn main() {
    use std::io::IsTerminal;
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.len() == 1 && args[0] == "doctor" {
        std::process::exit(doctor());
    }
    let mut profile = Profile::At600;
    let mut opus_mode: Option<bool> = None; // None = auto
    let keep_workdir = args.iter().any(|a| a == "--keep-workdir");
    let force = args.iter().any(|a| a == "--force");
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
            "--force" => {}
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
    let stdin_input = input.as_os_str() == "-";
    if output.exists() && !force {
        if stdin_input || !std::io::stdin().is_terminal() {
            eprintln!(
                "error: output file {} already exists; use --force to overwrite",
                output.display()
            );
            std::process::exit(1);
        }
        eprint!("overwrite {}? [y/N] ", output.display());
        let mut line = String::new();
        if std::io::stdin().read_line(&mut line).is_err() {
            std::process::exit(1);
        }
        let ok = matches!(line.trim(), "y" | "Y" | "yes" | "YES");
        if !ok {
            eprintln!("aborted");
            std::process::exit(1);
        }
    }
    eprintln!(
        "senaenc: profile @{}  total {}k -> xHE-AAC {}k (deducted) + Opus {}k ({})",
        profile.crossover_hz(),
        kbps,
        xhe_k,
        opus_k,
        if use_senav { "opusenc-senav" } else { "opusenc (original)" }
    );

    // locate binaries: same dir as the executable first (with Windows
    // .exe/PATHEXT variants), then PATH
    let finder = ToolFinder::new();
    let (exhale, _) = finder.find("exhale").unwrap_or_else(|| {
        eprintln!("error: exhale (>= 1.2.2) not found next to senaenc or on PATH");
        std::process::exit(4);
    });
    let exhale = exhale.to_string_lossy().into_owned();
    let opus_name = if use_senav { "opusenc-senav" } else { "opusenc" };
    let (opusenc, _) = finder.find(opus_name).unwrap_or_else(|| {
        eprintln!("error: {opus_name} not found next to senaenc or on PATH");
        std::process::exit(4);
    });
    let opusenc = opusenc.to_string_lossy().into_owned();

    let wd = std::env::temp_dir().join(format!("senaenc-{}", std::process::id()));
    let cfg = EncoderConfig {
        profile,
        total_kbps: kbps,
        use_senav,
        exhale: &exhale,
        opusenc: &opusenc,
        workdir: &wd,
    };
    // Streaming encode: the input is consumed as the DSP makes progress, so
    // feeding hosts (foobar2000 converter) see the real pipeline tempo.
    let result = if stdin_input {
        let mut stdin = std::io::stdin().lock();
        sena_enc::encode_stream(&cfg, &mut stdin, &output)
    } else {
        sena_enc::encode(&cfg, &input, &output)
    };
    if let Err(e) = result {
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
