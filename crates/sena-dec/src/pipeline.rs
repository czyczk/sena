//! Sena decode pipeline: external codec cores -> trim -> resample -> mix.

use std::fmt;

use opus_decoder::{Channels, DecodeMode, Decoder as OpusDecoder};
use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_core::PRE_GAIN;
use sena_core::es;
use sena_dsp::Resampler;

use crate::demux::Demuxed;

pub const SAMPLE_RATE: u32 = 48000;
pub const XHE_TRIM_CORE_SAMPLES: usize = 1024;
/// Maximum Opus frame size (120 ms at 48 kHz, RFC 6716 §2).
pub(crate) const MAX_OPUS_FRAME_SAMPLES: usize = 5760;

#[derive(Debug)]
pub enum DecodeError {
    Format(String),
    Unsupported(String),
    Codec(String),
    /// Failure of the host's read/seek callbacks (FFI io layer).
    Io(String),
    ShortOutput {
        have: u64,
        need: u64,
    },
}

impl fmt::Display for DecodeError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DecodeError::Format(s) => write!(f, "format: {s}"),
            DecodeError::Unsupported(s) => write!(f, "unsupported: {s}"),
            DecodeError::Codec(s) => write!(f, "codec: {s}"),
            // "io:" prefix matches the FFI error-code mapping.
            DecodeError::Io(s) => write!(f, "io: {s}"),
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
    /// LF profile number (300 or 600); the HF split is reported separately.
    pub profile: u32,
    /// Three-track layout: a top layer carrying the 12 kHz+ band
    /// (SSB-shifted to baseband, 16 kHz stream) rides along; it is shifted
    /// back up and mixed into the output.
    pub three_track: bool,
    /// Mid-layer codec (Opus or xHE-AAC; the xHE encoder identity is an
    /// encode-side detail and not stored).
    pub mid_codec: sena_core::MidCodec,
    /// ES format version (SENA-INIT version field).
    pub sena_version: u32,
    /// SENA-INIT audio_sha256 (hash of the INIT blob plus composite block
    /// payloads), hex.
    pub audio_sha256: String,
    /// Encoded size of all Cluster elements: codec payloads plus block
    /// framing, excluding Tags, Attachments (cover art) and Void filler.
    /// Average-bitrate displays use this instead of the raw file size so a
    /// large cover does not inflate the reported rate.
    pub audio_span_bytes: u64,
}

#[derive(Debug, Clone, Copy, Default)]
pub struct ReadInfo {
    pub start_frame: u64,
    pub frames: u64,
    pub payload_bits: u64,
}

/// One codec frame of a single layer, split out of a composite block.
#[derive(Debug, Clone)]
pub(crate) struct LaneFrame {
    /// Source block timestamp (diagnostics only).
    pub(crate) t_ns: u64,
    pub(crate) data: Vec<u8>,
}

