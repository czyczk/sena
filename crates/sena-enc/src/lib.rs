//! sena-enc: pipeline orchestration (WAV IO, subprocess drivers, packet extraction).

use sena_core::{Profile, PRE_GAIN, SAMPLE_RATE};
use sena_dsp::split;
use sena_mux::mka::{self, Frame, Track};
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub mod ogg;
pub mod wav;

/// Errors surfaced to the CLI.
#[derive(Debug)]
pub enum Error {
    Io(std::io::Error),
    Subprocess(String),
    Format(String),
}
impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        Error::Io(e)
    }
}
impl std::fmt::Display for Error {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Error::Io(e) => write!(f, "io: {e}"),
            Error::Subprocess(s) => write!(f, "subprocess: {s}"),
            Error::Format(s) => write!(f, "format: {s}"),
        }
    }
}

pub struct EncoderConfig<'a> {
    pub profile: Profile,
    pub total_kbps: u32,
    pub use_senav: bool,
    pub exhale: &'a str,
    pub opusenc: &'a str,
    pub workdir: &'a Path,
}

pub struct Encoded<'a> {
    pub hf_packets: Vec<Vec<u8>>,
    pub hf_head: &'a [u8],
    pub hf_preskip: u16,
    pub lf_aus: Vec<Vec<u8>>,
    pub lf_asc: Vec<u8>,
    pub lf_frame_ns: u64,
    pub hf_frame_ns: u64,
}

