//! sena-enc: pipeline orchestration (WAV IO, subprocess drivers, packet extraction).

use sena_core::{Profile, PRE_GAIN, SAMPLE_RATE};
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


/// Run two encoder subprocesses concurrently, waiting for both (their
/// stdout/stderr are piped so error text is preserved).
fn run_parallel(
    a: &mut Command,
    b: &mut Command,
    what_a: &str,
    what_b: &str,
) -> Option<Error> {
    use std::process::Stdio;
    a.stdout(Stdio::piped()).stderr(Stdio::piped());
    b.stdout(Stdio::piped()).stderr(Stdio::piped());
    let ha = match a.spawn() {
        Ok(h) => h,
        Err(e) => return Some(Error::Io(e)),
    };
    let hb = match b.spawn() {
        Ok(h) => h,
        Err(e) => return Some(Error::Io(e)),
    };
    let oa = ha.wait_with_output().map_err(Error::Io).ok();
    let ob = hb.wait_with_output().map_err(Error::Io).ok();
    if let Some(o) = oa.as_ref() {
        if !o.status.success() {
            return Some(Error::Subprocess(format!(
                "{what_a} failed: {}",
                String::from_utf8_lossy(&o.stderr)
            )));
        }
    }
    if let Some(o) = ob.as_ref() {
        if !o.status.success() {
            return Some(Error::Subprocess(format!(
                "{what_b} failed: {}",
                String::from_utf8_lossy(&o.stderr)
            )));
        }
    }
    None
}

pub fn check_version(path: &str, marker: Option<&str>, what: &str) -> Result<(), Error> {
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

/// First non-empty line of `tool --version` (for display in `senaenc doctor`).
pub fn opusenc_version(path: &str) -> Result<String, Error> {
    let out = Command::new(path).arg("--version").output().map_err(Error::Io)?;
    for text in [String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr)] {
        for line in text.lines() {
            let l = line.trim();
            if !l.is_empty() {
                return Ok(l.to_string());
            }
        }
    }
    Err(Error::Subprocess(format!("opusenc --version produced no output")))
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
        format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr))
    };
    let out = Command::new(path).arg("-V").output().map_err(Error::Io)?;
    if let Some(v) = parse_exhale_version(&all(&out)) {
        return Ok(v);
    }
    let out = Command::new(path).output().map_err(Error::Io)?;
    if let Some(v) = parse_exhale_version(&all(&out)) {
        return Ok(v);
    }
    Err(Error::Subprocess(format!("cannot determine exhale version at {path}")))
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
    let mut lf_rs: Option<StreamResampler> = None;
    let mut lf_wr: Option<wav::WavWriter> = None;
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
            lf_rs = Some(StreamResampler::new(SAMPLE_RATE, lf_rate));
            lf_wr = Some(wav::WavWriter::create(&wd.join("lf.wav"), lf_rate, 16, 2)?);
            hf_wr = Some(wav::WavWriter::create(&wd.join("hf.wav"), SAMPLE_RATE, 32, 2)?);
            continue;
        }

        match ws.take(48_000)? {
            Some(x) => push_chunk(
                &x,
                ch,
                norm.as_mut().unwrap(),
                split_s.as_mut().unwrap(),
                lf_rs.as_mut().unwrap(),
                lf_wr.as_mut().unwrap(),
                hf_wr.as_mut().unwrap(),
                &mut st,
            )?,
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

    // Flush the pipeline latencies (normalize tail -> split tail -> LF tail).
    let mut norm = norm.unwrap();
    let mut split_s = split_s.unwrap();
    let mut lf_rs = lf_rs.unwrap();
    let mut lf_wr = lf_wr.unwrap();
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
        let low_p: Vec<f64> = low_a.iter().chain(low_b.iter()).map(|v| v * PRE_GAIN).collect();
        st.begin();
        let lf16 = lf_rs.push(&low_p, ch);
        end_tick(&mut st.t, &mut st.lf);
        lf_wr.push(&lf16)?;
        st.begin();
        let lf16b = lf_rs.finish(ch);
        end_tick(&mut st.t, &mut st.lf);
        lf_wr.push(&lf16b)?;
        let high_p: Vec<f64> = high_a.iter().chain(high_b.iter()).map(|v| v * PRE_GAIN).collect();
        hf_wr.push(&high_p)?;
    }
    lf_wr.finish()?;
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
    pub opus: f64,   // Opus encode
    pub mux: f64,    // packet extraction + container mux
}

