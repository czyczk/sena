//! Sena decode pipeline: external codec cores -> trim -> resample -> mix.

use std::fmt;

use opus_decoder::{Channels, DecodeMode, Decoder as OpusDecoder};
use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_core::PRE_GAIN;
use sena_dsp::Resampler;

use crate::demux::{Demuxed, Frame, Track};

pub const SAMPLE_RATE: u32 = 48000;
pub const XHE_TRIM_CORE_SAMPLES: usize = 1024;

#[derive(Debug)]
pub enum DecodeError {
    Format(String),
    Unsupported(String),
    Codec(String),
    ShortOutput { have: u64, need: u64 },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Format(s) => write!(f, "format: {s}"),
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::Codec(s) => write!(f, "codec: {s}"),
            DecodeError::ShortOutput { have, need } => {
                write!(f, "decoded output too short: {have} frames, need {need}")
            }
        }
    }
}

impl std::error::Error for DecodeError {}

impl From<crate::demux::DemuxError> for DecodeError {
    fn from(e: crate::demux::DemuxError) -> Self {
        DecodeError::Format(e.to_string())
    }
}

#[derive(Debug, Clone)]
pub struct DecodedInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub playable_frames: u64,
    pub profile: u32,
    pub sena_version: u32,
    /// SENA_AUDIO_SHA256 tag (hash of the encoded Opus + xHE-AAC elementary
    /// streams carried by the container); empty for files that predate the tag.
    pub audio_sha256: String,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadInfo {
    pub start_frame: u64,
    pub frames: u64,
    pub payload_bits: u64,
}