#[derive(Debug)]
pub struct Decoded {
    info: DecodedInfo,
    pcm: Vec<f64>,
    lf_track_pcm: Vec<f64>,
    mid_track_pcm: Vec<f64>,
    /// A_OPUSHF (12 kHz+ band, restored from the shifted 16 kHz stream)
    /// track, upmixed by 1/PRE_GAIN; empty in the two-track layout.
    top_track_pcm: Vec<f64>,
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

#[derive(Debug, Clone, Copy)]
pub struct OpusHead {
    pub pre_skip: u16,
    pub output_gain: i16,
    /// OpusHead input sample rate field (the layer's stream rate: 48000 for
    /// the mid layer, 16000 for the shifted top layer).
    pub input_rate: u32,
}

pub(crate) fn parse_opus_head(data: &[u8]) -> Result<OpusHead, DecodeError> {
    if data.len() < 19 || &data[0..8] != b"OpusHead" {
        return Err(DecodeError::Format("malformed OpusHead".into()));
    }
    let version = data[8];
    if version == 0 || version & 0xF0 != 0 {
        return Err(DecodeError::Unsupported(format!(
            "OpusHead version 0x{version:02x}"
        )));
    }
    let channels = data[9];
    if channels != 2 {
        return Err(DecodeError::Unsupported(format!(
            "OpusHead channels {channels}, expected 2"
        )));
    }
    if data[18] != 0 {
        return Err(DecodeError::Unsupported(
            "OpusHead channel mapping family != 0".into(),
        ));
    }
    Ok(OpusHead {
        pre_skip: u16::from_le_bytes(data[10..12].try_into().unwrap()),
        output_gain: i16::from_le_bytes(data[16..18].try_into().unwrap()),
        input_rate: u32::from_le_bytes(data[12..16].try_into().unwrap()),
    })
}

fn lf_decoder(asc: &AudioSpecificConfig, lf_rate: u32) -> Result<UsacDecoder, DecodeError> {
    let dec = UsacDecoder::new(asc).map_err(|e| DecodeError::Codec(format!("rxaac init: {e}")))?;
    if dec.num_channels() != 2 {
        return Err(DecodeError::Unsupported(format!(
            "rxaac channels {}, expected 2",
            dec.num_channels()
        )));
    }
    let core_rate = dec.output_rate();
    if core_rate != lf_rate {
        return Err(DecodeError::Format(format!(
            "rxaac output rate {core_rate} != LF rate {lf_rate}"
        )));
    }
    Ok(dec)
}

fn decode_lf(
    lf_rate: u32,
    frames: &[LaneFrame],
    asc: &AudioSpecificConfig,
) -> Result<(Vec<f32>, Vec<LfAuInfo>, usize, usize), DecodeError> {
    let mut dec = lf_decoder(asc, lf_rate)?;
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
                dec = lf_decoder(asc, lf_rate)?;
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
            return Err(DecodeError::Codec(
                "rxaac returned a non-integral frame".into(),
            ));
        }
        infos.push(LfAuInfo {
            start_core: (before / ch) as i128,
            samples_core: samples as i128,
            bits: (frame.data.len() * 8) as u64,
        });
    }
    Ok((pcm, infos, core_rate as usize, errors))
}

/// Decode an opus track at its stream rate. Packet positions in the
/// returned infos are in 48 kHz units (scaled by 48000/`rate`) so the bit
/// accounting lines up with the playable timeline; the OpusHead pre-skip
/// is already in 48 kHz units (RFC 7845).
fn decode_opus(
    frames: &[LaneFrame],
    head: &OpusHead,
    rate: u32,
) -> Result<(Vec<f32>, Vec<OpusPacketInfo>), DecodeError> {
    let mut dec = OpusDecoder::new(rate, Channels::Stereo)
        .map_err(|e| DecodeError::Codec(format!("opus init: {e}")))?;
    if head.output_gain != 0 {
        dec.set_gain(i32::from(head.output_gain))
            .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
    }
    let scale = (SAMPLE_RATE / rate) as i128;
    assert_eq!(rate * scale as u32, SAMPLE_RATE, "opus rate must divide 48k");
    let mut pcm: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * 2];
    let mut infos = Vec::with_capacity(frames.len());
    let mut errors = 0usize;
    for frame in frames {
        let before = pcm.len();
        let mut n = dec
            .decode_float(Some(&frame.data), &mut buf, DecodeMode::Normal)
            .or_else(|e| {
                errors += 1;
                eprintln!(
                    "warning: Opus packet decode error ({}): {e}; using PLC",
                    frame.t_ns
                );
                dec.decode_float(None, &mut buf, DecodeMode::Normal)
            })
            .map_err(|e| DecodeError::Codec(format!("opus decode: {e}")))?;
        n = n.min(MAX_OPUS_FRAME_SAMPLES);
        pcm.extend_from_slice(&buf[..n * 2]);
        infos.push(OpusPacketInfo {
            start_48k: (before / 2) as i128 * scale,
            samples_48k: n as i128 * scale,
            bits: (frame.data.len() * 8) as u64,
        });
    }
    let _ = errors;
    Ok((pcm, infos))
}