fn run(cmd: &mut Command, what: &str) -> Result<(), Error> {
    let out = cmd.output().map_err(Error::Io)?;
    if !out.status.success() {
        return Err(Error::Subprocess(format!(
            "{what} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        )));
    }
    Ok(())
}

fn check_version(path: &str, marker: Option<&str>, what: &str) -> Result<(), Error> {
    let out = Command::new(path).arg("--version").output().map_err(Error::Io)?;
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

fn check_exhale(path: &str) -> Result<(), Error> {
    let out = Command::new(path).output().map_err(Error::Io)?;
    let text = String::from_utf8_lossy(&out.stdout);
    // exhale prints its banner on stderr/stdout when run without args
    let all = format!("{}{}", text, String::from_utf8_lossy(&out.stderr));
    if !all.contains("exhale") {
        return Err(Error::Subprocess(format!("cannot run exhale at {path}")));
    }
    // version check: banner contains "version 1.2.2" or later
    let ver_ok = all
        .split_whitespace()
        .any(|w| w.starts_with("version") || w.starts_with("1."));
    if !ver_ok {
        return Err(Error::Subprocess("cannot determine exhale version".into()));
    }
    Ok(())
}

pub fn encode(cfg: &EncoderConfig, input: &Path, output: &Path) -> Result<(), Error> {
    let bytes = std::fs::read(input)?;
    encode_bytes(cfg, &bytes, output)
}

pub fn encode_bytes(cfg: &EncoderConfig, input: &[u8], output: &Path) -> Result<(), Error> {
    // --- read input WAV (any sample rate; stdin-safe via encode_bytes) ---
    let (x, ch, in_rate) = wav::read_f64_bytes(input)?;
    if ch != 2 {
        return Err(Error::Format(format!("expected stereo input, got {ch} ch")));
    }
    // --- normalize the input rate to 48 kHz (zero-phase rational resampler) ---
    let x48: Vec<f64> = if in_rate == SAMPLE_RATE {
        x
    } else {
        sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch)
    };
    let x = x48;
    let n = x.len() / 2;

    // --- split & pad ---
    let (low, high) = split(&x, 2, cfg.profile.crossover_hz());
    let pad = |b: &[f64]| -> Vec<f64> { b.iter().map(|v| v * PRE_GAIN).collect() };
    let low_p = pad(&low);
    let high_p = pad(&high);

    // --- downsample low band 48k -> lf rate (zero-phase rational resampler) ---
    let low16 = sena_dsp::Resampler::new(SAMPLE_RATE, cfg.profile.lf_rate()).process(&low_p, 2);

    // --- write temp WAVs ---
    let wd = cfg.workdir;
    std::fs::create_dir_all(wd)?;
    let lf_wav = wd.join("lf.wav");
    wav::write_s16(&lf_wav, &low16, cfg.profile.lf_rate())?;
    let hf_wav = wd.join("hf.wav");
    wav::write_f32(&hf_wav, &high_p, SAMPLE_RATE)?;

    // --- encode ---
    let lf_m4a = wd.join("lf.m4a");
    check_exhale(cfg.exhale)?;
    let mut ex = Command::new(cfg.exhale);
    ex.arg(cfg.profile.xhe_preset().to_string())
        .arg(&lf_wav)
        .arg(&lf_m4a);
    run(&mut ex, "exhale")?;

    if cfg.use_senav {
        check_version(cfg.opusenc, Some("Opus SenaV"), "opusenc-senav")?;
    } else {
        check_version(cfg.opusenc, None, "opusenc")?;
    }
    let hf_ogg = wd.join("hf.opus");
    let (_, opus_kbps) = sena_core::account(cfg.total_kbps, cfg.profile)
        .ok_or_else(|| Error::Format("total bitrate below Sena minimum".into()))?;
    let mut op = Command::new(cfg.opusenc);
    op.arg("--quiet")
        .arg("--bitrate")
        .arg(opus_kbps.to_string())
        .arg("--vbr")
        .arg(&hf_wav)
        .arg(&hf_ogg);
    if cfg.use_senav {
        for (k, v) in [
            ("AUDIFF_ADAPT_INTENSITY", "1000"),
            ("AUDIFF_VBR_TBOOST", "25"),
            ("AUDIFF_TBOOST_SUSTAIN_GATE", "1"),
            ("AUDIFF_TBOOST_SUSTAIN_RATIO", "40"),
            ("AUDIFF_VBR_TDECAY", "0"),
        ] {
            op.env(k, v);
        }
    }
    run(&mut op, "opusenc")?;

    // --- extract packets ---
    let ogg_bytes = std::fs::read(&hf_ogg)?;
    let (hf_head, preskip, hf_packets) = ogg::extract_opus(&ogg_bytes)?;
    let (lf_asc, lf_aus) = extract_m4a(&lf_m4a)?;
    let lf_stream_rate = m4a_stream_rate(&lf_m4a)?;

    // --- frame timings ---
    let hf_frame_ns = 20_000_000u64; // 20 ms Opus frames
    // LF AU duration: one 1024-sample core frame at the actual stream rate
    // (64 ms at 16 kHz, 32 ms at 32 kHz).
    let lf_frame_ns = (sena_core::XHE_WARMUP_CORE as u64) * 1_000_000_000 / lf_stream_rate as u64;

    // --- mux ---
    let tracks = vec![
        Track {
            codec_id: "A_OPUS".into(),
            codec_private: hf_head.to_vec(),
            sample_rate: SAMPLE_RATE as f64,
            channels: 2,
            codec_delay_ns: (preskip as u64) * 1_000_000_000 / SAMPLE_RATE as u64,
            bit_depth: Some(32),
        },
        Track {
            codec_id: "A_SENALF".into(),
            codec_private: lf_asc.clone(),
            sample_rate: lf_stream_rate as f64,
            channels: 2,
            codec_delay_ns: (sena_core::XHE_WARMUP_CORE as u64) * 1_000_000_000 / lf_stream_rate as u64,
            bit_depth: None,
        },
    ];
    let mut frames = vec![];
    for (i, p) in hf_packets.iter().enumerate() {
        frames.push(Frame {
            track: 0,
            t_ns: i as u64 * hf_frame_ns,
            data: p.clone(),
        });
    }
    let mut lf_t = 0u64;
    for au in &lf_aus {
        frames.push(Frame {
            track: 1,
            t_ns: lf_t,
            data: au.clone(),
        });
        lf_t += lf_frame_ns;
    }
    let profile_tag = cfg.profile.crossover_hz().to_string();
    let version_tag = "1".to_string();
    let playable_tag = n.to_string(); // input length in 48 kHz stereo frames
    let tags: Vec<(&str, &str)> = vec![
        ("SENA_PROFILE", &profile_tag),
        ("SENA_VERSION", &version_tag),
        ("SENA_PLAYABLE_SAMPLES", &playable_tag),
    ];
    let playable_ns = (n as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
    let mka_bytes = mka::write_mka(&tracks, frames, &tags, 1000, playable_ns);
    std::fs::write(output, mka_bytes)?;
    Ok(())
}

/// Read the stream's actual sample rate from mdhd timescale.
pub fn m4a_stream_rate(path: &Path) -> Result<u32, Error> {
    let f = std::fs::read(path)?;
    for (tag, s, e) in boxes(&f, 0, f.len()) {
        if tag != b"moov" {
            continue;
        }
        for (tag2, s2, e2) in boxes(&f, s, e) {
            if tag2 != b"trak" {
                continue;
            }
            for (tag3, s3, e3) in boxes(&f, s2, e2) {
                if tag3 == b"mdia" {
                    for (tag4, s4, _e4) in boxes(&f, s3, e3) {
                        if tag4 == b"mdhd" {
                            let ver = f[s4];
                            let off = if ver == 1 { 16 } else { 12 };
                            return Ok(u32::from_be_bytes([
                                f[s4 + off],
                                f[s4 + off + 1],
                                f[s4 + off + 2],
                                f[s4 + off + 3],
                            ]));
                        }
                    }
                }
            }
        }
    }
    Err(Error::Format("mdhd not found".into()))
}

/// Extract ASC (esds desc 0x05) and raw AUs (mdat, stsz order) from an exhale m4a.
pub fn extract_m4a(path: &Path) -> Result<(Vec<u8>, Vec<Vec<u8>>), Error> {
    let f = std::fs::read(path)?;
    let asc = find_asc(&f)?;
    let aus = find_aus(&f)?;
    Ok((asc, aus))
}

fn boxes<'a>(f: &'a [u8], start: usize, end: usize) -> Vec<(&'a [u8], usize, usize)> {
    let mut out = vec![];
    let mut p = start;
    while p + 8 <= end {
        let sz = u32::from_be_bytes([f[p], f[p + 1], f[p + 2], f[p + 3]]) as usize;
        let tag = &f[p + 4..p + 8];
        if sz < 8 || p + sz > end {
            break;
        }
        out.push((tag, p + 8, p + sz));
        p += sz;
    }
    out
}

fn find_asc(f: &[u8]) -> Result<Vec<u8>, Error> {
    // walk: moov > trak > mdia > minf > stbl > stsd > mp4a > esds > desc 0x05
    for (tag, s, e) in boxes(f, 0, f.len()) {
        if tag != b"moov" {
            continue;
        }
        for (tag2, s2, e2) in boxes(f, s, e) {
            if tag2 != b"trak" {
                continue;
            }
            for (tag3, s3, e3) in boxes(f, s2, e2) {
                if tag3 != b"mdia" {
                    continue;
                }
                for (tag4, s4, e4) in boxes(f, s3, e3) {
                    if tag4 != b"minf" {
                        continue;
                    }
                    for (tag5, s5, e5) in boxes(f, s4, e4) {
                        if tag5 != b"stbl" {
                            continue;
                        }
                        for (tag6, s6, e6) in boxes(f, s5, e5) {
                            if tag6 != b"stsd" {
                                continue;
                            }
                            // entry at s6+4 (version/flags) + 4 (count)
                            let (_, es, ee) = boxes(f, s6 + 8, e6)[0];
                            // mp4a entry: 8 + 28 audio header
                            for (tag7, s7, e7) in boxes(f, es + 28, ee) {
                                if tag7 == b"esds" {
                                    if let Some(asc) = walk_descs(f, s7 + 4, e7) {
                                        return Ok(asc);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Err(Error::Format("ASC not found in m4a".into()))
}

/// Recursive MPEG-4 descriptor walker: find the 0x05 DecSpecificInfo payload.
fn walk_descs(f: &[u8], start: usize, end: usize) -> Option<Vec<u8>> {
    let mut p = start;
    while p + 2 <= end {
        let t = f[p];
        p += 1;
        let mut len = 0u32;
        for _ in 0..4 {
            if p >= end {
                return None;
            }
            let b = f[p];
            p += 1;
            len = (len << 7) | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                break;
            }
        }
        let c_end = p + len as usize;
        if c_end > end {
            return None;
        }
        match t {
            0x05 => return Some(f[p..c_end].to_vec()),
            0x03 => {
                // ES_ID(2) + flags(1), then children
                if let Some(v) = walk_descs(f, p + 3, c_end) {
                    return Some(v);
                }
            }
            0x04 => {
                // objectType(1) streamType(1) bufferSize(3) maxRate(4) avgRate(4)
                if let Some(v) = walk_descs(f, p + 13, c_end) {
                    return Some(v);
                }
            }
            _ => {}
        }
        p = c_end;
    }
    None
}

fn find_aus(f: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut sizes: Option<Vec<u32>> = None;
    let mut mdat_start = None;
    let mut first_chunk_offset = None;
    for (tag, s, e) in boxes(f, 0, f.len()) {
        if tag == b"mdat" {
            mdat_start = Some(s);
        }
        if tag == b"moov" {
            for (t2, s2, e2) in boxes(f, s, e) {
                if t2 != b"trak" {
                    continue;
                }
                for (t3, s3, e3) in boxes(f, s2, e2) {
                    if t3 != b"mdia" {
                        continue;
                    }
                    for (t4, s4, e4) in boxes(f, s3, e3) {
                        if t4 != b"minf" {
                            continue;
                        }
                        for (t5, s5, e5) in boxes(f, s4, e4) {
                            if t5 != b"stbl" {
                                continue;
                            }
                            for (t6, s6, _e6) in boxes(f, s5, e5) {
                                if t6 == b"stsz" {
                                    let count = u32::from_be_bytes([f[s6 + 8], f[s6 + 9], f[s6 + 10], f[s6 + 11]]) as usize;
                                    let mut v = Vec::with_capacity(count);
                                    for i in 0..count {
                                        v.push(u32::from_be_bytes([
                                            f[s6 + 12 + 4 * i],
                                            f[s6 + 13 + 4 * i],
                                            f[s6 + 14 + 4 * i],
                                            f[s6 + 15 + 4 * i],
                                        ]));
                                    }
                                    sizes = Some(v);
                                }
                                if t6 == b"stco" {
                                    // stco payload: version/flags(4) + count(4)
                                    // + chunk offsets; the first offset is the
                                    // authoritative start of AU #0. exhale's
                                    // mdat preamble length can vary, so the
                                    // old hard-coded `mdat + 2` is unreliable.
                                    let count = u32::from_be_bytes([f[s6 + 4], f[s6 + 5], f[s6 + 6], f[s6 + 7]]) as usize;
                                    if count > 0 {
                                        first_chunk_offset = Some(u32::from_be_bytes([
                                            f[s6 + 8],
                                            f[s6 + 9],
                                            f[s6 + 10],
                                            f[s6 + 11],
                                        ]) as usize);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let sizes = sizes.ok_or_else(|| Error::Format("stsz not found".into()))?;
    let start = mdat_start.ok_or_else(|| Error::Format("mdat not found".into()))?;
    // exhale writes "unused but informative" bytes at the start of the mdat
    // payload before the first AU. Their length can vary, so trust the MP4
    // stco chunk table; fall back to the historical two-byte preamble only
    // when stco is absent.
    let mut off = first_chunk_offset.unwrap_or(start + 2);
    let mut aus = vec![];
    for sz in sizes {
        let sz = sz as usize;
        if off + sz > f.len() {
            return Err(Error::Format("AU out of range".into()));
        }
        aus.push(f[off..off + sz].to_vec());
        off += sz;
    }
    Ok(aus)
}

#[allow(dead_code)]
fn _io_write(v: &[u8]) -> std::io::Result<()> {
    let s = Stdio::piped();
    let _ = s;
    std::io::stdout().write_all(v)
}