#[derive(Debug)]
pub struct Decoded {
    info: DecodedInfo,
    pcm: Vec<f64>,
    lf_track_pcm: Vec<f64>,
    hf_track_pcm: Vec<f64>,
    bits_total: Vec<f64>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub struct Decoder {
    decoded: Decoded,
    cursor: u64,
    last_read: ReadInfo,
}

#[derive(Debug, Clone)]
pub(crate) struct LfAuInfo {
    pub(crate) start_core: i128,
    pub(crate) samples_core: i128,
    pub(crate) bits: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct OpusPacketInfo {
    pub(crate) start_48k: i128,
    pub(crate) samples_48k: i128,
    pub(crate) bits: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct OpusHead {
    pub(crate) pre_skip: u16,
    pub(crate) output_gain: i16,
}

pub(crate) fn parse_opus_head(data: &[u8]) -> Result<OpusHead, DecodeError> {
    if data.len() < 19 || &data[0..8] != b"OpusHead" {
        return Err(DecodeError::Format("malformed OpusHead".into()));
    }
    let version = data[8];
    if version == 0 || version & 0xF0 != 0 {
        return Err(DecodeError::Unsupported(format!("OpusHead version 0x{version:02x}")));
    }
    let channels = data[9];
    if channels != 2 {
        return Err(DecodeError::Unsupported(format!("OpusHead channels {channels}, expected 2")));
    }
    let input_rate = u32::from_le_bytes(data[12..16].try_into().unwrap());
    if input_rate != SAMPLE_RATE {
        return Err(DecodeError::Unsupported(format!(
            "OpusHead input sample rate {input_rate}, expected {SAMPLE_RATE}"
        )));
    }
    if data[18] != 0 {
        return Err(DecodeError::Unsupported("OpusHead channel mapping family != 0".into()));
    }
    Ok(OpusHead {
        pre_skip: u16::from_le_bytes(data[10..12].try_into().unwrap()),
        output_gain: i16::from_le_bytes(data[16..18].try_into().unwrap()),
    })
}

fn lf_decoder(track: &Track, asc: &AudioSpecificConfig) -> Result<UsacDecoder, DecodeError> {
    let dec = UsacDecoder::new(asc).map_err(|e| DecodeError::Codec(format!("rxaac init: {e}")))?;
    if dec.num_channels() != 2 {
        return Err(DecodeError::Unsupported(format!(
            "rxaac channels {}, expected 2",
            dec.num_channels()
        )));
    }
    let core_rate = dec.output_rate();
    if core_rate != track.sample_rate.round() as u32 {
        return Err(DecodeError::Format(format!(
            "rxaac output rate {core_rate} != container LF rate {}",
            track.sample_rate
        )));
    }
    Ok(dec)
}

fn decode_lf(
    track: &Track,
    frames: &[Frame],
    asc: &AudioSpecificConfig,
) -> Result<(Vec<f32>, Vec<LfAuInfo>, usize, usize), DecodeError> {
    let mut dec = lf_decoder(track, asc)?;
    let core_rate = dec.output_rate();
    // Cache the valid frame geometry before any decode so failed-frame
    // padding never has to query a decoder that may have been left in an
    // inconsistent state by the failed AU.
    let frame_samples = dec.output_samples() * dec.num_channels();
    let mut pcm: Vec<f32> = Vec::new();
    let mut infos = Vec::with_capacity(frames.len());
    let mut errors = 0usize;
    for frame in frames {

        let before = pcm.len();
        // rxaac's contract is Result, and the current deterministic fuzz
        // corpus (upstream + `tests/rxaac_fuzz.rs`) is panic-free. Keep this
        // catch only as a thin last resort so a future decoder panic becomes
        // a clean DecodeError for the host instead of aborting the process.
        // A panic means the decoder state can no longer be trusted, so we
        // fail the decode instead of trying to continue.
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            dec.decode_au(&frame.data, &mut pcm)
        }));
        match result {
            Ok(Err(e)) => {
                eprintln!("warning: LF AU decode error ({}): {e}", frame.t_ns);
                // decode_au may append preroll frames before the main AU
                // fails. A failed AU can also leave decoder state partially
                // mutated, and rxaac exposes no reset(); recreate from the
                // container ASC so one bad AU cannot poison the following
                // frames. The silence length comes from the cached valid
                // geometry above.
                pcm.truncate(before);
                pcm.resize(before + frame_samples, 0.0);
                errors += 1;
                dec = lf_decoder(track, asc)?;
            }
            Err(_) => {
                return Err(DecodeError::Codec(format!(
                    "rxaac decoder panicked on LF AU ({}); aborting decode",
                    frame.t_ns
                )));
            }
            Ok(Ok(())) => {}
        }
        let after = pcm.len();
        let ch = dec.num_channels();
        let samples = (after - before) / ch;
        if samples == 0 || samples * ch != after - before {
            return Err(DecodeError::Codec("rxaac returned a non-integral frame".into()));
        }
        infos.push(LfAuInfo {
            start_core: (before / ch) as i128,
            samples_core: samples as i128,
            bits: (frame.data.len() * 8) as u64,
        });
    }
    Ok((pcm, infos, core_rate as usize, errors))
}

fn decode_opus(
    frames: &[Frame],
    head: &OpusHead,
) -> Result<(Vec<f32>, Vec<OpusPacketInfo>), DecodeError> {
    let mut dec = OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
        .map_err(|e| DecodeError::Codec(format!("opus init: {e}")))?;
    if head.output_gain != 0 {
        dec.set_gain(i32::from(head.output_gain))
            .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
    }
    let mut pcm: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; 5760 * 2];
    let mut infos = Vec::with_capacity(frames.len());
    let mut errors = 0usize;
    for frame in frames {
        let before = pcm.len();
        let mut n = dec
            .decode_float(Some(&frame.data), &mut buf, DecodeMode::Normal)
            .or_else(|e| {
                errors += 1;
                eprintln!("warning: Opus packet decode error ({}): {e}; using PLC", frame.t_ns);
                dec.decode_float(None, &mut buf, DecodeMode::Normal)
            })
            .map_err(|e| DecodeError::Codec(format!("opus decode: {e}")))?;
        n = n.min(5760);
        pcm.extend_from_slice(&buf[..n * 2]);
        infos.push(OpusPacketInfo {
            start_48k: (before / 2) as i128,
            samples_48k: n as i128,
            bits: (frame.data.len() * 8) as u64,
        });
    }
    let _ = errors;
    Ok((pcm, infos))
}

