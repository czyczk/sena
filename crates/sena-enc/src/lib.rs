//! sena-enc: pipeline orchestration (WAV IO, subprocess drivers, packet extraction).

use sena_core::{PRE_GAIN, Profile, SAMPLE_RATE};
use sena_mux::mka::{self, Frame, Track};
use sha2::{Digest, Sha256};
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
    /// Three-track layout: split the 600 Hz-high band again at the Opus
    /// b19 edge (15600 Hz) and encode the top band as a second opus track
    /// (`A_OPUSHF`, fixed 64 kbit/s nominal).
    pub three_track: bool,
    /// Value passed through to the opusenc-senav mid encode as
    /// `AUDIFF_TOPBAND_STEREO` (kbps, unmodified). None = knob off.
    pub topband_kbps: Option<u32>,
    pub exhale: &'a str,
    pub opusenc: &'a str,
    pub workdir: &'a Path,
    /// Optional gentle HF tilt on the Opus-bound band, percent of the
    /// reference curve (None/0 = off). In the three-track layout the tilt
    /// applies to the mid band only.
    pub hf_tilt_pct: Option<f64>,
}

/// Streams of one encoded Sena file (codec payloads exactly as muxed).
pub struct Encoded {
    /// A_OPUS track (600 Hz - 15.6 kHz in the three-track layout).
    pub hf_packets: Vec<Vec<u8>>,
    pub hf_head: Vec<u8>,
    pub hf_preskip: u16,
    pub lf_aus: Vec<Vec<u8>>,
    pub lf_asc: Vec<u8>,
    /// A_OPUSHF track (15.6 kHz - Nyquist); empty in the two-track layout.
    pub hf2_packets: Vec<Vec<u8>>,
    pub hf2_head: Vec<u8>,
    pub hf2_preskip: u16,
    pub lf_frame_ns: u64,
    pub hf_frame_ns: u64,
}

