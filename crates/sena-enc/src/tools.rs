//! External toolchain probing: version checks for exhale / opusenc(-senav)
//! / ezcd-cli, plus the SenaV tuning environment shared by every
//! opusenc-senav invocation.

use std::path::Path;
use std::process::Command;

use crate::Error;

/// Run `tool --version`-style probe and capture the output.
fn tool_output(path: &str, args: &[&str], what: &str) -> Result<std::process::Output, Error> {
    Command::new(path)
        .args(args)
        .output()
        .map_err(|e| Error::Subprocess(format!("cannot run {what} at {path}: {e}")))
}

/// Minimum accepted SenaV milestone in the opusenc-senav version string.
pub const SENAV_MIN_MILESTONE: u32 = 3;

/// SenaV build check: the `--version` output must carry a
/// `Opus SenaV m<N>` marker with N >= [`SENAV_MIN_MILESTONE`] (older builds
/// lack the knob semantics Sena relies on). The trailing `@<commit>` id is
/// informational only and never enforced.
pub fn check_senav_version(path: &str) -> Result<(), Error> {
    let out = tool_output(path, &["--version"], "opusenc-senav")?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() && text.is_empty() {
        return Err(Error::Subprocess(format!("opusenc-senav --version failed at {path}")));
    }
    let Some(pos) = text.find("Opus SenaV m") else {
        return Err(Error::Subprocess(format!(
            "opusenc-senav lacks a \"Opus SenaV m<N>\" marker (need m{SENAV_MIN_MILESTONE}+): {text}"
        )));
    };
    let rest = &text[pos + "Opus SenaV m".len()..];
    let digits: String = rest.chars().take_while(|c| c.is_ascii_digit()).collect();
    let n: u32 = digits.parse().unwrap_or(0);
    if n < SENAV_MIN_MILESTONE {
        return Err(Error::Subprocess(format!(
            "opusenc-senav is SenaV m{n}, need m{SENAV_MIN_MILESTONE} or newer: {text}"
        )));
    }
    Ok(())
}

/// Generic `--version` probe: succeeds when the tool runs and its output
/// carries `marker` (if given).
pub fn check_version(path: &str, marker: Option<&str>, what: &str) -> Result<(), Error> {
    let out = tool_output(path, &["--version"], what)?;
    let text = String::from_utf8_lossy(&out.stdout);
    if !out.status.success() && text.is_empty() {
        return Err(Error::Subprocess(format!("{what} --version failed")));
    }
    if let Some(m) = marker {
        if !text.contains(m) {
            return Err(Error::Subprocess(format!(
                "{what} lacks marker {m:?} in version output: {text}"
            )));
        }
    }
    Ok(())
}

/// First non-empty line of `tool --version` (for display in `senaenc doctor`).
pub fn opusenc_version(path: &str) -> Result<String, Error> {
    let out = tool_output(path, &["--version"], "opusenc")?;
    for text in [
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr),
    ] {
        for line in text.lines() {
            let l = line.trim();
            if !l.is_empty() {
                return Ok(l.to_string());
            }
        }
    }
    Err(Error::Subprocess(format!(
        "opusenc --version produced no output"
    )))
}

