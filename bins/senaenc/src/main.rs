//! senaenc CLI.

use sena_core::{
    MidCodec, Profile, RECOMMENDED_MIN_TOTAL_KBPS, SENAV_THRESHOLD_KBPS, THREE_TRACK_MIN_KBPS,
    TOPBAND_DEFAULT_RANGE, XFI_MAX_TOTAL_KBPS, XFI_MIN_TOTAL_KBPS, XFI_MID_KBPS, account,
    account3, three_track_tag, xfi_hf_kbps,
};
use sena_enc::{EncoderConfig, exhale_version, opusenc_version, version_ge};
use std::path::{Path, PathBuf};

use clap::{CommandFactory, Parser, Subcommand};

/// senaenc: WAV -> .sena encoder CLI.
#[derive(Parser)]
#[command(
    name = "senaenc",
    version,
    about = "Sena encoder: WAV -> .sena (exhale LF + Opus/xHE-AAC mid + Opus top)",
    override_usage = "senaenc [OPTIONS] <BITRATE_KBPS> <IN.WAV|-> <OUT.SENA>\n       senaenc doctor",
    args_conflicts_with_subcommands = true
)]
struct Cli {
    #[command(subcommand)]
    command: Option<Cmd>,

    /// Encoding profile: <LF split Hz><mid codec>. 300o/600o = Opus mid
    /// (bare 300/600 still accepted); 300xfi/600xfi = Fraunhofer-IIS
    /// xHE-AAC CBR 128k @ 44.1 kHz mid via ezcd-cli (always three-track;
    /// totals 256-288k only, extras feed the top band 96k->128k).
    #[arg(long, default_value = "600o", value_parser = parse_profile, value_name = "PROFILE")]
    profile: (Profile, MidCodec),

    /// Force the stock opusenc build for the Opus rails (default: original
    /// at totals <= 192k, senav above).
    #[arg(long, conflicts_with = "opus_senav")]
    opus_original: bool,
    /// Force the SenaV-tuned opusenc build for the Opus rails.
    #[arg(long)]
    opus_senav: bool,

    /// Passed straight to the opusenc-senav mid encode as
    /// AUDIFF_TOPBAND_STEREO (kbps). Default: the Opus budget when senav is
    /// used at totals in [192, 256); ignored (with a warning) under
    /// --opus-original; never applied to the 12 kHz+ top track.
    #[arg(long, value_parser = clap::value_parser!(u64).range(1..=500), value_name = "1-500")]
    opus_topband_stereo: Option<u64>,

    /// Optional gentle high-frequency tilt on the Opus band (percent of the
    /// reference curve; 0 = off; default off).
    #[arg(long, value_parser = parse_tilt, value_name = "0-100")]
    hf_tilt: Option<f64>,

    /// Parent dir for the spool workdir (e.g. a RAM disk like Z:\);
    /// falls back to the system temp dir when missing or full. Workdirs
    /// are per-run unique, cleaned on every perceivable exit, and stale
    /// leftovers (>1 h) are swept at startup.
    #[arg(long, value_name = "DIR")]
    tmp_dir: Option<PathBuf>,

    /// Keep the workdir after a successful encode (debug; prints its path).
    #[arg(long)]
    keep_workdir: bool,
    /// Overwrite an existing output file.
    #[arg(long)]
    force: bool,
    /// Encode below the recommended minimum bitrate anyway.
    #[arg(long)]
    bypass_recommendations: bool,

    /// Total Sena bitrate in kbit/s (see README for the per-profile
    /// minimums and the recommended floor).
    #[arg(value_name = "BITRATE_KBPS")]
    bitrate_kbps: Option<u32>,
    /// Input WAV file; '-' reads stdin (foobar2000 converter pipes).
    #[arg(value_name = "IN.WAV|-")]
    input: Option<PathBuf>,
    /// Output .sena path (refuses to overwrite without --force).
    #[arg(value_name = "OUT.SENA")]
    output: Option<PathBuf>,
}

#[derive(Subcommand)]
enum Cmd {
    /// Check the required encoder tools (exhale, opusenc, ezcd-cli).
    Doctor,
}