/// Gate a raw stereo lane buffer on covering `trim` leading frames.
fn require_covered<T>(raw: &[T], trim: usize) -> Result<(), DecodeError> {
    if raw.len() < trim * 2 {
        return Err(DecodeError::ShortOutput {
            have: (raw.len() / 2) as u64,
            need: trim as u64,
        });
    }
    Ok(())
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

#[allow(clippy::too_many_arguments)]
fn build_bit_accounting(
    playable: u64,
    lf_infos: &[LfAuInfo],
    lf_rate: usize,
    opus_infos: &[OpusPacketInfo],
    pre_skip: usize,
    top_infos: &[OpusPacketInfo],
    top_skip: usize,
) -> (Vec<f64>, Vec<f64>) {
    let n = playable as usize;
    let mut delta = vec![0.0f64; n + 1];
    for info in lf_infos {
        let start_core = info.start_core - XHE_TRIM_CORE_SAMPLES as i128;
        let start48 = start_core * 48_000 / lf_rate as i128;
        let end48 = (start_core + info.samples_core) * 48_000 / lf_rate as i128;
        add_bits_interval(&mut delta, n as i128, start48, end48, info.bits as f64);
    }
    for (infos, skip) in [(opus_infos, pre_skip), (top_infos, top_skip)] {
        for info in infos {
            let start = info.start_48k - skip as i128;
            let end = start + info.samples_48k;
            add_bits_interval(&mut delta, n as i128, start, end, info.bits as f64);
        }
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

/// Parse and validate a Sena INIT blob without decoding audio. This is the
/// fast path used by playlist "read info" and tag-read operations.
pub fn probe_params(
    init: &es::Init,
    audio_span_bytes: u64,
) -> Result<(DecodedInfo, Vec<String>), DecodeError> {
    let params = crate::es::parse_params(init.clone())?;
    let mut info = params.info;
    info.audio_span_bytes = audio_span_bytes;
    Ok((info, params.warnings))
}

pub fn decode(demux: &Demuxed) -> Result<Decoded, DecodeError> {
    let init = &demux.init;
    let params = crate::es::parse_params(init.clone())?;
    let mut warnings = params.warnings.clone();
    let playable = init.playable_samples;
    let profile_num = init.profile.crossover_hz() as u32;
    let three_track = init.three_track;
    let lf_rate = init.lf_rate;
    let asc = &params.asc;
    let opus_head = params.mid_head.unwrap_or(crate::pipeline::OpusHead { pre_skip: 0, output_gain: 0, input_rate: SAMPLE_RATE });
    let tf_head = params.tf_head;

    // Split the composite blocks into per-layer codec frames (timeline
    // order; timestamps are diagnostics only).
    let mut lf_frames: Vec<LaneFrame> = Vec::new();
    let mut mid_frames: Vec<LaneFrame> = Vec::new();
    let mut tf_frames: Vec<LaneFrame> = Vec::new();
    for b in &demux.blocks {
        let block = es::decode_block(&b.data, init.mid_codec)
            .map_err(|e| DecodeError::Format(format!("composite block: {e}")))?;
        match block.mid {
            es::MidSection::Opus(pkt) => mid_frames.push(LaneFrame {
                t_ns: b.t_ns,
                data: pkt.to_vec(),
            }),
            es::MidSection::Xhe { aus, .. } => {
                for au in aus {
                    mid_frames.push(LaneFrame {
                        t_ns: b.t_ns,
                        data: au.to_vec(),
                    });
                }
            }
        }
        for au in &block.lf {
            lf_frames.push(LaneFrame {
                t_ns: b.t_ns,
                data: au.to_vec(),
            });
        }
        for p in &block.hf {
            tf_frames.push(LaneFrame {
                t_ns: b.t_ns,
                data: p.to_vec(),
            });
        }
    }
    if lf_frames.is_empty() || mid_frames.is_empty() || (three_track && tf_frames.is_empty()) {
        return Err(DecodeError::Format("container has no audio blocks".into()));
    }

    // The layer decodes are independent, and so is each layer's post-decode
    // DSP (LF warmup trim + upsample, mid pre-skip trim, top band upsample +
    // shift-back + pre-skip trim): run all of it concurrently so the
    // whole-file path overlaps the DSP with the codec decodes instead of
    // serializing it after them (the ES core consumes blocks interleaved
    // instead).
    let (lf_res, mid_res, top_res) = std::thread::scope(|s| {
        let hlf = s.spawn(|| -> Result<(Vec<f64>, Vec<LfAuInfo>, usize, usize), DecodeError> {
            let (lf_raw, lf_infos, core_rate, lf_errors) = decode_lf(lf_rate, &lf_frames, asc)?;
            let lf_trim = XHE_TRIM_CORE_SAMPLES;
            require_covered(&lf_raw, lf_trim)?;
            let lf_after: Vec<f64> = lf_raw[lf_trim * 2..]
                .iter()
                .map(|&s| f64::from(s))
                .collect();
            let lf48 = Resampler::new(core_rate as u32, SAMPLE_RATE).process(&lf_after, 2);
            Ok((lf48, lf_infos, core_rate, lf_errors))
        });
        let hmid =
            s.spawn(|| -> Result<(Vec<f64>, Vec<OpusPacketInfo>), DecodeError> {
                // xHE mid (44.1 kHz): same warmup-trim + resample shape as
                // the LF lane; infos ride the LF-style core accounting.
                if let Some(mid_asc) = params.mid_xhe_asc.as_ref() {
                    let rate = sena_core::XFI_MID_RATE;
                    let (raw, infos, _core, errs) = decode_lf(rate, &mid_frames, mid_asc)?;
                    if errs != 0 {
                        eprintln!("warning: {errs} mid AU decode errors replaced by silence");
                    }
                    let trim = XHE_TRIM_CORE_SAMPLES;
                    require_covered(&raw, trim)?;
                    let after: Vec<f64> = raw[trim * 2..].iter().map(|&s| f64::from(s)).collect();
                    let up = if rate == sena_core::XFI_MID_RATE {
                        Resampler::new_with_passband(
                            rate,
                            SAMPLE_RATE,
                            sena_core::XFI_MID_RESAMPLE_PASS_HZ,
                        )
                        .process(&after, 2)
                    } else {
                        Resampler::new(rate, SAMPLE_RATE).process(&after, 2)
                    };
                    // Reuse the Opus-info accounting slot; playable mapping
                    // is the constant warmup shift (raw_core - 1024) as in
                    // the LF timeline model.
                    let oinfos = infos
                        .iter()
                        .map(|i| OpusPacketInfo {
                            start_48k: (i.start_core - XHE_TRIM_CORE_SAMPLES as i128)
                                * SAMPLE_RATE as i128
                                / rate as i128,
                            samples_48k: i.samples_core * SAMPLE_RATE as i128 / rate as i128,
                            bits: i.bits,
                        })
                        .collect();
                    return Ok((up, oinfos));
                }
                let (mid_raw, opus_infos) = decode_opus(&mid_frames, &opus_head, SAMPLE_RATE)?;
                let mid_skip = opus_head.pre_skip as usize;
                require_covered(&mid_raw, mid_skip)?;
                let mid_after: Vec<f64> = mid_raw[mid_skip * 2..]
                    .iter()
                    .map(|&s| f64::from(s))
                    .collect();
                Ok((mid_after, opus_infos))
            });
        let htop = s.spawn(
            || -> Result<(Vec<f64>, Vec<OpusPacketInfo>), DecodeError> {
                match &tf_head {
                    Some(head) => {
                        let (raw16, infos) =
                            decode_opus(&tf_frames, head, sena_core::HF_TRACK_RATE)?;
                        // The top stream decodes at 16 kHz: upsample back to
                        // 48 kHz (zero-phase, grid-exact x3), shift the
                        // baseband content back up to the 12 kHz+ band,
                        // then drop the pre-skip (48 kHz units).
                        let top_skip = head.pre_skip as usize;
                        let raw: Vec<f64> = raw16.iter().map(|&s| f64::from(s)).collect();
                        let up =
                            Resampler::new(sena_core::HF_TRACK_RATE, SAMPLE_RATE).process(&raw, 2);
                        require_covered(&up, top_skip)?;
                        let shifted = sena_dsp::shift_up_offset(
                            &up,
                            2,
                            sena_core::HF_SPLIT_HZ,
                            SAMPLE_RATE,
                            -(top_skip as i64),
                        );
                        Ok((shifted[top_skip * 2..].to_vec(), infos))
                    }
                    None => Ok((Vec::new(), Vec::new())),
                }
            },
        );
        let j = hlf.join();
        let j2 = hmid.join();
        let j3 = htop.join();
        (
            j.unwrap_or_else(|e| std::panic::resume_unwind(e)),
            j2.unwrap_or_else(|e| std::panic::resume_unwind(e)),
            j3.unwrap_or_else(|e| std::panic::resume_unwind(e)),
        )
    });
    let (lf48, lf_infos, core_rate, lf_errors) = lf_res?;
    let (mid48, opus_infos) = mid_res?;
    let (top_after, top_infos) = top_res?;
    if lf_errors != 0 {
        warnings.push(format!(
            "{lf_errors} LF AU decode errors replaced by silence"
        ));
    }

    let n = (lf48.len() / 2).min(mid48.len() / 2).min(if three_track {
        top_after.len() / 2
    } else {
        usize::MAX
    });
    if (n as u64) < playable {
        return Err(DecodeError::ShortOutput {
            have: n as u64,
            need: playable,
        });
    }
    let gain = 1.0 / PRE_GAIN;
    let mut pcm = Vec::with_capacity(playable as usize * 2);
    let mut lf_track_pcm = Vec::with_capacity(playable as usize * 2);
    let mut mid_track_pcm = Vec::with_capacity(playable as usize * 2);
    let mut top_track_pcm = Vec::with_capacity(playable as usize * 2);
    for i in 0..(playable as usize * 2) {
        lf_track_pcm.push(lf48[i] * gain);
        mid_track_pcm.push(mid48[i] * gain);
        if three_track {
            top_track_pcm.push(top_after[i] * gain);
        }
        pcm.push(
            lf_track_pcm[i] + mid_track_pcm[i] + top_track_pcm.get(i).copied().unwrap_or(0.0),
        );
    }

    let (_rate_per_frame, bits_total) = build_bit_accounting(
        playable,
        &lf_infos,
        core_rate,
        &opus_infos,
        opus_head.pre_skip as usize,
        &top_infos,
        tf_head.as_ref().map_or(0, |h| h.pre_skip) as usize,
    );

    Ok(Decoded {
        info: DecodedInfo {
            sample_rate: SAMPLE_RATE,
            channels: 2,
            playable_frames: playable,
            profile: profile_num,
            three_track,
            mid_codec: params.init.mid_codec,
            sena_version: es::ES_VERSION as u32,
            audio_sha256: params.info.audio_sha256.clone(),
            audio_span_bytes: demux.audio_span_bytes,
        },
        pcm,
        lf_track_pcm,
        mid_track_pcm,
        top_track_pcm,
        bits_total,
        warnings,
    })
}

impl Decoded {
    pub fn info(&self) -> &DecodedInfo {
        &self.info
    }
    pub fn pcm_f64(&self) -> &[f64] {
        &self.pcm
    }
    pub fn lf_track_pcm(&self) -> &[f64] {
        &self.lf_track_pcm
    }
    pub fn mid_track_pcm(&self) -> &[f64] {
        &self.mid_track_pcm
    }
    /// A_OPUSHF (12 kHz+ band) track PCM; empty slice in the two-track
    /// layout.
    pub fn top_track_pcm(&self) -> &[f64] {
        &self.top_track_pcm
    }
    pub fn warnings(&self) -> &[String] {
        &self.warnings
    }
}

impl Decoder {
    pub fn open(demux: &Demuxed) -> Result<Self, DecodeError> {
        let decoded = decode(demux)?;
        Ok(Self {
            decoded,
            cursor: 0,
            last_read: ReadInfo::default(),
        })
    }

    pub fn info(&self) -> &DecodedInfo {
        &self.decoded.info
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
            self.last_read = ReadInfo {
                start_frame: playable,
                frames: 0,
                payload_bits: 0,
            };
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
    use sena_core::es as core_es;
    use sena_mux::mka;

    fn asset(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../assets/e2e/{name}.sena",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    /// Build a tiny v2 file from the first `n_blocks` blocks of a real
    /// asset, with playable_samples trimmed to `playable`.
    fn trim_asset(name: &str, n_blocks: usize, playable: u64) -> Vec<u8> {
        let demux = Demuxed::parse(asset(name)).unwrap();
        let mut init = demux.init.clone();
        init.playable_samples = playable;
        init.audio_sha256 = [0; 32];
        let cp = core_es::build_codec_private(&init, demux.avg_bytes_per_sec);
        let blocks: Vec<mka::SenaBlock> = demux.blocks[..n_blocks]
            .iter()
            .map(|b| mka::SenaBlock {
                t_ns: b.t_ns,
                data: b.data.clone(),
            })
            .collect();
        mka::write_sena(
            &cp,
            &blocks,
            &core_es::codec_name(init.profile, init.three_track, init.mid_codec),
            1000,
            playable * 1_000_000_000 / SAMPLE_RATE as u64,
        )
    }

    #[test]
    fn small_au_warmup_lock_vector() {
        // Four blocks of a @300 asset: LF AU 0 (warmup) at block 0 and AU 1
        // at block 3 (by-end placement) cover playable [0, 1024) core =
        // 3072 frames at 48 kHz after the warmup trim.
        let file = trim_asset("01__p300__lfa__hf144", 4, 3072);
        let small = Demuxed::parse(file).unwrap();
        let dec = Decoder::open(&small).unwrap();
        assert_eq!(dec.info().playable_frames, 3072);
        let mut buf = vec![0.0f32; 3072 * 2];
        let n = {
            let mut d = dec;
            d.read_f32(&mut buf, 4096).unwrap()
        };
        assert_eq!(n, 3072);
        assert!(buf.iter().any(|&s| s != 0.0), "decoded output is silent");
    }

    /// Three-layer layout: probe reports it, whole-file decode mixes the top
    /// band, and the mix is exactly lf+mid+top.
    #[test]
    fn three_track_layout() {
        let demux = Demuxed::parse(asset("01__p600__3t__hf160+96")).unwrap();

        let (info, warnings) = probe_params(&demux.init, demux.audio_span_bytes).unwrap();
        assert!(warnings.is_empty(), "{warnings:?}");
        assert_eq!(info.profile, 600);
        assert!(info.three_track);
        assert!(demux.init.hf_head.is_some());

        let decoded = decode(&demux).unwrap();
        assert!(decoded.info().three_track);
        assert_eq!(decoded.pcm_f64().len(), 960000 * 2);
        assert_eq!(decoded.top_track_pcm().len(), 960000 * 2);
        let top_peak = decoded
            .top_track_pcm()
            .iter()
            .fold(0.0f64, |a, &v| a.max(v.abs()));
        assert!(top_peak > 1e-4, "top layer is silent (peak {top_peak})");
        let mix_peak = decoded
            .pcm_f64()
            .iter()
            .zip(
                decoded
                    .lf_track_pcm()
                    .iter()
                    .zip(decoded.mid_track_pcm().iter().zip(decoded.top_track_pcm().iter())),
            )
            .map(|(m, (l, (h, t)))| (m - l - h - t).abs())
            .fold(0.0f64, f64::max);
        assert!(mix_peak < 1e-9, "pcm != lf+mid+top (max {mix_peak})");
    }

    /// INIT-level consistency: the three-track flag requires the top head.
    #[test]
    fn three_track_flag_requires_head() {
        let demux = Demuxed::parse(asset("01__p600__lfd__hf136")).unwrap();
        assert!(!demux.init.three_track);
        let mut bad = demux.init.encode();
        // set flags bit0 (three-track) without adding a head
        bad[7] |= 1;
        assert!(core_es::Init::decode(&bad).is_err());
    }
}