/// Leading dotted-numeric prefix of a version token, e.g. `1.2.2RC` -> `1.2.2`.
fn version_token(tok: &str) -> Option<String> {
    let mut out = String::new();
    let mut dots = 0u32;
    for c in tok.chars() {
        if c.is_ascii_digit() {
            out.push(c);
        } else if c == '.' && !out.is_empty() && dots < 3 {
            out.push('.');
            dots += 1;
        } else {
            break;
        }
    }
    while out.ends_with('.') {
        out.pop();
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Parse an exhale version out of its output. Handles both forms:
/// `-V`:  "exhale 1.2.2 (x64, ...)"
/// banner (argument-less run): "| version 1.2.2 (x64, built on ...) |"
pub fn parse_exhale_version(text: &str) -> Option<String> {
    let mut it = text.split_whitespace();
    while let Some(tok) = it.next() {
        let intro = tok.eq_ignore_ascii_case("exhale")
            || tok.eq_ignore_ascii_case("version")
            || tok.starts_with("version");
        if !intro {
            continue;
        }
        if let Some(v) = it.next().and_then(version_token) {
            return Some(v);
        }
    }
    None
}

/// Dotted version comparison (`1.2.2` >= `1.2.2`, `1.10` > `1.9`).
pub fn version_ge(a: &str, b: &str) -> bool {
    fn nums(v: &str) -> Vec<u64> {
        v.split('.').map(|p| p.parse().unwrap_or(0)).collect()
    }
    let (a, b) = (nums(a), nums(b));
    for i in 0..a.len().max(b.len()) {
        let x = a.get(i).copied().unwrap_or(0);
        let y = b.get(i).copied().unwrap_or(0);
        if x != y {
            return x > y;
        }
    }
    true
}

/// ezcd-cli invocation: on WSL (Linux host, Windows .exe payload) file path
/// arguments must be translated to Windows paths via wslpath.
pub(crate) fn cli_path(p: &Path, windows_host: bool) -> Result<String, Error> {
    if !windows_host {
        return Ok(p.to_string_lossy().into_owned());
    }
    let out = Command::new("wslpath")
        .arg("-w")
        .arg(p)
        .output()
        .map_err(|e| Error::Subprocess(format!("cannot run wslpath (needed by the WSL ezcd-cli bridge): {e}")))?;
    if !out.status.success() {
        return Err(Error::Subprocess(format!(
            "wslpath -w {} failed",
            p.display()
        )));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

/// Pinned semantics: translation is a WSL-only bridge concern.
#[cfg(test)]
mod windows_paths_tests {
    #[test]
    fn translation_only_on_linux_host_with_exe_payload() {
        #[cfg(target_os = "linux")]
        {
            assert!(super::needs_windows_paths("/tools/ezcd-cli.exe"));
            assert!(super::needs_windows_paths("/tools/EZCD-CLI.EXE"));
            assert!(!super::needs_windows_paths("/usr/bin/ezcd-cli"));
        }
        #[cfg(windows)]
        {
            assert!(!super::needs_windows_paths("C:\\tools\\ezcd-cli.exe"));
            assert!(!super::needs_windows_paths("C:\\tools\\ezcd-cli"));
        }
    }
}

/// True when path arguments handed to this tool need Windows translation:
/// only on a Linux host (WSL) with a Windows `.exe` payload. On native
/// Windows every tool is a Windows program and paths are already native —
/// translating there would (a) be wrong and (b) fail, because wslpath does
/// not exist off WSL.
pub(crate) fn needs_windows_paths(path: &str) -> bool {
    cfg!(target_os = "linux") && path.to_ascii_lowercase().ends_with(".exe")
}

/// Run exhale and return its version. `exhale -V` prints the version and
/// exits 0; older builds fall back to the argument-less banner, which also
/// contains "version x.y.z".
pub fn exhale_version(path: &str) -> Result<String, Error> {
    let all = |out: &std::process::Output| {
        format!(
            "{}{}",
            String::from_utf8_lossy(&out.stdout),
            String::from_utf8_lossy(&out.stderr)
        )
    };
    let out = tool_output(path, &["-V"], "exhale")?;
    if let Some(v) = parse_exhale_version(&all(&out)) {
        return Ok(v);
    }
    let out = tool_output(path, &[], "exhale")?;
    if let Some(v) = parse_exhale_version(&all(&out)) {
        return Ok(v);
    }
    Err(Error::Subprocess(format!(
        "cannot determine exhale version at {path}"
    )))
}

/// Require exhale >= 1.2.2 (the version baseline the encoder needs).
pub fn check_exhale(path: &str) -> Result<(), Error> {
    let v = exhale_version(path)?;
    if version_ge(&v, "1.2.2") {
        Ok(())
    } else {
        Err(Error::Subprocess(format!(
            "exhale {v} at {path} is older than the required 1.2.2"
        )))
    }
}

/// SenaV tuning knobs shared by every opusenc-senav invocation; the
/// topband-stereo floor is armed only for the mid track when requested.
pub(crate) fn apply_senav_env(op: &mut Command, topband_kbps: Option<u32>) {
    for (k, v) in [
        ("AUDIFF_ADAPT_INTENSITY", "1000"),
        ("AUDIFF_VBR_TBOOST", "40"),
        ("AUDIFF_TBOOST_SUSTAIN_GATE", "1"),
        ("AUDIFF_TBOOST_SUSTAIN_RATIO", "80"),
        ("AUDIFF_VBR_TDECAY", "0"),
    ] {
        op.env(k, v);
    }
    if let Some(kbps) = topband_kbps {
        op.env("AUDIFF_TOPBAND_STEREO", kbps.to_string());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhale_version_parsing() {
        // -V output (exit 0)
        assert_eq!(
            parse_exhale_version("exhale 1.2.2 (x64, Unicode, Mar 03 2025)"),
            Some("1.2.2".into())
        );
        // argument-less banner
        assert_eq!(
            parse_exhale_version(
                " | version 1.2.3 (x64, built on Mar 03 2025) - written by C.R.Helmrich |"
            ),
            Some("1.2.3".into())
        );
        // release-candidate suffix
        assert_eq!(
            parse_exhale_version("exhale 1.2.2RC (x64)"),
            Some("1.2.2".into())
        );
        // colored banner (colors stripped, still parseable)
        assert_eq!(
            parse_exhale_version("\x1b[31mexhale\x1b[0m - ecodis ... version 1.2.4 (x64)"),
            Some("1.2.4".into())
        );
        assert_eq!(parse_exhale_version("no version here"), None);
    }

    #[test]
    fn version_comparison() {
        assert!(version_ge("1.2.2", "1.2.2"));
        assert!(version_ge("1.2.3", "1.2.2"));
        assert!(version_ge("2.0", "1.9.9"));
        assert!(version_ge("1.10", "1.9"));
        assert!(!version_ge("1.2.1", "1.2.2"));
        assert!(!version_ge("1.1.9", "1.2.2"));
        assert!(!version_ge("1.2RC", "1.2.2"));
    }
}