/// Run encoder subprocesses concurrently, waiting for all (their
/// stdout/stderr are piped so error text is preserved). Every child is
/// drained on its own thread: waiting one-by-one would let a chatty later
/// process stall on a full pipe while an earlier one is being waited on.
fn run_parallel(cmds: &mut [(&str, &mut Command)]) -> Option<Error> {
    use std::process::Stdio;
    for (_, c) in cmds.iter_mut() {
        c.stdout(Stdio::piped()).stderr(Stdio::piped());
    }
    let spawned: Vec<std::io::Result<(&str, std::process::Child)>> = cmds
        .iter_mut()
        .map(|(what, c)| c.spawn().map(|h| (*what, h)))
        .collect();
    let results: Vec<Result<(&str, std::process::Output), Error>> = std::thread::scope(|s| {
        let handles: Vec<_> = spawned
            .into_iter()
            .map(|r| {
                s.spawn(move || -> Result<(&str, std::process::Output), Error> {
                    let (what, child) = r.map_err(Error::Io)?;
                    let out = child.wait_with_output().map_err(Error::Io)?;
                    Ok((what, out))
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|h| h.join().unwrap_or_else(|e| std::panic::resume_unwind(e)))
            .collect()
    });
    let mut err: Option<Error> = None;
    for r in results {
        match r {
            Ok((what, o)) => {
                if !o.status.success() && err.is_none() {
                    err = Some(Error::Subprocess(format!(
                        "{what} failed: {}",
                        String::from_utf8_lossy(&o.stderr)
                    )));
                }
            }
            Err(e) => {
                if err.is_none() {
                    err = Some(e);
                }
            }
        }
    }
    err
}

pub fn check_version(path: &str, marker: Option<&str>, what: &str) -> Result<(), Error> {
    let out = Command::new(path)
        .arg("--version")
        .output()
        .map_err(Error::Io)?;
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
    let out = Command::new(path)
        .arg("--version")
        .output()
        .map_err(Error::Io)?;
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
    let out = Command::new(path).arg("-V").output().map_err(Error::Io)?;
    if let Some(v) = parse_exhale_version(&all(&out)) {
        return Ok(v);
    }
    let out = Command::new(path).output().map_err(Error::Io)?;
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

pub fn encode(cfg: &EncoderConfig, input: &Path, output: &Path) -> Result<(), Error> {
    let mut f = std::fs::File::open(input)?;
    encode_stream(cfg, &mut f, output)
}

pub fn encode_bytes(cfg: &EncoderConfig, input: &[u8], output: &Path) -> Result<(), Error> {
    let mut bytes = input;
    encode_stream(cfg, &mut bytes, output)
}

/// Audio SHA-256 of the encoded audio elementary streams carried by the
/// container: the Opus stream (OpusHead + Opus audio packets) and the
/// xHE-AAC stream (ASC + raw AUs), in Sena track order. The streams are the
/// exact bytes copied into the muxed file; the hash is length-prefixed and
/// domain-separated so it is deterministic and independent of container
/// layout, timestamps and tags. It intentionally does NOT hash the input
/// PCM or the decoded output.
pub fn encoded_audio_sha256(
    opus_head: &[u8],
    opus_packets: &[Vec<u8>],
    lf_asc: &[u8],
    lf_aus: &[Vec<u8>],
) -> String {
    hash_streams(
        "SENA encoded audio sha256 v1\0",
        &[
            ("A_OPUS", opus_head, opus_packets),
            ("A_SENALF", lf_asc, lf_aus),
        ],
    )
}

/// Three-track variant (`A_OPUSHF` added, v2 domain): the two-track hash
/// bytes stay frozen so existing files keep their recorded value.
pub fn encoded_audio_sha256_3(
    opus_head: &[u8],
    opus_packets: &[Vec<u8>],
    lf_asc: &[u8],
    lf_aus: &[Vec<u8>],
    hf2_head: &[u8],
    hf2_packets: &[Vec<u8>],
) -> String {
    hash_streams(
        "SENA encoded audio sha256 v2\0",
        &[
            ("A_OPUS", opus_head, opus_packets),
            ("A_SENALF", lf_asc, lf_aus),
            ("A_OPUSHF", hf2_head, hf2_packets),
        ],
    )
}

fn hash_streams(domain: &str, streams: &[(&str, &[u8], &[Vec<u8>])]) -> String {
    let mut h = Sha256::new();
    h.update(domain.as_bytes());
    h.update((streams.len() as u32).to_le_bytes()); // stream count
    for (codec_id, private, packets) in streams {
        let id = codec_id.as_bytes();
        h.update((id.len() as u32).to_le_bytes());
        h.update(id);
        h.update((private.len() as u32).to_le_bytes());
        h.update(private);
        h.update((packets.len() as u64).to_le_bytes());
        for p in packets.iter() {
            h.update((p.len() as u32).to_le_bytes());
            h.update(p);
        }
    }
    let out = h.finalize();
    let mut hex = String::with_capacity(64);
    for b in out {
        use std::fmt::Write;
        let _ = write!(hex, "{b:02x}");
    }
    hex
}

/// Full encode pipeline over a streaming input (stdin-safe).
///
/// The WAV is parsed incrementally and pushed through the streaming DSP
/// (rate normalize -> crossover split -> LF downsample) chunk by chunk,
/// writing the two codec-input WAVs as the input arrives. This keeps the
/// memory bounded and makes the input consumption rate track the actual
/// work, so a feeding host (foobar2000 converter) drives its progress bar
/// with the real pipeline instead of racing to 100% while senaenc then
/// does all the work.
pub fn encode_stream<R: std::io::Read>(
    cfg: &EncoderConfig,
    input: &mut R,
    output: &Path,
) -> Result<(), Error> {
    use sena_dsp::{CrossoverStream, StreamResampler};

    let wd = cfg.workdir;
    std::fs::create_dir_all(wd)?;
    let mut st = StageTimes::new();

    let mut ws = wav::WavStream::new();
    let mut norm: Option<StreamResampler> = None;
    let mut split_s: Option<CrossoverStream> = None;
    // Second split (three-track layout): 15600 Hz on the 600 Hz-high band.
    let mut split_hf: Option<CrossoverStream> = None;
    let mut tilt: Option<sena_dsp::FirStream> = cfg
        .hf_tilt_pct
        .filter(|&s| s > 0.0)
        .map(|s| sena_dsp::FirStream::new(sena_dsp::tilt_taps(s)));
    let mut lf_rs: Option<StreamResampler> = None;
    let mut lf_wr: Option<wav::WavWriter> = None;
    let mut mid_wr: Option<wav::WavWriter> = None;
    let mut hf_wr: Option<wav::WavWriter> = None;
    let mut ch = 2usize;
    let mut byte_buf = [0u8; 262_144];
    let mut eof = false;

    loop {
        if norm.is_none() {
            if !ws.parsed() {
                if eof {
                    return Err(Error::Format("no WAV header".into()));
                }
                let n = input.read(&mut byte_buf).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                    continue;
                }
                ws.feed(&byte_buf[..n])?;
                continue;
            }
            // header is parsed: initialize the pipeline
            let in_rate = ws.rate().unwrap();
            ch = ws.channels().unwrap();
            if ch != 2 {
                return Err(Error::Format(format!("expected stereo input, got {ch} ch")));
            }
            let lf_rate = cfg.profile.lf_rate();
            norm = Some(StreamResampler::new(in_rate, SAMPLE_RATE));
            split_s = Some(CrossoverStream::new(cfg.profile.crossover_hz()));
            split_hf = cfg
                .three_track
                .then(|| CrossoverStream::new(sena_core::HF_SPLIT_HZ));
            lf_rs = Some(StreamResampler::new(SAMPLE_RATE, lf_rate));
            lf_wr = Some(wav::WavWriter::create(&wd.join("lf.wav"), lf_rate, 16, 2)?);
            if cfg.three_track {
                mid_wr = Some(wav::WavWriter::create(
                    &wd.join("mid.wav"),
                    SAMPLE_RATE,
                    32,
                    2,
                )?);
            }
            hf_wr = Some(wav::WavWriter::create(
                &wd.join("hf.wav"),
                SAMPLE_RATE,
                32,
                2,
            )?);
            continue;
        }

        match ws.take(48_000)? {
            Some(x) => {
                push_chunk(
                    &x,
                    ch,
                    norm.as_mut().unwrap(),
                    split_s.as_mut().unwrap(),
                    split_hf.as_mut(),
                    lf_rs.as_mut().unwrap(),
                    lf_wr.as_mut().unwrap(),
                    mid_wr.as_mut(),
                    hf_wr.as_mut().unwrap(),
                    &mut tilt,
                    &mut st,
                )?;
            }
            None => {
                if eof {
                    break;
                }
                let n = input.read(&mut byte_buf).map_err(Error::Io)?;
                if n == 0 {
                    eof = true;
                } else {
                    ws.feed(&byte_buf[..n])?;
                }
            }
        }
    }
    ws.finish()?;

    // Flush the pipeline latencies (normalize tail -> split tails -> LF tail).
    let mut norm = norm.unwrap();
    let mut split_s = split_s.unwrap();
    let mut split_hf = split_hf.take();
    let mut lf_rs = lf_rs.unwrap();
    let mut lf_wr = lf_wr.unwrap();
    let mut mid_wr = mid_wr.take();
    let mut hf_wr = hf_wr.unwrap();
    let playable = norm.output_frames_total();
    st.begin();
    let n48_tail = norm.finish(ch);
    end_tick(&mut st.t, &mut st.norm);
    st.begin();
    let (low_a, high_a) = split_s.push(&n48_tail, ch);
    let (low_b, high_b) = split_s.finish(ch);
    end_tick(&mut st.t, &mut st.split);
    {
        let low_p: Vec<f64> = low_a
            .iter()
            .chain(low_b.iter())
            .map(|v| v * PRE_GAIN)
            .collect();
        st.begin();
        let lf16 = lf_rs.push(&low_p, ch);
        end_tick(&mut st.t, &mut st.lf);
        lf_wr.push(&lf16)?;
        st.begin();
        let lf16b = lf_rs.finish(ch);
        end_tick(&mut st.t, &mut st.lf);
        lf_wr.push(&lf16b)?;
        let high_in: Vec<f64> = high_a.iter().chain(high_b.iter()).cloned().collect();
        if let Some(split_hf) = split_hf.as_mut() {
            // Three-track tail: split the high band at 15600 Hz, tilt and
            // pad the mid band, pad the top band.
            st.begin();
            let (mid_a, top_a) = split_hf.push(&high_in, ch);
            let (mid_b, top_b) = split_hf.finish(ch);
            end_tick(&mut st.t, &mut st.split);
            let mid_in: Vec<f64> = mid_a.iter().chain(mid_b.iter()).cloned().collect();
            let top_in: Vec<f64> = top_a.iter().chain(top_b.iter()).cloned().collect();
            let mid_f = if let Some(t) = tilt.as_mut() {
                let mut v = t.push(&mid_in, ch);
                v.extend(t.finish(ch));
                v
            } else {
                mid_in
            };
            let mid_p: Vec<f64> = mid_f.iter().map(|v| v * PRE_GAIN).collect();
            let top_p: Vec<f64> = top_in.iter().map(|v| v * PRE_GAIN).collect();
            st.begin();
            mid_wr.as_mut().unwrap().push(&mid_p)?;
            hf_wr.push(&top_p)?;
            end_tick(&mut st.t, &mut st.write);
        } else {
            let high_f = if let Some(t) = tilt.as_mut() {
                let mut v = t.push(&high_in, ch);
                v.extend(t.finish(ch));
                v
            } else {
                high_in
            };
            let high_p: Vec<f64> = high_f.iter().map(|v| v * PRE_GAIN).collect();
            st.begin();
            hf_wr.push(&high_p)?;
            end_tick(&mut st.t, &mut st.write);
        }
    }
    lf_wr.finish()?;
    if let Some(mw) = mid_wr {
        mw.finish()?;
    }
    hf_wr.finish()?;

    let result = run_codecs(cfg, wd, playable, output, &mut st);
    if result.is_ok() {
        st.report(playable);
    }
    result
}

/// Env-gated stage timing for pipeline analysis (SENAENC_TIME=1 prints the
/// per-stage CPU/wall breakdown at the end of an encode).
#[derive(Default)]
pub struct StageTimes {
    on: bool,
    t: Option<std::time::Instant>,
    pub norm: f64,   // input rate -> 48k normalization
    pub split: f64,  // crossover split
    pub lf: f64,     // LF downsample 48k -> lf rate
    pub write: f64,  // band WAV IO (16/32-bit conversion + file write)
    pub check: f64,  // tool version checks
    pub exhale: f64, // xHE-AAC encode
    pub opus: f64,   // Opus encode (mid track; three-track layouts attribute
    // the shared codec wall time to both opus accumulators)
    pub opus_hf: f64, // Opus encode (A_OPUSHF top track, three-track only)
    pub mux: f64,     // packet extraction + container mux
}

impl StageTimes {
    pub fn new() -> Self {
        Self {
            on: std::env::var_os("SENAENC_TIME").is_some(),
            ..Default::default()
        }
    }

    pub fn begin(&mut self) {
        if self.on {
            self.t = Some(std::time::Instant::now());
        }
    }

    pub fn consume_tick(&mut self) {
        self.t = None;
    }

    pub fn tick(&self) -> &Option<std::time::Instant> {
        &self.t
    }

    pub fn report(&self, playable: usize) {
        if !self.on {
            return;
        }
        let total = self.norm
            + self.split
            + self.lf
            + self.write
            + self.check
            + self.exhale
            + self.opus
            + self.opus_hf
            + self.mux;
        eprintln!(
            "SENAENC_TIME: playable={playable} frames\n\
             \x20 normalize {:.3}s  split {:.3}s  lf-down {:.3}s  write {:.3}s\n\
             \x20 check {:.3}s  exhale {:.3}s  opus {:.3}s  opus-hf {:.3}s  mux {:.3}s\n\
             \x20 summed {:.3}s",
            self.norm,
            self.split,
            self.lf,
            self.write,
            self.check,
            self.exhale,
            self.opus,
            self.opus_hf,
            self.mux,
            total
        );
    }
}

/// Fold a pending begin() tick into an accumulator. Takes the two disjoint
/// fields separately so call sites can borrow them without aliasing.
fn end_tick(tick: &mut Option<std::time::Instant>, acc: &mut f64) {
    if let Some(t) = tick.take() {
        *acc += t.elapsed().as_secs_f64();
    }
}

/// One chunk through normalize -> split -> pad -> LF downsample -> writers.
/// In the three-track layout the LF branch (downsample + LF write) and the
/// HF branch (15600 Hz split + tilt + mid/top writes) are independent and
/// run concurrently; their stage timers overlap, so the SENAENC_TIME sum
/// can exceed the wall time.
#[allow(clippy::too_many_arguments)]
fn push_chunk(
    x: &[f64],
    ch: usize,
    norm: &mut sena_dsp::StreamResampler,
    split_s: &mut sena_dsp::CrossoverStream,
    split_hf: Option<&mut sena_dsp::CrossoverStream>,
    lf_rs: &mut sena_dsp::StreamResampler,
    lf_wr: &mut wav::WavWriter,
    mut mid_wr: Option<&mut wav::WavWriter>,
    hf_wr: &mut wav::WavWriter,
    tilt: &mut Option<sena_dsp::FirStream>,
    st: &mut StageTimes,
) -> Result<(), Error> {
    st.begin();
    let n48 = norm.push(x, ch);
    end_tick(&mut st.t, &mut st.norm);
    st.begin();
    let (low, high) = split_s.push(&n48, ch);
    end_tick(&mut st.t, &mut st.split);
    if let Some(split_hf) = split_hf {
        // Three-track: the two band branches are independent; run them
        // concurrently so the p600 LF downsample overlaps the second
        // crossover and the mid/top writes.
        let mut a_time = (0.0f64, 0.0f64); // (lf, write)
        let mut b_time = (0.0f64, 0.0f64); // (split, write)
        let (ra, rb) = std::thread::scope(|s| {
            let ha = s.spawn(|| lf_branch(&low, ch, lf_rs, lf_wr, &mut a_time));
            let hb = s.spawn(|| {
                hf_branch3(
                    &high,
                    ch,
                    split_hf,
                    tilt,
                    mid_wr.as_mut().unwrap(),
                    hf_wr,
                    &mut b_time,
                )
            });
            (
                ha.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
                hb.join().unwrap_or_else(|e| std::panic::resume_unwind(e)),
            )
        });
        st.lf += a_time.0;
        st.write += a_time.1 + b_time.1;
        st.split += b_time.0;
        ra?;
        rb?;
        return Ok(());
    }
    if !low.is_empty() {
        let low_p: Vec<f64> = low.iter().map(|v| v * PRE_GAIN).collect();
        st.begin();
        let lf16 = lf_rs.push(&low_p, ch);
        end_tick(&mut st.t, &mut st.lf);
        st.begin();
        lf_wr.push(&lf16)?;
        end_tick(&mut st.t, &mut st.write);
    }
    let high = if let Some(t) = tilt.as_mut() {
        t.push(&high, ch)
    } else {
        high
    };
    if !high.is_empty() {
        let high_p: Vec<f64> = high.iter().map(|v| v * PRE_GAIN).collect();
        st.begin();
        hf_wr.push(&high_p)?;
        end_tick(&mut st.t, &mut st.write);
    }
    Ok(())
}

/// LF branch: PRE_GAIN -> LF downsample -> s16 WAV write. Returns its
/// (lf, write) elapsed times through `t`.
fn lf_branch(
    low: &[f64],
    ch: usize,
    lf_rs: &mut sena_dsp::StreamResampler,
    lf_wr: &mut wav::WavWriter,
    t: &mut (f64, f64),
) -> Result<(), Error> {
    if low.is_empty() {
        return Ok(());
    }
    let low_p: Vec<f64> = low.iter().map(|v| v * PRE_GAIN).collect();
    let t0 = std::time::Instant::now();
    let lf16 = lf_rs.push(&low_p, ch);
    t.0 += t0.elapsed().as_secs_f64();
    let t1 = std::time::Instant::now();
    let r = lf_wr.push(&lf16);
    t.1 += t1.elapsed().as_secs_f64();
    r
}

/// Three-track HF branch: 15600 Hz split (the tilt, if any, shapes the mid
/// band only) -> PRE_GAIN -> mid/top WAV writes. Returns its (split,
/// write) elapsed times through `t`.
#[allow(clippy::too_many_arguments)]
fn hf_branch3(
    high: &[f64],
    ch: usize,
    split_hf: &mut sena_dsp::CrossoverStream,
    tilt: &mut Option<sena_dsp::FirStream>,
    mid_wr: &mut wav::WavWriter,
    hf_wr: &mut wav::WavWriter,
    t: &mut (f64, f64),
) -> Result<(), Error> {
    let t0 = std::time::Instant::now();
    let (mid, top) = split_hf.push(high, ch);
    t.0 += t0.elapsed().as_secs_f64();
    let mut err: Option<Error> = None;
    if !mid.is_empty() {
        let mid = if let Some(tl) = tilt.as_mut() {
            tl.push(&mid, ch)
        } else {
            mid
        };
        let mid_p: Vec<f64> = mid.iter().map(|v| v * PRE_GAIN).collect();
        let t1 = std::time::Instant::now();
        if let Err(e) = mid_wr.push(&mid_p) {
            err = Some(e);
        }
        t.1 += t1.elapsed().as_secs_f64();
    }
    if err.is_none() && !top.is_empty() {
        let top_p: Vec<f64> = top.iter().map(|v| v * PRE_GAIN).collect();
        let t2 = std::time::Instant::now();
        if let Err(e) = hf_wr.push(&top_p) {
            err = Some(e);
        }
        t.1 += t2.elapsed().as_secs_f64();
    }
    match err {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

/// Encode the temp band WAVs, extract the streams and mux the container.
fn run_codecs(
    cfg: &EncoderConfig,
    wd: &Path,
    playable: usize,
    output: &Path,
    st: &mut StageTimes,
) -> Result<(), Error> {
    let lf_wav = wd.join("lf.wav");
    let mid_wav = wd.join("mid.wav");
    let hf_wav = wd.join("hf.wav");
    st.begin();
    let ck_err: Option<Error> = check_exhale(cfg.exhale)
        .and_then(|_| {
            if cfg.use_senav {
                check_version(cfg.opusenc, Some("Opus SenaV"), "opusenc-senav")
            } else {
                check_version(cfg.opusenc, None, "opusenc")
            }
        })
        .err();
    end_tick(&mut st.t, &mut st.check);
    if let Some(e) = ck_err {
        return Err(e);
    }

    // --- encode ---
    // The codec passes are independent (separate input files, separate
    // output files) and each is single-threaded, so run them concurrently.
    // Two-track: opusenc codes hf.wav (600 Hz..Nyquist). Three-track: it
    // codes mid.wav (600 Hz..15600 Hz) and a second opusenc codes hf.wav
    // (15600 Hz..Nyquist) at a fixed 64k nominal.
    let lf_m4a = wd.join("lf.m4a");
    let hf_ogg = wd.join("hf.opus");
    let mid_ogg = wd.join("mid.opus");
    let mut ex = Command::new(cfg.exhale);
    ex.arg(cfg.profile.xhe_preset().to_string())
        .arg(&lf_wav)
        .arg(&lf_m4a);
    let (_, mid_kbps) = if cfg.three_track {
        let (_, mid, _) = sena_core::account3(cfg.total_kbps, cfg.profile)
            .ok_or_else(|| Error::Format("total bitrate below the three-track minimum".into()))?;
        (0, mid)
    } else {
        sena_core::account(cfg.total_kbps, cfg.profile)
            .ok_or_else(|| Error::Format("total bitrate below Sena minimum".into()))?
    };
    let mut op = Command::new(cfg.opusenc);
    op.arg("--quiet")
        .arg("--bitrate")
        .arg(mid_kbps.to_string())
        .arg("--vbr");
    if cfg.three_track {
        op.arg(&mid_wav).arg(&mid_ogg);
    } else {
        op.arg(&hf_wav).arg(&hf_ogg);
    }
    if cfg.use_senav {
        apply_senav_env(&mut op, cfg.topband_kbps);
    }
    let mut cmds: Vec<(&str, &mut Command)> = vec![("exhale", &mut ex), ("opusenc(mid)", &mut op)];
    // Three-track top-band encode (A_OPUSHF): fixed 64k nominal, the
    // topband-stereo knob is never armed for it.
    let mut op2 = Command::new(cfg.opusenc);
    if cfg.three_track {
        op2.arg("--quiet")
            .arg("--bitrate")
            .arg(sena_core::HF_TRACK_KBPS.to_string())
            .arg("--vbr")
            .arg(&hf_wav)
            .arg(&hf_ogg);
        if cfg.use_senav {
            apply_senav_env(&mut op2, None);
        }
        cmds.push(("opusenc(hf)", &mut op2));
    }
    let t0 = std::time::Instant::now();
    let codec_err = run_parallel(&mut cmds);
    if st.on {
        let d = t0.elapsed().as_secs_f64();
        st.exhale += d;
        st.opus += d;
        if cfg.three_track {
            st.opus_hf += d;
        }
    }
    if let Some(e) = codec_err {
        return Err(e);
    }

    // --- extract packets ---
    st.begin();
    let extracted: Result<_, Error> = (|| {
        let (hf_head, preskip, hf_packets, hf2_head, hf2_preskip, hf2_packets) = if cfg.three_track
        {
            let mid_bytes = std::fs::read(&mid_ogg)?;
            let (mid_head, mid_preskip, mid_packets) = ogg::extract_opus(&mid_bytes)?;
            let top_bytes = std::fs::read(&hf_ogg)?;
            let (top_head, top_preskip, top_packets) = ogg::extract_opus(&top_bytes)?;
            (
                mid_head,
                mid_preskip,
                mid_packets,
                top_head,
                top_preskip,
                top_packets,
            )
        } else {
            let ogg_bytes = std::fs::read(&hf_ogg)?;
            let (head, preskip, packets) = ogg::extract_opus(&ogg_bytes)?;
            (head, preskip, packets, Vec::new(), 0, Vec::new())
        };
        let (lf_asc, lf_aus) = extract_m4a(&lf_m4a)?;
        let lf_stream_rate = m4a_stream_rate(&lf_m4a)?;
        Ok((
            hf_head,
            preskip,
            hf_packets,
            hf2_head,
            hf2_preskip,
            hf2_packets,
            lf_asc,
            lf_aus,
            lf_stream_rate,
        ))
    })();
    end_tick(&mut st.t, &mut st.mux);
    let (
        hf_head,
        preskip,
        hf_packets,
        hf2_head,
        hf2_preskip,
        hf2_packets,
        lf_asc,
        lf_aus,
        lf_stream_rate,
    ) = extracted?;

    // Hash the encoded streams exactly as they are about to be muxed. The
    // container tags (including this hash) are written afterwards, so the
    // hash covers the elementary streams and nothing else. Three-track
    // files use the v2 domain (3 streams); two-track files keep v1.
    let audio_sha256 = if cfg.three_track {
        encoded_audio_sha256_3(
            &hf_head,
            &hf_packets,
            &lf_asc,
            &lf_aus,
            &hf2_head,
            &hf2_packets,
        )
    } else {
        encoded_audio_sha256(&hf_head, &hf_packets, &lf_asc, &lf_aus)
    };

    // --- frame timings / mux: the remaining tail is tiny and measured with
    // the extraction stage (st.mux). ---
    let muxed = (|| {
        let hf_frame_ns = 20_000_000u64; // 20 ms Opus frames
        // LF AU duration: one 1024-sample core frame at the actual stream rate
        // (64 ms at 16 kHz, 32 ms at 32 kHz).
        let lf_frame_ns =
            (sena_core::XHE_WARMUP_CORE as u64) * 1_000_000_000 / lf_stream_rate as u64;

        let mut tracks = vec![
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
                codec_delay_ns: (sena_core::XHE_WARMUP_CORE as u64) * 1_000_000_000
                    / lf_stream_rate as u64,
                bit_depth: None,
            },
        ];
        if cfg.three_track {
            tracks.push(Track {
                codec_id: "A_OPUSHF".into(),
                codec_private: hf2_head.clone(),
                sample_rate: SAMPLE_RATE as f64,
                channels: 2,
                codec_delay_ns: (hf2_preskip as u64) * 1_000_000_000 / SAMPLE_RATE as u64,
                bit_depth: Some(32),
            });
        }
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
        if cfg.three_track {
            for (i, p) in hf2_packets.iter().enumerate() {
                frames.push(Frame {
                    track: 2,
                    t_ns: i as u64 * hf_frame_ns,
                    data: p.clone(),
                });
            }
        }
        let profile_tag = if cfg.three_track {
            sena_core::three_track_tag(cfg.profile)
        } else {
            sena_core::profile_tag(cfg.profile)
        };
        let version_tag = "1".to_string();
        // input length in 48 kHz stereo frames (the normalized source timeline)
        let playable_tag = playable.to_string();
        let tags: Vec<(&str, &str)> = vec![
            ("SENA_PROFILE", &profile_tag),
            ("SENA_VERSION", &version_tag),
            ("SENA_PLAYABLE_SAMPLES", &playable_tag),
            // Audio SHA-256: hash of the encoded Opus + xHE-AAC streams
            // (see `encoded_audio_sha256`), not of the input PCM.
            ("SENA_AUDIO_SHA256", &audio_sha256),
        ];
        let playable_ns = (playable as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
        let mka_bytes = mka::write_mka(&tracks, frames, &tags, 1000, playable_ns);
        std::fs::write(output, mka_bytes)?;
        Ok::<(), Error>(())
    })();
    muxed
}

/// SenaV tuning knobs shared by every opusenc-senav invocation; the
/// topband-stereo floor is armed only for the mid track when requested.
fn apply_senav_env(op: &mut Command, topband_kbps: Option<u32>) {
    for (k, v) in [
        ("AUDIFF_ADAPT_INTENSITY", "1000"),
        ("AUDIFF_VBR_TBOOST", "25"),
        ("AUDIFF_TBOOST_SUSTAIN_GATE", "1"),
        ("AUDIFF_TBOOST_SUSTAIN_RATIO", "40"),
        ("AUDIFF_VBR_TDECAY", "0"),
    ] {
        op.env(k, v);
    }
    if let Some(kbps) = topband_kbps {
        op.env("AUDIFF_TOPBAND_STEREO", kbps.to_string());
    }
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
                                    let count = u32::from_be_bytes([
                                        f[s6 + 8],
                                        f[s6 + 9],
                                        f[s6 + 10],
                                        f[s6 + 11],
                                    ]) as usize;
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
                                    let count = u32::from_be_bytes([
                                        f[s6 + 4],
                                        f[s6 + 5],
                                        f[s6 + 6],
                                        f[s6 + 7],
                                    ]) as usize;
                                    if count > 0 {
                                        first_chunk_offset = Some(u32::from_be_bytes([
                                            f[s6 + 8],
                                            f[s6 + 9],
                                            f[s6 + 10],
                                            f[s6 + 11],
                                        ])
                                            as usize);
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

    #[test]
    fn encoded_audio_sha256_known_vector_and_determinism() {
        // Expected value computed independently in Python with the same
        // length-prefixed stream layout.
        let opus_head = b"opus-head";
        let opus_packets = vec![vec![1, 2, 3], vec![4, 5]];
        let lf_asc = b"asc";
        let lf_aus = vec![vec![6], vec![7, 8]];
        let h = encoded_audio_sha256(opus_head, &opus_packets, lf_asc, &lf_aus);
        assert_eq!(
            h,
            "b20f384c7c0ab805a337ece19a67d14aafab2f3261f9a3832fb9c6dbe8c511ce"
        );
        assert_eq!(
            encoded_audio_sha256(opus_head, &opus_packets, lf_asc, &lf_aus),
            h,
            "must be deterministic"
        );

        // Different encoded payloads must produce different hashes.
        let other = vec![vec![1, 2, 3], vec![4, 6]];
        assert_ne!(encoded_audio_sha256(opus_head, &other, lf_asc, &lf_aus), h);

        // Length-prefix collisions are impossible by construction: a packet
        // split changes the packet count and payload bytes.
        let split = vec![vec![1, 2], vec![3, 4, 5]];
        assert_ne!(encoded_audio_sha256(opus_head, &split, lf_asc, &lf_aus), h);

        // Stream order is fixed and significant (Opus first, then xHE-AAC).
        assert_ne!(
            encoded_audio_sha256(opus_head, &opus_packets, lf_asc, &lf_aus),
            encoded_audio_sha256(lf_asc, &lf_aus, opus_head, &opus_packets)
        );
    }

    fn synth_wav(secs: usize, rate: u32, stream_len: bool) -> Vec<u8> {
        // Stereo s16 PCM: RIFF/WAVE/fmt/data with a streaming data length
        // (0xFFFFFFFF) when `stream_len`, else the real length.
        let frames = secs * rate as usize;
        let data_len = frames * 4usize;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&1u16.to_le_bytes()); // PCM
        w.extend_from_slice(&2u16.to_le_bytes()); // stereo
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * 4).to_le_bytes());
        w.extend_from_slice(&4u16.to_le_bytes());
        w.extend_from_slice(&16u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(
            &(if stream_len {
                0xFFFF_FFFFu32
            } else {
                data_len as u32
            })
            .to_le_bytes(),
        );
        for i in 0..frames {
            let t = i as f64 / rate as f64;
            let l = (2.0 * std::f64::consts::PI * 220.0 * t).sin() as i16;
            let r = (0.6 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16;
            w.extend_from_slice(&l.to_le_bytes());
            w.extend_from_slice(&r.to_le_bytes());
        }
        w
    }

    #[test]
    fn wav_stream_matches_batch_reader() {
        for stream_len in [false, true] {
            let bytes = synth_wav(3, 44100, stream_len);
            let (batch, ch, rate) = wav::read_f64_bytes(&bytes).unwrap();
            // feed in odd-sized chunks
            let mut ws = wav::WavStream::new();
            for chunk in bytes.chunks(997) {
                ws.feed(chunk).unwrap();
            }
            ws.finish().unwrap();
            assert_eq!((ch, rate), (2, 44100));
            let mut got = Vec::new();
            loop {
                match ws.take(4096).unwrap() {
                    Some(x) => got.extend(x),
                    None => break,
                }
            }
            assert_eq!(got.len(), batch.len());
            let maxe = got
                .iter()
                .zip(batch.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(maxe < 1e-9, "stream reader vs batch error {maxe}");
        }
    }

    #[test]
    fn wav_stream_ignores_trailing_bytes_after_declared_data() {
        // foobar-style pipes: the header declares the real data size; bytes
        // after the data chunk (metadata/padding) must never become samples.
        let mut bytes = synth_wav(3, 44100, false);
        let declared = bytes.len() - 44;
        bytes.extend_from_slice(b"LIST\x00\x00\x00\x80"); // fake trailing junk
        bytes.extend_from_slice(&[0u8; 100]);
        let (batch, _, _) = wav::read_f64_bytes(&bytes).unwrap();
        let mut ws = wav::WavStream::new();
        for chunk in bytes.chunks(1800) {
            ws.feed(chunk).unwrap();
        }
        ws.finish().unwrap();
        let mut got = Vec::new();
        loop {
            match ws.take(4096).unwrap() {
                Some(x) => got.extend(x),
                None => break,
            }
        }
        assert_eq!(got.len(), batch.len());
        assert_eq!(
            got.len() / 2 * 4,
            declared,
            "decoded bytes must equal the declared data size"
        );
    }

    #[test]
    fn stream_dsp_matches_batch_dsp() {
        // Drive encode_stream far enough to write the temp band WAVs (the
        // codec step then fails on the fake exhale path), and compare those
        // against the batch DSP written by hand.
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        let cfg = EncoderConfig {
            profile: Profile::At300,
            total_kbps: 160,
            use_senav: false,
            three_track: false,
            topband_kbps: None,
            exhale: "/nonexistent/exhale",
            opusenc: "/nonexistent/opusenc",
            workdir: &wd,
            hf_tilt_pct: None,
        };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let err = encode_stream(&cfg, &mut bytes, &out);
        assert!(err.is_err(), "expected codec-step failure, got {err:?}");

        // Reference: the same DSP via the batch functions.
        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        assert_eq!(ch, 2);
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (low, high) = sena_dsp::split(&x48, 2, 300.0);
        let low_p: Vec<f64> = low.iter().map(|v| v * PRE_GAIN).collect();
        let low16 = sena_dsp::Resampler::new(SAMPLE_RATE, 16000).process(&low_p, 2);
        let high_p: Vec<f64> = high.iter().map(|v| v * PRE_GAIN).collect();
        let ref_lf = wd.join("ref_lf.wav");
        let ref_hf = wd.join("ref_hf.wav");
        wav::write_s16(&ref_lf, &low16, 16000).unwrap();
        wav::write_f32(&ref_hf, &high_p, SAMPLE_RATE).unwrap();

        let lf = wav::read_f64(&wd.join("lf.wav")).unwrap().0;
        let hf = wav::read_f64(&wd.join("hf.wav")).unwrap().0;
        let (rlf, _, _) = wav::read_f64(&ref_lf).unwrap();
        let (rhf, _, _) = wav::read_f64(&ref_hf).unwrap();
        assert_eq!(lf.len(), rlf.len(), "LF lengths differ");
        assert_eq!(hf.len(), rhf.len(), "HF lengths differ");
        // s16 quantization: allow 1 LSB; f32 path: allow 1e-6.
        let ml = lf
            .iter()
            .zip(rlf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let mh = hf
            .iter()
            .zip(rhf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(ml <= 1.0 / 32768.0 + 1e-9, "LF max diff {ml}");
        assert!(mh < 1e-6, "HF max diff {mh}");
        let _ = std::fs::remove_dir_all(&wd);
    }

    #[test]
    fn stream_hf_tilt_matches_batch() {
        // hf_tilt on: the written HF band WAV must equal the batch reference
        // (split -> batch tilt convolution -> PRE_GAIN).
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-tilt-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        let cfg = EncoderConfig {
            profile: Profile::At300,
            total_kbps: 160,
            use_senav: false,
            three_track: false,
            topband_kbps: None,
            exhale: "/nonexistent/exhale",
            opusenc: "/nonexistent/opusenc",
            workdir: &wd,
            hf_tilt_pct: Some(100.0),
        };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let err = encode_stream(&cfg, &mut bytes, &out);
        assert!(err.is_err(), "expected codec-step failure, got {err:?}");

        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (_low, high) = sena_dsp::split(&x48, 2, 300.0);
        // batch tilt: zero-phase conv (aligned like FirStream)
        let h = sena_dsp::tilt_taps(100.0);
        let d = (h.len() - 1) / 2;
        let n = high.len() / ch;
        let mut tilted = vec![0.0; high.len()];
        for i in 0..n {
            for c in 0..ch {
                let mut acc = 0.0;
                for (k, &t) in h.iter().enumerate() {
                    let ii = i as isize + k as isize - d as isize;
                    if ii >= 0 && (ii as usize) < n {
                        acc += t * high[(ii as usize) * ch + c];
                    }
                }
                tilted[i * ch + c] = acc;
            }
        }
        let ref_hf = wd.join("ref_hf_tilt.wav");
        let high_p: Vec<f64> = tilted.iter().map(|v| v * PRE_GAIN).collect();
        wav::write_f32(&ref_hf, &high_p, SAMPLE_RATE).unwrap();
        let hf = wav::read_f64(&wd.join("hf.wav")).unwrap().0;
        let (rhf, _, _) = wav::read_f64(&ref_hf).unwrap();
        assert_eq!(hf.len(), rhf.len(), "HF lengths differ");
        let mh = hf
            .iter()
            .zip(rhf.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(mh < 1e-6, "HF max diff {mh}");
        let _ = std::fs::remove_dir_all(&wd);
    }

    /// Three-track streaming DSP: mid.wav + hf.wav must equal the batch
    /// reference (split@600 -> split@15600 on the high band -> PRE_GAIN),
    /// and mid + top must sum back to the 600 Hz-high band.
    #[test]
    fn stream_three_track_matches_batch() {
        let src = synth_wav(3, 44100, true);
        let wd = std::env::temp_dir().join(format!("sena-enc-3t-test-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&wd);
        let cfg = EncoderConfig {
            profile: Profile::At300,
            total_kbps: 256,
            use_senav: false,
            three_track: true,
            topband_kbps: None,
            exhale: "/nonexistent/exhale",
            opusenc: "/nonexistent/opusenc",
            workdir: &wd,
            hf_tilt_pct: None,
        };
        let mut bytes = &src[..];
        let out = wd.join("out.sena");
        let err = encode_stream(&cfg, &mut bytes, &out);
        assert!(err.is_err(), "expected codec-step failure, got {err:?}");

        let (x, ch, in_rate) = wav::read_f64_bytes(&src).unwrap();
        let x48 = sena_dsp::Resampler::new(in_rate, SAMPLE_RATE).process(&x, ch);
        let (_low, high) = sena_dsp::split(&x48, 2, 300.0);
        let (mid, top) = sena_dsp::split(&high, 2, sena_core::HF_SPLIT_HZ);
        let mid_p: Vec<f64> = mid.iter().map(|v| v * PRE_GAIN).collect();
        let top_p: Vec<f64> = top.iter().map(|v| v * PRE_GAIN).collect();
        let ref_mid = wd.join("ref_mid.wav");
        let ref_top = wd.join("ref_top.wav");
        wav::write_f32(&ref_mid, &mid_p, SAMPLE_RATE).unwrap();
        wav::write_f32(&ref_top, &top_p, SAMPLE_RATE).unwrap();

        let got_mid = wav::read_f64(&wd.join("mid.wav")).unwrap().0;
        let got_top = wav::read_f64(&wd.join("hf.wav")).unwrap().0;
        let (rmid, _, _) = wav::read_f64(&ref_mid).unwrap();
        let (rtop, _, _) = wav::read_f64(&ref_top).unwrap();
        assert_eq!(got_mid.len(), rmid.len(), "mid lengths differ");
        assert_eq!(got_top.len(), rtop.len(), "top lengths differ");
        let mm = got_mid
            .iter()
            .zip(rmid.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        let mt = got_top
            .iter()
            .zip(rtop.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(mm < 1e-6, "mid max diff {mm}");
        assert!(mt < 1e-6, "top max diff {mt}");

        // Complementarity: mid + top reconstructs the 600 Hz-high band
        // (settled region only; both writers pad the FIR tails with zeros).
        let n = high.len() / 2;
        let settle = 2100usize;
        for i in settle..n - settle {
            let sum_l = got_mid[i * 2] + got_top[i * 2];
            let sum_r = got_mid[i * 2 + 1] + got_top[i * 2 + 1];
            assert!((sum_l - high[i * 2]).abs() < 1e-6, "frame {i} L");
            assert!((sum_r - high[i * 2 + 1]).abs() < 1e-6, "frame {i} R");
        }
        let _ = std::fs::remove_dir_all(&wd);
    }
}