pub(crate) fn add_bits_interval(
    delta: &mut [f64],
    playable: i128,
    start: i128,
    end: i128,
    bits: f64,
) {
    if end <= start || end <= 0 || start >= playable {
        return;
    }
    let s = start.max(0).min(playable) as usize;
    let e = end.max(0).min(playable) as usize;
    if s >= e {
        return;
    }
    let rate = bits / ((end - start) as f64);
    delta[s] += rate;
    delta[e] -= rate;
}

fn build_bit_accounting(
    playable: u64,
    lf_infos: &[LfAuInfo],
    lf_rate: usize,
    opus_infos: &[OpusPacketInfo],
    pre_skip: usize,
) -> (Vec<f64>, Vec<f64>) {
    let n = playable as usize;
    let mut delta = vec![0.0f64; n + 1];
    for info in lf_infos {
        let start_core = info.start_core - XHE_TRIM_CORE_SAMPLES as i128;
        let start48 = start_core * 48_000 / lf_rate as i128;
        let end48 = (start_core + info.samples_core) * 48_000 / lf_rate as i128;
        add_bits_interval(&mut delta, n as i128, start48, end48, info.bits as f64);
    }
    for info in opus_infos {
        let start = info.start_48k - pre_skip as i128;
        let end = start + info.samples_48k;
        add_bits_interval(&mut delta, n as i128, start, end, info.bits as f64);
    }
    // rate_per_frame[i+1] = payload bits attributed to playable frame i.
    // total_bits[end] = sum over frames [0, end): used by read_f32 blocks.
    let mut rate_per_frame = vec![0.0f64; n + 1];
    let mut total_bits = vec![0.0f64; n + 1];
    let mut acc = 0.0;
    let mut total = 0.0;
    for i in 0..n {
        acc += delta[i];
        rate_per_frame[i + 1] = acc;
        total += acc;
        total_bits[i + 1] = total;
    }
    (rate_per_frame, total_bits)
}