fn parse_profile(s: &str) -> Result<(Profile, MidCodec), String> {
    match s {
        "300" | "300o" => Ok((Profile::At300, MidCodec::Opus)),
        "600" | "600o" => Ok((Profile::At600, MidCodec::Opus)),
        "300xfi" => Ok((Profile::At300, MidCodec::Xhe)),
        "600xfi" => Ok((Profile::At600, MidCodec::Xhe)),
        _ => Err("expected 300o | 600o | 300xfi | 600xfi".into()),
    }
}

fn parse_tilt(s: &str) -> Result<f64, String> {
    let v: f64 = s.parse().map_err(|_| "expected a number in 0..=100".to_string())?;
    if (0.0..=100.0).contains(&v) {
        Ok(v)
    } else {
        Err("expected a number in 0..=100".into())
    }
}

/// Name candidates to probe in one directory: the name as-is, then the
/// Windows executable extensions (.exe plus PATHEXT). On Windows a bare
/// `exhale` path never resolves to `exhale.exe`, so the end-to-end lookup
/// next to `senaenc.exe` used to report an adjacent `exhale.exe` as missing.
/// Windows .exe tools are runnable here: native Windows, or WSL (the
/// interop binfmt handler exists even when WSL_DISTRO_NAME didn't
/// propagate into the environment).
fn windows_exe_runnable() -> bool {
    cfg!(windows)
        || std::env::var_os("WSL_DISTRO_NAME").is_some()
        || std::path::Path::new("/proc/sys/fs/binfmt_misc/WSLInterop").exists()
}