impl StageTimes {
    pub fn new() -> Self {
        Self { on: std::env::var_os("SENAENC_TIME").is_some(), ..Default::default() }
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
        let total = self.norm + self.split + self.lf + self.write + self.check + self.exhale + self.opus + self.mux;
        eprintln!(
            "SENAENC_TIME: playable={playable} frames\n\
             \x20 normalize {:.3}s  split {:.3}s  lf-down {:.3}s  write {:.3}s\n\
             \x20 check {:.3}s  exhale {:.3}s  opus {:.3}s  mux {:.3}s\n\
             \x20 summed {:.3}s",
            self.norm, self.split, self.lf, self.write, self.check, self.exhale, self.opus, self.mux, total
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
fn push_chunk(
    x: &[f64],
    ch: usize,
    norm: &mut sena_dsp::StreamResampler,
    split_s: &mut sena_dsp::CrossoverStream,
    lf_rs: &mut sena_dsp::StreamResampler,
    lf_wr: &mut wav::WavWriter,
    hf_wr: &mut wav::WavWriter,
    st: &mut StageTimes,
) -> Result<(), Error> {
    st.begin();
    let n48 = norm.push(x, ch);
    end_tick(&mut st.t, &mut st.norm);
    st.begin();
    let (low, high) = split_s.push(&n48, ch);
    end_tick(&mut st.t, &mut st.split);
    if !low.is_empty() {
        let low_p: Vec<f64> = low.iter().map(|v| v * PRE_GAIN).collect();
        st.begin();
        let lf16 = lf_rs.push(&low_p, ch);
        end_tick(&mut st.t, &mut st.lf);
        let mut err: Option<Error> = None;
        st.begin();
        if let Err(e) = lf_wr.push(&lf16) {
            err = Some(e);
        }
        end_tick(&mut st.t, &mut st.write);
        if let Some(e) = err {
            return Err(e);
        }
    }
    if !high.is_empty() {
        let high_p: Vec<f64> = high.iter().map(|v| v * PRE_GAIN).collect();
        let mut err: Option<Error> = None;
        st.begin();
        if let Err(e) = hf_wr.push(&high_p) {
            err = Some(e);
        }
        end_tick(&mut st.t, &mut st.write);
        if let Some(e) = err {
            return Err(e);
        }
    }
    Ok(())
}

/// Encode the two temp WAVs, extract the streams and mux the container.
fn run_codecs(
    cfg: &EncoderConfig,
    wd: &Path,
    playable: usize,
    output: &Path,
    st: &mut StageTimes,
) -> Result<(), Error> {
    let lf_wav = wd.join("lf.wav");
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
    // The two codec passes are independent (separate input files, separate
    // output files) and each is single-threaded, so run them concurrently.
    let lf_m4a = wd.join("lf.m4a");
    let hf_ogg = wd.join("hf.opus");
    let mut ex = Command::new(cfg.exhale);
    ex.arg(cfg.profile.xhe_preset().to_string())
        .arg(&lf_wav)
        .arg(&lf_m4a);
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
    let t0 = std::time::Instant::now();
    let codec_err = run_parallel(&mut ex, &mut op, "exhale", "opusenc");
    if st.on {
        let d = t0.elapsed().as_secs_f64();
        st.exhale += d;
        st.opus += d;
    }
    if let Some(e) = codec_err {
        return Err(e);
    }

    // --- extract packets ---
    st.begin();
    let extracted: Result<_, Error> = (|| {
        let ogg_bytes = std::fs::read(&hf_ogg)?;
        let (hf_head, preskip, hf_packets) = ogg::extract_opus(&ogg_bytes)?;
        let (lf_asc, lf_aus) = extract_m4a(&lf_m4a)?;
        let lf_stream_rate = m4a_stream_rate(&lf_m4a)?;
        Ok((hf_head, preskip, hf_packets, lf_asc, lf_aus, lf_stream_rate))
    })();
    end_tick(&mut st.t, &mut st.mux);
    let (hf_head, preskip, hf_packets, lf_asc, lf_aus, lf_stream_rate) = extracted?;

    // --- frame timings / mux: the remaining tail is tiny and measured with
    // the extraction stage (st.mux). ---
    let muxed = (|| {
        let hf_frame_ns = 20_000_000u64; // 20 ms Opus frames
        // LF AU duration: one 1024-sample core frame at the actual stream rate
        // (64 ms at 16 kHz, 32 ms at 32 kHz).
        let lf_frame_ns =
            (sena_core::XHE_WARMUP_CORE as u64) * 1_000_000_000 / lf_stream_rate as u64;

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
                codec_delay_ns: (sena_core::XHE_WARMUP_CORE as u64)
                    * 1_000_000_000
                    / lf_stream_rate as u64,
                bit_depth: None,
            },
        ];
        let mut frames = vec![];
        for (i, p) in hf_packets.iter().enumerate() {
            frames.push(Frame { track: 0, t_ns: i as u64 * hf_frame_ns, data: p.clone() });
        }
        let mut lf_t = 0u64;
        for au in &lf_aus {
            frames.push(Frame { track: 1, t_ns: lf_t, data: au.clone() });
            lf_t += lf_frame_ns;
        }
        let profile_tag = cfg.profile.crossover_hz().to_string();
        let version_tag = "1".to_string();
        // input length in 48 kHz stereo frames (the normalized source timeline)
        let playable_tag = playable.to_string();
        let tags: Vec<(&str, &str)> = vec![
            ("SENA_PROFILE", &profile_tag),
            ("SENA_VERSION", &version_tag),
            ("SENA_PLAYABLE_SAMPLES", &playable_tag),
        ];
        let playable_ns = (playable as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
        let mka_bytes = mka::write_mka(&tracks, frames, &tags, 1000, playable_ns);
        std::fs::write(output, mka_bytes)?;
        Ok::<(), Error>(())
    })();
    muxed
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exhale_version_parsing() {
        // -V output (exit 0)
        assert_eq!(parse_exhale_version("exhale 1.2.2 (x64, Unicode, Mar 03 2025)"), Some("1.2.2".into()));
        // argument-less banner
        assert_eq!(
            parse_exhale_version(" | version 1.2.3 (x64, built on Mar 03 2025) - written by C.R.Helmrich |"),
            Some("1.2.3".into())
        );
        // release-candidate suffix
        assert_eq!(parse_exhale_version("exhale 1.2.2RC (x64)"), Some("1.2.2".into()));
        // colored banner (colors stripped, still parseable)
        assert_eq!(parse_exhale_version("\x1b[31mexhale\x1b[0m - ecodis ... version 1.2.4 (x64)"), Some("1.2.4".into()));
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
        w.extend_from_slice(&(if stream_len { 0xFFFF_FFFFu32 } else { data_len as u32 }).to_le_bytes());
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
            exhale: "/nonexistent/exhale",
            opusenc: "/nonexistent/opusenc",
            workdir: &wd,
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
        let ml = lf.iter().zip(rlf.iter()).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        let mh = hf.iter().zip(rhf.iter()).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
        assert!(ml <= 1.0 / 32768.0 + 1e-9, "LF max diff {ml}");
        assert!(mh < 1e-6, "HF max diff {mh}");
        let _ = std::fs::remove_dir_all(&wd);
    }
}