/// Parse and validate a Sena container without decoding audio. This is the
/// fast path used by playlist "read info" and tag-read operations.
pub fn probe(demux: &Demuxed) -> Result<(DecodedInfo, Vec<String>), DecodeError> {
    let profile = demux
        .immutable_tag("SENA_PROFILE")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_PROFILE tag".into()))?;
    let profile_num: u32 = profile
        .parse()
        .map_err(|_| DecodeError::Unsupported(format!("bad SENA_PROFILE {profile}")))?;
    if profile_num != 300 && profile_num != 600 {
        return Err(DecodeError::Unsupported(format!("SENA_PROFILE {profile_num}")));
    }
    let version = demux
        .immutable_tag("SENA_VERSION")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_VERSION tag".into()))?;
    let version_num: u32 = version
        .parse()
        .map_err(|_| DecodeError::Unsupported(format!("bad SENA_VERSION {version}")))?;
    if version_num != 1 {
        return Err(DecodeError::Unsupported(format!("SENA_VERSION {version_num}")));
    }
    let playable: u64 = demux
        .immutable_tag("SENA_PLAYABLE_SAMPLES")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_PLAYABLE_SAMPLES tag".into()))?
        .parse()
        .map_err(|_| DecodeError::Unsupported("bad SENA_PLAYABLE_SAMPLES".into()))?;
    if playable == 0 {
        return Err(DecodeError::Unsupported("SENA_PLAYABLE_SAMPLES is zero".into()));
    }

    let mut warnings = demux.warnings.clone();
    let lf_track = demux
        .track("A_SENALF")
        .ok_or_else(|| DecodeError::Format("missing A_SENALF track".into()))?;
    let hf_track = demux
        .track("A_OPUS")
        .ok_or_else(|| DecodeError::Format("missing A_OPUS track".into()))?;
    let lf_rate = lf_track.sample_rate.round() as u32;
    if lf_rate != 16_000 && lf_rate != 32_000 {
        return Err(DecodeError::Unsupported(format!("LF track rate {lf_rate}")));
    }
    if hf_track.sample_rate.round() as u32 != SAMPLE_RATE || lf_track.channels != 2 || hf_track.channels != 2 {
        return Err(DecodeError::Unsupported("track rates/channels do not match Sena P0".into()));
    }

    let lf_delay_expected = (XHE_TRIM_CORE_SAMPLES as u64) * 1_000_000_000 / lf_rate as u64;
    if let Some(actual) = lf_track.codec_delay_ns {
        if actual != lf_delay_expected {
            warnings.push(format!(
                "LF CodecDelay {actual} != expected {lf_delay_expected} ns; using 1024-frame trim"
            ));
        }
    }
    let opus_head = parse_opus_head(&hf_track.codec_private)?;
    let hf_delay_expected = (opus_head.pre_skip as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
    if let Some(actual) = hf_track.codec_delay_ns {
        if actual != hf_delay_expected {
            warnings.push(format!(
                "Opus CodecDelay {actual} != expected {hf_delay_expected} ns; using OpusHead pre-skip"
            ));
        }
    }
    // ASC is parsed for validation only; no decoder state is created here.
    AudioSpecificConfig::parse(&lf_track.codec_private)
        .map_err(|e| DecodeError::Format(format!("ASC parse: {e}")))?;

    let audio_sha256 = demux
        .immutable_tag("SENA_AUDIO_SHA256")
        .unwrap_or("")
        .to_string();
    Ok((DecodedInfo {
        sample_rate: SAMPLE_RATE,
        channels: 2,
        playable_frames: playable,
        profile: profile_num,
        sena_version: version_num,
        audio_sha256,
    }, warnings))
}

pub fn decode(demux: &Demuxed) -> Result<Decoded, DecodeError> {
    let profile = demux
        .immutable_tag("SENA_PROFILE")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_PROFILE tag".into()))?;
    let profile_num: u32 = profile
        .parse()
        .map_err(|_| DecodeError::Unsupported(format!("bad SENA_PROFILE {profile}")))?;
    if profile_num != 300 && profile_num != 600 {
        return Err(DecodeError::Unsupported(format!("SENA_PROFILE {profile_num}")));
    }
    let version = demux
        .immutable_tag("SENA_VERSION")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_VERSION tag".into()))?;
    let version_num: u32 = version
        .parse()
        .map_err(|_| DecodeError::Unsupported(format!("bad SENA_VERSION {version}")))?;
    if version_num != 1 {
        return Err(DecodeError::Unsupported(format!("SENA_VERSION {version_num}")));
    }
    let playable: u64 = demux
        .immutable_tag("SENA_PLAYABLE_SAMPLES")
        .ok_or_else(|| DecodeError::Unsupported("missing SENA_PLAYABLE_SAMPLES tag".into()))?
        .parse()
        .map_err(|_| DecodeError::Unsupported("bad SENA_PLAYABLE_SAMPLES".into()))?;
    if playable == 0 {
        return Err(DecodeError::Unsupported("SENA_PLAYABLE_SAMPLES is zero".into()));
    }

    let mut warnings = demux.warnings.clone();
    let lf_track = demux
        .track("A_SENALF")
        .ok_or_else(|| DecodeError::Format("missing A_SENALF track".into()))?;
    let hf_track = demux
        .track("A_OPUS")
        .ok_or_else(|| DecodeError::Format("missing A_OPUS track".into()))?;
    let lf_rate = lf_track.sample_rate.round() as u32;
    if lf_rate != 16_000 && lf_rate != 32_000 {
        return Err(DecodeError::Unsupported(format!("LF track rate {lf_rate}")));
    }
    if hf_track.sample_rate.round() as u32 != SAMPLE_RATE || lf_track.channels != 2 || hf_track.channels != 2 {
        return Err(DecodeError::Unsupported("track rates/channels do not match Sena P0".into()));
    }

    // CodecDelay cross-check (informational).
    let lf_delay_expected = (XHE_TRIM_CORE_SAMPLES as u64) * 1_000_000_000 / lf_rate as u64;
    if let Some(actual) = lf_track.codec_delay_ns {
        if actual != lf_delay_expected {
            warnings.push(format!(
                "LF CodecDelay {actual} != expected {lf_delay_expected} ns; using 1024-frame trim"
            ));
        }
    }
    let opus_head = parse_opus_head(&hf_track.codec_private)?;
    let hf_delay_expected = (opus_head.pre_skip as u64) * 1_000_000_000 / SAMPLE_RATE as u64;
    if let Some(actual) = hf_track.codec_delay_ns {
        if actual != hf_delay_expected {
            warnings.push(format!(
                "Opus CodecDelay {actual} != expected {hf_delay_expected} ns; using OpusHead pre-skip"
            ));
        }
    }

    let asc = AudioSpecificConfig::parse(&lf_track.codec_private)
        .map_err(|e| DecodeError::Format(format!("ASC parse: {e}")))?;
    let lf_frames: Vec<Frame> = demux.frames_for(lf_track.number).cloned().collect();
    let hf_frames: Vec<Frame> = demux.frames_for(hf_track.number).cloned().collect();
    if lf_frames.is_empty() || hf_frames.is_empty() {
        return Err(DecodeError::Format("container has no audio blocks".into()));
    }

    let (lf_raw, lf_infos, core_rate, lf_errors) = decode_lf(lf_track, &lf_frames, &asc)?;
    let (hf_raw, opus_infos) = decode_opus(&hf_frames, &opus_head)?;
    if lf_errors != 0 {
        warnings.push(format!("{lf_errors} LF AU decode errors replaced by silence"));
    }

    // Leading trims.
    let lf_trim = XHE_TRIM_CORE_SAMPLES;
    if lf_raw.len() < lf_trim * 2 {
        return Err(DecodeError::ShortOutput { have: (lf_raw.len() / 2) as u64, need: lf_trim as u64 });
    }
    let lf_after: Vec<f64> = lf_raw[lf_trim * 2..].iter().map(|&s| f64::from(s)).collect();
    let hf_skip = opus_head.pre_skip as usize;
    if hf_raw.len() < hf_skip * 2 {
        return Err(DecodeError::ShortOutput { have: (hf_raw.len() / 2) as u64, need: hf_skip as u64 });
    }
    let hf_after: Vec<f64> = hf_raw[hf_skip * 2..].iter().map(|&s| f64::from(s)).collect();

    let lf48 = Resampler::new(core_rate as u32, SAMPLE_RATE).process(&lf_after, 2);
    let hf48 = hf_after;

    let n = (lf48.len() / 2).min(hf48.len() / 2);
    if (n as u64) < playable {
        return Err(DecodeError::ShortOutput { have: n as u64, need: playable });
    }
    let gain = 1.0 / PRE_GAIN;
    let mut pcm = Vec::with_capacity(playable as usize * 2);
    let mut lf_track_pcm = Vec::with_capacity(playable as usize * 2);
    let mut hf_track_pcm = Vec::with_capacity(playable as usize * 2);
    for i in 0..(playable as usize * 2) {
        lf_track_pcm.push(lf48[i] * gain);
        hf_track_pcm.push(hf48[i] * gain);
        pcm.push(lf_track_pcm[i] + hf_track_pcm[i]);
    }

    let (_rate_per_frame, bits_total) = build_bit_accounting(playable, &lf_infos, core_rate, &opus_infos, hf_skip);

    let audio_sha256 = demux
        .immutable_tag("SENA_AUDIO_SHA256")
        .unwrap_or("")
        .to_string();
    Ok(Decoded {
        info: DecodedInfo {
            sample_rate: SAMPLE_RATE,
            channels: 2,
            playable_frames: playable,
            profile: profile_num,
            sena_version: version_num,
            audio_sha256,
        },
        pcm,
        lf_track_pcm,
        hf_track_pcm,
        bits_total,
        warnings,
    })
}

impl Decoded {
    pub fn info(&self) -> DecodedInfo {
        self.info.clone()
    }
    pub fn pcm_f64(&self) -> &[f64] {
        &self.pcm
    }
    pub fn lf_track_pcm(&self) -> &[f64] {
        &self.lf_track_pcm
    }
    pub fn hf_track_pcm(&self) -> &[f64] {
        &self.hf_track_pcm
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

impl Decoder {
    pub fn open(demux: &Demuxed) -> Result<Self, DecodeError> {
        let decoded = decode(demux)?;
        Ok(Self { decoded, cursor: 0, last_read: ReadInfo::default() })
    }

    pub fn info(&self) -> DecodedInfo {
        self.decoded.info.clone()
    }

    pub fn warnings(&self) -> &[String] {
        &self.decoded.warnings
    }

    pub fn read_f32(&mut self, out: &mut [f32], max_frames: u64) -> Result<u64, DecodeError> {
        let playable = self.decoded.info.playable_frames;
        let remaining = playable.saturating_sub(self.cursor);
        let want = max_frames.min(remaining) as usize;
        if out.len() < want * 2 {
            return Err(DecodeError::Format("output buffer too small".into()));
        }
        if want == 0 {
            self.last_read = ReadInfo { start_frame: playable, frames: 0, payload_bits: 0 };
            return Ok(0);
        }
        let start = self.cursor as usize;
        let end = start + want;
        for (i, o) in out[..want * 2].iter_mut().enumerate() {
            *o = self.decoded.pcm[i + start * 2] as f32;
        }
        let bits = self.decoded.bits_total[end] - self.decoded.bits_total[start];
        self.last_read = ReadInfo {
            start_frame: self.cursor,
            frames: want as u64,
            payload_bits: bits.max(0.0).round() as u64,
        };
        self.cursor += want as u64;
        Ok(want as u64)
    }

    pub fn read_info(&self) -> ReadInfo {
        self.last_read
    }

    pub fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        if frame > self.decoded.info.playable_frames {
            return Err(DecodeError::Format("seek past playable length".into()));
        }
        self.cursor = frame;
        Ok(())
    }

    pub fn position(&self) -> u64 {
        self.cursor
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sena_mux::mka::{self, Frame as MuxFrame, Track as MuxTrack};

    #[test]
    fn small_au_warmup_lock_vector() {
        // Build a tiny Sena file from the first 3 LF AUs and 3 Opus packets
        // of a real asset. Three LF AUs are just enough to cover the
        // 1024-sample warmup plus a short playable tail.
        let asset = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let bytes = std::fs::read(asset).unwrap();
        let demux = Demuxed::parse(bytes).unwrap();
        let lf = demux.track("A_SENALF").unwrap();
        let hf = demux.track("A_OPUS").unwrap();
        let lf_frames: Vec<_> = demux.frames_for(lf.number).take(3).cloned().collect();
        let hf_frames: Vec<_> = demux.frames_for(hf.number).take(3).cloned().collect();

        let tracks = vec![
            MuxTrack {
                codec_id: "A_OPUS".into(),
                codec_private: hf.codec_private.clone(),
                sample_rate: 48000.0,
                channels: 2,
                codec_delay_ns: hf.codec_delay_ns.unwrap_or(0),
                bit_depth: Some(32),
            },
            MuxTrack {
                codec_id: "A_SENALF".into(),
                codec_private: lf.codec_private.clone(),
                sample_rate: lf.sample_rate,
                channels: 2,
                codec_delay_ns: lf.codec_delay_ns.unwrap_or(0),
                bit_depth: None,
            },
        ];
        let mut frames = Vec::new();
        for (i, f) in hf_frames.iter().enumerate() {
            frames.push(MuxFrame { track: 0, t_ns: i as u64 * 20_000_000, data: f.data.clone() });
        }
        for (i, f) in lf_frames.iter().enumerate() {
            frames.push(MuxFrame { track: 1, t_ns: i as u64 * 64_000_000, data: f.data.clone() });
        }
        // After LF trim: (3072-1024)*3 = 6144 frames; Opus trim is 312
        // samples -> 3*960-312 = 2568 frames. Use the smaller timeline.
        let tags = [("SENA_PROFILE", "300"), ("SENA_VERSION", "1"), ("SENA_PLAYABLE_SAMPLES", "2568")];
        let playable_ns = 2568u64 * 1_000_000_000 / 48_000;
        let file = mka::write_mka(&tracks, frames, &tags, 1000, playable_ns);
        let small = Demuxed::parse(file).unwrap();
        let dec = Decoder::open(&small).unwrap();
        assert_eq!(dec.info().playable_frames, 2568);
        let mut buf = vec![0.0f32; 2568 * 2];
        let n = {
            let mut d = dec;
            d.read_f32(&mut buf, 4096).unwrap()
        };
        assert_eq!(n, 2568);
    }
}