fn tool_candidates(dir: &Path, name: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    let mut push = |p: PathBuf| {
        if !out.contains(&p) {
            out.push(p);
        }
    };
    push(dir.join(name));
    if windows_exe_runnable() {
        push(dir.join(format!("{name}.exe")));
        #[cfg(windows)]
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
                eprintln!(
                    "  [fatal] exhale {v} at {} ({src}): older than required 1.2.2",
                    p.display()
                );
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
            eprintln!(
                "         fix: put exhale.exe next to senaenc.exe ({}) or add its folder to PATH",
                finder.exe_dir.display()
            );
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
            eprintln!(
                "         fix: put opusenc.exe next to senaenc.exe ({}) or add its folder to PATH",
                finder.exe_dir.display()
            );
        }
    }

    // opusenc-senav: optional; must carry the SenaV m>=3 marker to be usable
    match finder.find("opusenc-senav") {
        Some((p, src)) => {
            match sena_enc::check_senav_version(&p.to_string_lossy()) {
                Ok(()) => {
                    eprintln!(
                        "  [ok] opusenc-senav (SenaV build) ({}, {src})",
                        p.display()
                    );
                }
                Err(e) => {
                    warnings += 1;
                    eprintln!("  [warn] opusenc-senav at {} ({src}): {e}", p.display());
                    eprintln!(
                        "         impact: treated as absent - automatic senav selection (bitrate > {SENAV_THRESHOLD_KBPS} kbit/s) and --opus-senav will fail."
                    );
                }
            }
        }
        None => {
            warnings += 1;
            eprintln!("  [warn] opusenc-senav not found (optional)");
            eprintln!(
                "         impact: --opus-senav and automatic senav selection (bitrate > {SENAV_THRESHOLD_KBPS} kbit/s) are unavailable;"
            );
            eprintln!(
                "         encoding still works up to {SENAV_THRESHOLD_KBPS} kbit/s (auto) or at any rate with --opus-original."
            );
            eprintln!(
                "         fix: optional - put opusenc-senav.exe next to senaenc.exe ({}) or add its folder to PATH",
                finder.exe_dir.display()
            );
        }
    }

    // ezcd-cli: optional; required only by the xfi profile family
    match finder.find("ezcd-cli").or_else(|| finder.find("ezcd-cli-portable")) {
        Some((p, src)) => {
            let ok = std::process::Command::new(&p)
                .args(["encoders"])
                .output()
                .map(|o| String::from_utf8_lossy(&o.stdout).contains("xhe-aac-fhg-iis"))
                .unwrap_or_else(|e| {
                    eprintln!("  [warn] cannot run {}: {e}", p.display());
                    false
                });
            if ok {
                eprintln!("  [ok] ezcd-cli (xhe-aac-fhg-iis present) ({}, {src})", p.display());
            } else {
                warnings += 1;
                eprintln!("  [warn] ezcd-cli at {} ({src}): `encoders` lacks xhe-aac-fhg-iis", p.display());
                eprintln!("         impact: the xfi profile family is unavailable.");
            }
        }
        None => {
            warnings += 1;
            eprintln!("  [warn] ezcd-cli not found (optional; tried ezcd-cli, ezcd-cli-portable)");
            eprintln!("         impact: the xfi profile family (Fraunhofer IIS xHE-AAC mid) is unavailable.");
            eprintln!(
                "         fix: optional - put ezcd-cli.exe or ezcd-cli-portable.exe next to senaenc.exe ({}) or add its folder to PATH",
                finder.exe_dir.display()
            );
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
    let cli = Cli::parse();
    if matches!(cli.command, Some(Cmd::Doctor)) {
        std::process::exit(doctor());
    }
    let (profile, mid_codec) = cli.profile;
    let opus_mode: Option<bool> = if cli.opus_original {
        Some(false)
    } else if cli.opus_senav {
        Some(true)
    } else {
        None
    };
    let keep_workdir = cli.keep_workdir;
    let force = cli.force;
    let bypass = cli.bypass_recommendations;
    let hf_tilt_pct = cli.hf_tilt.filter(|&v| v > 0.0);
    let topband_flag = cli.opus_topband_stereo.map(|v| v as u32);
    let tmpdir = cli.tmp_dir;
    let (kbps, input, output) = match (cli.bitrate_kbps, cli.input, cli.output) {
        (Some(b), Some(i), Some(o)) => (b, i, o),
        _ => Cli::command()
            .error(
                clap::error::ErrorKind::MissingRequiredArgument,
                "expected <BITRATE_KBPS> <IN.WAV|-> <OUT.SENA>",
            )
            .exit(),
    };

    let xfi = mid_codec == MidCodec::Xhe;
    if xfi && !(XFI_MIN_TOTAL_KBPS..=XFI_MAX_TOTAL_KBPS).contains(&kbps) {
        eprintln!(
            "error: the xfi profile family (Fraunhofer IIS xHE-AAC mid) accepts only              {XFI_MIN_TOTAL_KBPS}-{XFI_MAX_TOTAL_KBPS} kbit/s; requested {kbps}k"
        );
        std::process::exit(3);
    }
    if !xfi && account(kbps, profile).is_none() {
        let min = profile.min_total_kbps();
        eprintln!(
            "error: minimum bitrate for profile @{} is {min} kbit/s ({}k xHE-AAC + 32k Opus floor); requested {kbps}k. Use plain Opus at this rate.",
            profile.crossover_hz(),
            profile.deduct_kbps(),
        );
        std::process::exit(3);
    }
    if kbps < RECOMMENDED_MIN_TOTAL_KBPS && !bypass {
        eprintln!(
            "error: {kbps} kbit/s is below the recommended Sena minimum ({RECOMMENDED_MIN_TOTAL_KBPS} kbit/s)."
        );
        eprintln!(
            "       at this rate a plain Opus encode serves better: opusenc --bitrate {kbps} in.wav out.opus"
        );
        eprintln!("       re-run with --bypass-recommendations to encode Sena anyway.");
        std::process::exit(5);
    }
    let three_track = xfi || kbps >= THREE_TRACK_MIN_KBPS;
    if three_track && !xfi && account3(kbps, profile).is_none() {
        // Unreachable for >= 256k with the defined profiles; kept as a guard.
        eprintln!("error: total bitrate {kbps}k cannot fund the three-track layout");
        std::process::exit(3);
    }
    let (xhe_k, opus_k) = account(kbps, profile).unwrap();
    let mid_k = if xfi {
        XFI_MID_KBPS
    } else if three_track {
        account3(kbps, profile).unwrap().1
    } else {
        opus_k
    };
    // xfi: the mid encode is ezcd-cli (not Opus); --opus-* only selects the
    // top-band encoder build. use_senav still governs the top-band env.
    let use_senav = opus_mode.unwrap_or(kbps > SENAV_THRESHOLD_KBPS);

    // Topband-stereo resolution (mid encode only; the 15600 Hz+ top track
    // never gets the knob):
    // - explicit flag wins, passed through verbatim (no Sena arithmetic);
    // - under --opus-original it is meaningless: warn and ignore;
    // - opus-senav two-track in [192, 256) defaults to the Opus budget;
    // - the three-track layout has no top bands in the mid encode -> off.
    let topband_kbps = if xfi {
        if let Some(v) = topband_flag {
            eprintln!("senaenc: warning: --opus-topband-stereo {v} ignored (xfi mid is xHE-AAC)");
        }
        None
    } else if !use_senav {
        if let Some(v) = topband_flag {
            eprintln!(
                "senaenc: warning: --opus-topband-stereo {v} ignored (--opus-original selected)"
            );
        }
        None
    } else if let Some(v) = topband_flag {
        Some(v)
    } else if !three_track && TOPBAND_DEFAULT_RANGE.contains(&kbps) {
        Some(mid_k)
    } else {
        None
    };

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
    // xfi family: the 288k nominal switches the top band to FhG-IIS xHE
    // (44.1 kHz native output); 256-287k keeps the Opus top band.
    let hf_codec = if mid_codec == sena_core::MidCodec::Xhe {
        sena_core::xfi_hf_codec(kbps)
    } else {
        sena_core::HfCodec::Opus
    };
    if three_track {
        if xfi {
            let top_desc = match hf_codec {
                sena_core::HfCodec::Xhe => "FhG-IIS xHE top".to_string(),
                sena_core::HfCodec::Opus => format!(
                    "Opus top ({})",
                    if use_senav { "opusenc-senav" } else { "opusenc (original)" }
                ),
            };
            eprintln!(
                "senaenc: profile @{}  total {}k -> xHE-AAC {}k (deducted) + FhG-IIS xHE mid {}k CBR@44.1kHz (600 Hz..12 kHz) + {} {}k (12 kHz+) [{}]",
                profile.crossover_hz(),
                kbps,
                xhe_k,
                mid_k,
                top_desc,
                xfi_hf_kbps(kbps),
                three_track_tag(profile, mid_codec, hf_codec),
            );
        } else {
            eprintln!(
                "senaenc: profile @{}  total {}k -> xHE-AAC {}k (deducted) + Opus mid {}k (600 Hz..12 kHz) + Opus top {}k (12 kHz+, {}) [{}]{}",
                profile.crossover_hz(),
                kbps,
                xhe_k,
                mid_k,
                sena_core::hf_track_kbps(kbps),
                if use_senav {
                    "opusenc-senav"
                } else {
                    "opusenc (original)"
                },
                three_track_tag(profile, mid_codec, hf_codec),
                if let Some(t) = topband_kbps {
                    format!("  topband-stereo={t}")
                } else {
                    String::new()
                }
            );
        }
    } else {
        eprintln!(
            "senaenc: profile @{}  total {}k -> xHE-AAC {}k (deducted) + Opus {}k ({}){}",
            profile.crossover_hz(),
            kbps,
            xhe_k,
            mid_k,
            if use_senav {
                "opusenc-senav"
            } else {
                "opusenc (original)"
            },
            if let Some(t) = topband_kbps {
                format!("  topband-stereo={t}")
            } else {
                String::new()
            }
        );
    }

    // locate binaries: same dir as the executable first (with Windows
    // .exe/PATHEXT variants), then PATH
    let finder = ToolFinder::new();
    let (exhale, _) = finder.find("exhale").unwrap_or_else(|| {
        eprintln!("error: exhale (>= 1.2.2) not found next to senaenc or on PATH");
        std::process::exit(4);
    });
    let exhale = exhale.to_string_lossy().into_owned();
    let opus_name = if use_senav {
        "opusenc-senav"
    } else {
        "opusenc"
    };
    let (opusenc, _) = finder.find(opus_name).unwrap_or_else(|| {
        eprintln!("error: {opus_name} not found next to senaenc or on PATH");
        std::process::exit(4);
    });
    let opusenc = opusenc.to_string_lossy().into_owned();
    let ezcd_cli: Option<String> = if xfi {
        let found = finder
            .find("ezcd-cli")
            .or_else(|| finder.find("ezcd-cli-portable"))
            .unwrap_or_else(|| {
                eprintln!(
                    "error: ezcd-cli not found next to senaenc or on PATH \
                     (tried ezcd-cli, ezcd-cli-portable)"
                );
                eprintln!("       the xfi mid layer (Fraunhofer IIS xHE-AAC) cannot be encoded without it.");
                std::process::exit(4);
            });
        Some(found.0.to_string_lossy().into_owned())
    } else {
        None
    };

    // Workdir bases: explicit --tmpdir or the default tmpfs preference,
    // with the system temp dir as the fallback volume. A unique
    // senaenc-<pid>-<rand> subdir is created inside after the free-space
    // preflight (see sena_enc::workdir).
    let default_base = || {
        let shm = std::path::Path::new("/dev/shm");
        #[cfg(unix)]
        let shm_ok = shm.is_dir();
        #[cfg(not(unix))]
        let shm_ok = {
            let _ = &shm;
            false
        };
        if shm_ok {
            shm.to_path_buf()
        } else {
            std::env::temp_dir()
        }
    };
    let (wd_primary, wd_fallback) = match &tmpdir {
        Some(d) => (d.clone(), Some(default_base())),
        None => {
            let b = default_base();
            let t = std::env::temp_dir();
            if b == t { (b, None) } else { (b, Some(t)) }
        }
    };
    // Reap leftovers from unperceivable exits (SIGKILL/TerminateProcess);
    // everything perceivable is covered by the guard + signal handlers.
    sena_enc::workdir::sweep_stale(&wd_primary);
    if let Some(fb) = &wd_fallback {
        sena_enc::workdir::sweep_stale(fb);
    }

    // Exit cleanup: the guard covers normal/error/panic paths; the signal
    // handler (a normal thread context via ctrlc) covers Ctrl+C/SIGTERM.
    let cleanup = sena_enc::workdir::Cleanup::new();
    struct WorkdirGuard {
        cleanup: sena_enc::workdir::Cleanup,
        armed: bool,
    }
    impl Drop for WorkdirGuard {
        fn drop(&mut self) {
            if self.armed {
                self.cleanup.clean_now();
            }
        }
    }
    let mut guard = WorkdirGuard { cleanup: cleanup.clone(), armed: true };
    {
        let c = cleanup.clone();
        let _ = ctrlc::set_handler(move || {
            c.clean_now();
            std::process::exit(130);
        });
    }

    let cfg = EncoderConfig {
        profile,
        total_kbps: kbps,
        use_senav,
        three_track,
        mid_codec,
        hf_codec,
        topband_kbps,
        exhale: &exhale,
        opusenc: &opusenc,
        ezcd_cli: ezcd_cli.as_deref(),
        workdir: &wd_primary,
        workdir_fallback: wd_fallback.as_deref(),
        cleanup: Some(cleanup.clone()),
        hf_tilt_pct,
    };
    // Streaming encode: the input is consumed as the DSP makes progress, so
    // feeding hosts (foobar2000 converter) see the real pipeline tempo.
    let result = if stdin_input {
        let mut stdin = std::io::stdin().lock();
        sena_enc::encode_stream(&cfg, &mut stdin, &output)
    } else {
        sena_enc::encode(&cfg, &input, &output)
    };
    let outcome = match result {
        Ok(o) => o,
        Err(e) => {
            eprintln!("error: {e}");
            // process::exit runs no destructors: clean the workdir first.
            cleanup.clean_now();
            std::process::exit(1);
        }
    };
    if keep_workdir {
        eprintln!("senaenc: workdir kept at {}", outcome.workdir.display());
        if let Some(d) = &outcome.spill_dir {
            eprintln!("senaenc: LF spool relocated to {}", d.display());
        }
        guard.armed = false;
    }
    eprintln!("senaenc: wrote {}", output.display());
}
