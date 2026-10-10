//! ES decoder core: decodes the Sena composite elementary stream block by
//! block. Feeds: a container block store (random access, full seek) or a
//! host-pushed queue (DirectShow/ffmpeg style), with seek warmup from a
//! shadow block source and a bounded cold-start ramp.
//!
//! Lane bookkeeping is absolute: LF infos are anchored by the block's
//! `au_index`, Opus infos by packet index, so bit accounting maps to the
//! playable timeline with constant trims (1024-sample LF warmup, OpusHead
//! pre-skip), regardless of where decoding started.

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};

use opus_decoder::{Channels, DecodeMode, Decoder as OpusDecoder};
use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_core::es;
use sena_core::es::Init;
use sena_dsp::{HILBERT_DELAY, Resampler};

use crate::demux::BlockRef;
use crate::pipeline::{
    DecodeError, DecodedInfo, LfAuInfo, OpusPacketInfo, ReadInfo, SAMPLE_RATE, add_bits_interval,
    parse_opus_head,
};

const MIN_CHUNK_FRAMES: u64 = 4800; // 100 ms @48k
/// USAC independency flag: bit 7 of the AU's first payload byte.
const INDEP_BIT: u8 = 0x80;
/// The top layer's pre-skip must stay below one block for the by-end
/// placement to line up (OpusHead pre-skip is 312 in practice).
const MAX_TF_PRESKIP: u16 = 959;

/// Where composite block payloads come from. Both variants share the same
/// `BlockRef` index; only the payload fetch differs.
pub enum BlockStore {
    /// Whole file in memory (non-seekable inputs, examples): payloads are
    /// copied out of the buffer on demand.
    Memory {
        bytes: Vec<u8>,
        blocks: Vec<BlockRef>,
    },
    /// Lazy positioned reads through the host's io callbacks: the index is
    /// the only per-file state, decode fetches one block at a time. A small
    /// read-ahead window keeps linear decode from doing a seek+read syscall
    /// per block.
    Lazy {
        blocks: Vec<BlockRef>,
        read_at: Box<dyn FnMut(u64, usize) -> Result<Vec<u8>, String>>,
        /// `(absolute offset of window[0], window bytes)`.
        window: (u64, Vec<u8>),
    },
}

/// Read-ahead for lazy block fetches; blocks are consumed in file order, so
/// one window covers hundreds of blocks.
const LAZY_READ_AHEAD: usize = 256 * 1024;

impl BlockStore {
    fn blocks(&self) -> &[BlockRef] {
        match self {
            BlockStore::Memory { blocks, .. } => blocks,
            BlockStore::Lazy { blocks, .. } => blocks,
        }
    }

    /// Fetch the payload of block `index` (one bounded allocation per call).
    fn read(&mut self, index: usize) -> Result<Vec<u8>, DecodeError> {
        let br = self.blocks()[index];
        match self {
            BlockStore::Memory { bytes, .. } => {
                let start = usize::try_from(br.offset)
                    .map_err(|_| DecodeError::Format("block offset too large".into()))?;
                bytes
                    .get(start..start + br.len as usize)
                    .map(|s| s.to_vec())
                    .ok_or_else(|| DecodeError::Format("block range outside file".into()))
            }
            BlockStore::Lazy {
                read_at, window, ..
            } => {
                let len = br.len as usize;
                let end = br.offset + len as u64;
                let wend = window.0 + window.1.len() as u64;
                if br.offset < window.0 || end > wend {
                    let fetch = len.max(LAZY_READ_AHEAD);
                    let buf = read_at(br.offset, fetch).map_err(DecodeError::Io)?;
                    if buf.len() < len {
                        return Err(DecodeError::Format("truncated block payload".into()));
                    }
                    *window = (br.offset, buf);
                }
                let off = (br.offset - window.0) as usize;
                Ok(window.1[off..off + len].to_vec())
            }
        }
    }
}

/// Static per-file parameters distilled from the INIT blob.
pub struct EsParams {
    pub init: Init,
    pub asc: AudioSpecificConfig,
    /// Mid-layer OpusHead (Opus mid only).
    pub mid_head: Option<crate::pipeline::OpusHead>,
    /// Mid-layer xHE-AAC config (xHE mid only, v3).
    pub mid_xhe_asc: Option<AudioSpecificConfig>,
    pub tf_head: Option<crate::pipeline::OpusHead>,
    pub info: DecodedInfo,
    pub warnings: Vec<String>,
}

/// Parse and validate an INIT blob into decoder-ready parameters.
pub fn parse_params(init: Init) -> Result<EsParams, DecodeError> {
    let asc = AudioSpecificConfig::parse(&init.lf_asc)
        .map_err(|e| DecodeError::Format(format!("ASC parse: {e}")))?;
    let (mid_head, mid_xhe_asc) = match init.mid_codec {
        sena_core::MidCodec::Opus => {
            let head = parse_opus_head(&init.mid_head)?;
            if head.input_rate != SAMPLE_RATE {
                return Err(DecodeError::Unsupported(format!(
                    "mid OpusHead input sample rate {}, expected {SAMPLE_RATE}",
                    head.input_rate
                )));
            }
            (Some(head), None)
        }
        sena_core::MidCodec::Xhe => {
            if !init.three_track {
                return Err(DecodeError::Format(
                    "xHE mid without the three-track layout".into(),
                ));
            }
            let asc = AudioSpecificConfig::parse(&init.mid_head)
                .map_err(|e| DecodeError::Format(format!("mid ASC parse: {e}")))?;
            (None, Some(asc))
        }
    };
    let tf_head = match &init.hf_head {
        Some(h) => {
            let head = parse_opus_head(h)?;
            if head.input_rate != sena_core::HF_TRACK_RATE {
                return Err(DecodeError::Unsupported(format!(
                    "top OpusHead input sample rate {}, expected {}",
                    head.input_rate,
                    sena_core::HF_TRACK_RATE
                )));
            }
            if head.pre_skip > MAX_TF_PRESKIP {
                return Err(DecodeError::Unsupported(format!(
                    "top OpusHead pre-skip {} exceeds one block",
                    head.pre_skip
                )));
            }
            Some(head)
        }
        None => None,
    };
    if init.three_track != tf_head.is_some() {
        return Err(DecodeError::Format(
            "three-track flag/head mismatch in SENA-INIT".into(),
        ));
    }
    if init.playable_samples == 0 {
        return Err(DecodeError::Unsupported("playable_samples is zero".into()));
    }
    let audio_sha256: String = init
        .audio_sha256
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    Ok(EsParams {
        info: DecodedInfo {
            sample_rate: SAMPLE_RATE,
            channels: 2,
            playable_frames: init.playable_samples,
            profile: init.profile.crossover_hz() as u32,
            three_track: init.three_track,
            mid_codec: init.mid_codec,
            sena_version: es::ES_VERSION as u32,
            audio_sha256,
            audio_span_bytes: 0, // filled by the container layer
        },
        asc,
        mid_head,
        mid_xhe_asc,
        tf_head,
        init,
        warnings: Vec::new(),
    })
}

/// One Opus decode lane (the 48 kHz mid layer or the 16 kHz top layer).
struct OpusLane<'a> {
    dec: &'a mut OpusDecoder,
    name: &'static str,
    errors: &'a mut usize,
    raw: &'a mut VecDeque<f32>,
    infos: &'a mut Vec<OpusPacketInfo>,
    total_before: &'a mut i128,
    /// 48 kHz samples per decoded sample: 1 at 48 kHz, 3 for the 16 kHz
    /// stream (positions in `infos` are absolute 48 kHz units so the bit
    /// accounting lines up with the playable timeline).
    scale: i128,
}

/// Decode one Opus packet with PLC fallback, append the PCM to the lane's
/// raw queue and record the packet info.
fn decode_opus_packet(lane: &mut OpusLane, data: &[u8], t_ns: u64) -> Result<(), DecodeError> {
    let mut buf = vec![0.0f32; 5760 * 2];
    let mut n = match lane.dec.decode_float(Some(data), &mut buf, DecodeMode::Normal) {
        Ok(n) => n,
        Err(e) => {
            eprintln!(
                "warning: {} packet decode error ({t_ns}): {e}; using PLC",
                lane.name
            );
            *lane.errors += 1;
            lane.dec
                .decode_float(None, &mut buf, DecodeMode::Normal)
                .map_err(|e| DecodeError::Codec(format!("opus PLC ({}): {e}", lane.name)))?
        }
    };
    n = n.min(5760);
    lane.raw.extend(buf[..n * 2].iter().copied());
    lane.infos.push(OpusPacketInfo {
        start_48k: *lane.total_before,
        samples_48k: (n as i128) * lane.scale,
        bits: (data.len() * 8) as u64,
    });
    *lane.total_before += (n as i128) * lane.scale;
    Ok(())
}

/// xHE mid lane state (v3). Field-for-field mirror of the LF lane's
/// trim/context/anchor machinery, at 44.1 kHz core rate.
struct MxLane {
    dec: UsacDecoder,
    resampler: Resampler,
    raw: VecDeque<f32>,
    trim: usize,
    trim_pending: bool,
    trim_after: bool,
    anchored: bool,
    muted: bool,
    join48: Option<i64>,
    decode_from: u64,
    total_before: i128,
    ctx: usize,
    zero_prefix: usize,
    errors: usize,
}

struct State {
    lf_rate: u32,
    /// Leading output trims applied on the first produce after a (re)start:
    /// one 1024-sample core frame at stream start; at a seek, the raw
    /// samples from the jump AU up to the target frame.
    lf_trim: usize,
    hf_trim: usize,
    /// xHE mid lane (v3, 44.1 kHz): mirrors the LF lane (same warmup model:
    /// AU0 = 2048, trim 1024 core, steady 1024/AU; measured on Fraunhofer
    /// IIS CBR 128k streams via rxaac). None for an Opus mid. Boxed: a
    /// second UsacDecoder inline in State overflowed the foobar2000 decode
    /// thread's stack (see decode_fits_small_host_stack).
    mx: Option<Box<MxLane>>,
    /// Top layer pre-skip/seek trim (48 kHz units, applied on the
    /// upsampled/shifted timeline).
    tf_trim: usize,
    /// Next top-layer packet index (blocks and packets share the 20 ms
    /// grid; the final block may carry two).
    tf_next_pkt: u64,
    /// Lane indices below these are skipped without decoding (seek jump).
    lf_decode_from: u64,
    mid_decode_from: u64,
    tf_decode_from: u64,
    /// Whether the LF lane has been anchored (au_index seen). Only false
    /// between a cold mid-stream start and the first LF section.
    lf_anchored: bool,
    /// Ramp mode (cold mid-stream start): LF AUs are skipped until the
    /// first independency AU; the LF output stays silent until the join.
    lf_muted: bool,
    /// Ramp join in the future: absolute playable frame where the joined LF
    /// window starts. Cleared once production reaches it.
    lf_join48: Option<i64>,
    /// Seek (true): keep the raw LF context before the trim point so the
    /// zero-phase resampler has real audio instead of zeros (start: false).
    lf_trim_after: bool,
    /// Apply `lf_trim` on the next produce (set at restart and at a ramp
    /// join).
    lf_trim_pending: bool,
    lf_resampler: Resampler,
    /// Core samples at the `lf_raw` head kept purely as resampler context
    /// (already accounted in the playable timeline).
    lf_ctx: usize,
    /// Core samples at the decode start to replace with zeros (seek anchored
    /// at AU 0: the warmup AU must not leak into the resampler context).
    lf_zero_prefix: usize,
    xdec: UsacDecoder,
    opus: OpusDecoder,
    opus2: Option<OpusDecoder>,
    tf_resampler: Option<Resampler>,
    lf_raw: VecDeque<f32>,
    hf_raw: VecDeque<f32>,
    /// Decoded 16 kHz samples of the top layer; the head frame corresponds
    /// to raw 16 kHz index `tf_base16` (relative to the decode anchor).
    tf_raw: VecDeque<f32>,
    /// Total raw 16 kHz frames drained from the `tf_raw` head so far.
    tf_base16: usize,
    /// Playable frame index of the last (re)start: the shift-up carrier
    /// phase is locked to the playable timeline (phase 0 at frame 0).
    tf_seek_base: u64,
    /// Absolute raw positions: LF core samples / Opus 48 kHz units decoded
    /// so far (anchored at the lane's decode-from index).
    lf_total_before: i128,
    opus_total_before: i128,
    tf_total_before: i128,
    lf_infos: Vec<LfAuInfo>,
    opus_infos: Vec<OpusPacketInfo>,
    tf_infos: Vec<OpusPacketInfo>,
    lf_errors: usize,
    opus_errors: usize,
    tf_errors: usize,
    /// Cumulative payload bytes decoded per layer (stats display).
    lf_bytes: u64,
    mid_bytes: u64,
    tf_bytes: u64,
    blocks_decoded: u64,
    output: VecDeque<f64>,
    first: bool,
    produced: u64,
    finished: bool,
}

/// Emittable top-band output frames (48 kHz, playable timeline) from the
/// current `tf_raw` window: upsample the whole pending 16 kHz window, keep
/// only fully-supported outputs (resampler guard on the right unless the
/// stream is finished, Hilbert half-length for the shift), and count what
/// lies ahead of the current emit point. Negative means "need more input".
fn tf_emittable(st: &State) -> i128 {
    let Some(rs) = st.tf_resampler.as_ref() else {
        return i128::MAX;
    };
    let w16 = st.tf_raw.len() / 2;
    let right_cap = if st.finished {
        rs.output_frames(w16)
    } else {
        rs.output_frames(w16.saturating_sub(rs.guard_frames()))
            .saturating_sub(HILBERT_DELAY)
    };
    // Window emit index of the current production point (the decode anchor
    // is folded into tf_trim, as in the v1 streaming decoder).
    let j = st.produced as i128 + st.tf_trim as i128 - 3 * st.tf_base16 as i128;
    right_cap as i128 - j
}

pub struct EsDecoder {
    params: EsParams,
    /// Container feed: lazy block source + next block index.
    store: Option<BlockStore>,
    store_index: usize,
    /// Push feed: host-queued blocks (timestamp ns, payload).
    queue: VecDeque<(u64, Vec<u8>)>,
    host_eof: bool,
    /// After a push-mode warmup, blocks older than this timestamp were
    /// already decoded via the shadow source and are dropped on push.
    live_from_ts: Option<u64>,
    /// Push-mode seek target: pull discards output before this frame.
    output_start: Option<u64>,
    /// Cold-start anchor of the push feed has been configured.
    push_started: bool,
    state: Option<State>,
    /// Absolute playable frames read.
    cursor: u64,
    bits_base: u64,
    bits_prefix: Vec<f64>,
    last_read: ReadInfo,
    pub warnings: Vec<String>,
}

impl EsDecoder {
    /// Container mode: decode from a block store.
    pub fn open_store(params: EsParams, store: BlockStore) -> Result<Self, DecodeError> {
        let warnings = params.warnings.clone();
        Ok(Self {
            params,
            store: Some(store),
            store_index: 0,
            queue: VecDeque::new(),
            host_eof: false,
            live_from_ts: None,
            output_start: None,
            push_started: false,
            state: None,
            cursor: 0,
            bits_base: 0,
            bits_prefix: vec![0.0],
            last_read: ReadInfo::default(),
            warnings,
        })
    }

    /// Push mode: the host pushes composite blocks in timeline order.
    pub fn open_push(params: EsParams) -> Result<Self, DecodeError> {
        let warnings = params.warnings.clone();
        Ok(Self {
            params,
            store: None,
            store_index: 0,
            queue: VecDeque::new(),
            host_eof: false,
            live_from_ts: None,
            output_start: None,
            push_started: false,
            state: None,
            cursor: 0,
            bits_base: 0,
            bits_prefix: vec![0.0],
            last_read: ReadInfo::default(),
            warnings,
        })
    }

    pub fn info(&self) -> DecodedInfo {
        self.params.info.clone()
    }

    pub fn init(&self) -> &Init {
        &self.params.init
    }

    /// True when the feed is exhausted and no more output can be produced.
    pub fn at_eof(&self) -> bool {
        self.feed_exhausted()
            && self
                .state
                .as_ref()
                .map_or(false, |st| st.finished && st.output.is_empty())
    }

    fn playable(&self) -> u64 {
        self.params.info.playable_frames
    }

    fn make_state(&self) -> Result<State, DecodeError> {
        let xdec = UsacDecoder::new(&self.params.asc)
            .map_err(|e| DecodeError::Codec(format!("rxaac init: {e}")))?;
        if xdec.num_channels() != 2 {
            return Err(DecodeError::Unsupported("rxaac channels != 2".into()));
        }
        let mut opus = OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
            .map_err(|e| DecodeError::Codec(format!("opus init: {e}")))?;
        if let Some(mid_head) = self.params.mid_head.as_ref() {
            if mid_head.output_gain != 0 {
                opus.set_gain(i32::from(mid_head.output_gain))
                    .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
            }
        }
        let mut opus2 = None;
        let mut tf_resampler = None;
        if let Some(head) = self.params.tf_head.as_ref() {
            let mut d = OpusDecoder::new(sena_core::HF_TRACK_RATE, Channels::Stereo)
                .map_err(|e| DecodeError::Codec(format!("opus init (top): {e}")))?;
            if head.output_gain != 0 {
                d.set_gain(i32::from(head.output_gain))
                    .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
            }
            opus2 = Some(d);
            tf_resampler = Some(Resampler::new(sena_core::HF_TRACK_RATE, SAMPLE_RATE));
        }
        let mx = match &self.params.mid_xhe_asc {
            Some(asc) => {
                let dec = UsacDecoder::new(asc)
                    .map_err(|e| DecodeError::Codec(format!("rxaac init (mid): {e}")))?;
                if dec.num_channels() != 2 {
                    return Err(DecodeError::Unsupported("rxaac mid channels != 2".into()));
                }
                if dec.output_rate() != sena_core::XFI_MID_RATE {
                    return Err(DecodeError::Format(format!(
                        "rxaac mid output rate {} != {}",
                        dec.output_rate(),
                        sena_core::XFI_MID_RATE
                    )));
                }
                Some(Box::new(MxLane {
                    dec,
                    resampler: Resampler::new(sena_core::XFI_MID_RATE, SAMPLE_RATE),
                    raw: VecDeque::new(),
                    trim: crate::pipeline::XHE_TRIM_CORE_SAMPLES,
                    trim_pending: true,
                    trim_after: false,
                    anchored: true,
                    muted: false,
                    join48: None,
                    decode_from: 0,
                    total_before: 0,
                    ctx: 0,
                    zero_prefix: 0,
                    errors: 0,
                }))
            }
            None => None,
        };
        Ok(State {
            lf_rate: self.params.init.lf_rate,
            lf_trim: crate::pipeline::XHE_TRIM_CORE_SAMPLES,
            hf_trim: self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip as usize),
            mx,
            tf_trim: self.params.tf_head.as_ref().map_or(0, |h| h.pre_skip as usize),
            tf_next_pkt: 0,
            lf_decode_from: 0,
            mid_decode_from: 0,
            tf_decode_from: 0,
            lf_anchored: true,
            lf_muted: false,
            lf_join48: None,
            lf_trim_after: false,
            lf_trim_pending: true,
            lf_resampler: Resampler::new(self.params.init.lf_rate, SAMPLE_RATE),
            lf_ctx: 0,
            lf_zero_prefix: 0,
            xdec,
            opus,
            opus2,
            tf_resampler,
            lf_raw: VecDeque::new(),
            hf_raw: VecDeque::new(),
            tf_raw: VecDeque::new(),
            tf_base16: 0,
            tf_seek_base: 0,
            lf_total_before: 0,
            opus_total_before: 0,
            tf_total_before: 0,
            lf_infos: Vec::new(),
            opus_infos: Vec::new(),
            tf_infos: Vec::new(),
            lf_errors: 0,
            opus_errors: 0,
            tf_errors: 0,
            lf_bytes: 0,
            mid_bytes: 0,
            tf_bytes: 0,
            blocks_decoded: 0,
            output: VecDeque::new(),
            first: true,
            produced: 0,
            finished: false,
        })
    }
}

impl EsDecoder {
    fn feed_next(&mut self) -> Result<Option<(u64, Vec<u8>)>, DecodeError> {
        if let Some(store) = &mut self.store {
            if self.store_index < store.blocks().len() {
                let t_ns = store.blocks()[self.store_index].t_ns;
                let data = store.read(self.store_index)?;
                self.store_index += 1;
                return Ok(Some((t_ns, data)));
            }
            return Ok(None);
        }
        Ok(self.queue.pop_front())
    }

    fn feed_exhausted(&self) -> bool {
        if let Some(store) = &self.store {
            self.store_index >= store.blocks().len()
        } else {
            self.host_eof && self.queue.is_empty()
        }
    }

    /// Push one composite block (push mode). `t_ns` is the block's playable
    /// timeline timestamp in nanoseconds (20 ms grid).
    pub fn push(&mut self, t_ns: u64, data: &[u8]) -> Result<(), DecodeError> {
        if let Some(live) = self.live_from_ts {
            if t_ns < live {
                return Ok(()); // already decoded via the warmup shadow source
            }
        }
        // integrity check so host bugs surface loudly
        es::decode_block(data, self.params.init.mid_codec)
            .map_err(|e| DecodeError::Format(format!("push: {e}")))?;
        self.queue.push_back((t_ns, data.to_vec()));
        Ok(())
    }

    pub fn queued(&self) -> usize {
        self.queue.len()
    }

    /// Signal end of stream on the push feed.
    pub fn finish(&mut self) {
        self.host_eof = true;
    }

    /// Reset to the uninitialized state (push-mode flush). The next push
    /// re-anchors the timeline from its timestamp.
    pub fn reset(&mut self) {
        self.state = None;
        self.queue.clear();
        self.live_from_ts = None;
        self.output_start = None;
        self.push_started = false;
        self.cursor = 0;
        self.bits_base = 0;
        self.bits_prefix = vec![0.0];
        self.last_read = ReadInfo::default();
    }

    /// Push-mode: discard output before `frame` on subsequent pulls (the
    /// demuxer delivers from a cue point earlier than the seek target).
    pub fn set_output_start(&mut self, frame: u64) {
        self.output_start = Some(frame);
    }

    /// Push-mode seek with a shadow block source: decode the warmup window
    /// through the shadow store so the state at the delivery point equals
    /// the continuous decode (same quality as a container seek). Blocks the
    /// host then re-delivers below the shadow cursor are dropped by
    /// timestamp dedupe.
    pub fn warmup_seek(&mut self, store: BlockStore, target: u64) -> Result<(), DecodeError> {
        if target > self.playable() {
            return Err(DecodeError::Format("seek past playable length".into()));
        }
        // Full container-style seek on the shadow source...
        self.store = Some(store);
        self.store_index = 0;
        self.queue.clear();
        self.live_from_ts = None;
        self.output_start = None;
        self.state = None;
        self.seek(target)?;
        // ...then force one production round so the warmup window is
        // actually decoded (a bare seek only positions the output). The
        // produced PCM is kept: it covers [target, target + chunk) and the
        // host's re-delivery of those blocks is deduped by timestamp.
        self.fill(1)?;
        // None = the warmup consumed the whole file: every later push is
        // already decoded, so dedupe drops them all.
        let live_from = self
            .store
            .as_ref()
            .and_then(|s| s.blocks().get(self.store_index))
            .map(|b| b.t_ns)
            .unwrap_or(u64::MAX);
        self.live_from_ts = Some(live_from);
        // Detach the shadow source; the live queue takes over.
        self.store = None;
        self.host_eof = false;
        self.push_started = true;
        Ok(())
    }
}

impl EsDecoder {
    /// Decode the lanes of one composite block.
    fn decode_block(&mut self, t_ns: u64, payload: &[u8]) -> Result<(), DecodeError> {
        let block = es::decode_block(payload, self.params.init.mid_codec)
            .map_err(|e| DecodeError::Format(e.to_string()))?;
        let st = self.state.as_mut().unwrap();
        st.blocks_decoded += 1;
        match block.mid {
            es::MidSection::Opus(pkt) => {
                let mid_pkt = t_ns / es::BLOCK_NS;
                if mid_pkt >= st.mid_decode_from {
                    st.mid_bytes += pkt.len() as u64;
                    let mut lane = OpusLane {
                        dec: &mut st.opus,
                        name: "Opus",
                        errors: &mut st.opus_errors,
                        raw: &mut st.hf_raw,
                        infos: &mut st.opus_infos,
                        total_before: &mut st.opus_total_before,
                        scale: 1,
                    };
                    decode_opus_packet(&mut lane, pkt, t_ns)?;
                }
            }
            es::MidSection::Xhe { au_index, aus } => {
                for (i, au) in aus.iter().enumerate() {
                    let au_idx = au_index as u64 + i as u64;
                    // Anchor/ramp logic mirrors the LF lane (below), at the
                    // 44.1 kHz mid rate.
                    {
                        let mx = st.mx.as_mut().unwrap();
                        if !mx.anchored {
                            mx.anchored = true;
                            let au_start48 = mx_au_start48(au_idx, t_ns);
                            if au_idx == 0 && au_start48 < 0 {
                                mx.decode_from = 0;
                                mx.total_before = 0;
                                mx.trim = crate::pipeline::XHE_TRIM_CORE_SAMPLES;
                                mx.trim_after = false;
                                mx.trim_pending = true;
                            } else if au_payload_indep(au) {
                                mx.decode_from = au_idx;
                                mx.total_before =
                                    1024 + au_start48 as i128 * sena_core::XFI_MID_RATE as i128
                                        / SAMPLE_RATE as i128;
                                let start_abs = (st.tf_seek_base + st.produced) as i64;
                                if au_start48 <= start_abs {
                                    mx.trim = ((start_abs - au_start48)
                                        * sena_core::XFI_MID_RATE as i64
                                        / SAMPLE_RATE as i64)
                                        .max(0) as usize;
                                    mx.trim_after = true;
                                    mx.trim_pending = true;
                                } else {
                                    mx.trim = 0;
                                    mx.join48 = Some(au_start48);
                                }
                            } else {
                                mx.muted = true;
                                mx.decode_from = au_idx;
                            }
                        }
                    }
                    {
                        let mx = st.mx.as_mut().unwrap();
                        if au_idx < mx.decode_from {
                            continue;
                        }
                        if mx.muted {
                            if !au_payload_indep(au) {
                                continue;
                            }
                            let au_start48 = mx_au_start48(au_idx, t_ns);
                            let start_abs = (st.tf_seek_base + st.produced) as i64;
                            mx.muted = false;
                            mx.decode_from = au_idx;
                            mx.total_before =
                                1024 + au_start48 as i128 * sena_core::XFI_MID_RATE as i128
                                    / SAMPLE_RATE as i128;
                            if au_start48 <= start_abs {
                                mx.trim = ((start_abs - au_start48)
                                    * sena_core::XFI_MID_RATE as i64
                                    / SAMPLE_RATE as i64)
                                    .max(0) as usize;
                                mx.trim_after = true;
                                mx.trim_pending = true;
                                mx.join48 = None;
                            } else {
                                mx.trim = 0;
                                mx.join48 = Some(au_start48);
                            }
                        }
                    }
                    st.mid_bytes += au.len() as u64;
                    Self::decode_mx_au(st, au, t_ns)?;
                }
            }
        }
        if let Some(au_index) = block.lf_au_index {
            for (i, au) in block.lf.iter().enumerate() {
                let au_idx = au_index as u64 + i as u64;
                if !st.lf_anchored {
                    // Cold start: anchor the LF lane on the first AU seen.
                    // The AU's grid start is reconstructed from (block ts,
                    // au_index); the same formula covers a true stream start
                    // (au_index 0 at ts 0), an absolute mid-stream start, and
                    // a cue-shifted start.
                    st.lf_anchored = true;
                    let au_start48 = lf_au_start48(au_idx, t_ns, st.lf_rate);
                    if au_idx == 0 && au_start48 < 0 {
                        // True stream start: trim the 1024-sample warmup
                        // without keeping it as resampler context.
                        st.lf_decode_from = 0;
                        st.lf_total_before = 0;
                        st.lf_trim = crate::pipeline::XHE_TRIM_CORE_SAMPLES;
                        st.lf_trim_after = false;
                        st.lf_trim_pending = true;
                    } else if au_payload_indep(au) {
                        st.lf_decode_from = au_idx;
                        st.lf_total_before = lf_anchor_core(au_start48, st.lf_rate);
                        // Decide against the CURRENT production point: the
                        // muted mid lane may already have run past the AU's
                        // start, in which case the covered part of the LF
                        // window is trimmed (its frames were emitted as
                        // zeros while muted and are unrecoverable).
                        let start_abs = (st.tf_seek_base + st.produced) as i64;
                        if au_start48 <= start_abs {
                            st.lf_trim = ((start_abs - au_start48) * st.lf_rate as i64
                                / SAMPLE_RATE as i64)
                                .max(0) as usize;
                            st.lf_trim_after = true;
                            st.lf_trim_pending = true;
                        } else {
                            // The AU starts in the future: silence until then.
                            st.lf_trim = 0;
                            st.lf_join48 = Some(au_start48);
                        }
                    } else {
                        // Not decodable without history: ramp (silence until
                        // the first independency AU).
                        st.lf_muted = true;
                        st.lf_decode_from = au_idx;
                    }
                }
                if au_idx < st.lf_decode_from {
                    continue;
                }
                if st.lf_muted {
                    if !au_payload_indep(au) {
                        continue;
                    }
                    // Ramp join: start LF decode at this independency AU.
                    let au_start48 = lf_au_start48(au_idx, t_ns, st.lf_rate);
                    let start_abs = (st.tf_seek_base + st.produced) as i64;
                    st.lf_muted = false;
                    st.lf_decode_from = au_idx;
                    st.lf_total_before = lf_anchor_core(au_start48, st.lf_rate);
                    if au_start48 <= start_abs {
                        st.lf_trim = ((start_abs - au_start48) * st.lf_rate as i64
                            / SAMPLE_RATE as i64)
                            .max(0) as usize;
                        st.lf_trim_after = true;
                        st.lf_trim_pending = true;
                        st.lf_join48 = None;
                    } else {
                        st.lf_trim = 0;
                        st.lf_join48 = Some(au_start48);
                    }
                }
                st.lf_bytes += au.len() as u64;
                Self::decode_lf_au(st, au, t_ns)?;
            }
        }
        for pkt in block.hf.iter() {
            let j = st.tf_next_pkt;
            st.tf_next_pkt += 1;
            if j < st.tf_decode_from {
                continue;
            }
            st.tf_bytes += pkt.len() as u64;
            let mut lane = OpusLane {
                dec: st
                    .opus2
                    .as_mut()
                    .ok_or_else(|| DecodeError::Format("top-band frame without a decoder".into()))?,
                name: "A_OPUSHF",
                errors: &mut st.tf_errors,
                raw: &mut st.tf_raw,
                infos: &mut st.tf_infos,
                total_before: &mut st.tf_total_before,
                scale: 3,
            };
            decode_opus_packet(&mut lane, pkt, t_ns)?;
        }
        Ok(())
    }

    fn decode_lf_au(st: &mut State, data: &[u8], t_ns: u64) -> Result<(), DecodeError> {
        // Decode into a scratch buffer so a panicking decoder can never
        // leave partial samples in the stream queue.
        let mut out: Vec<f32> = Vec::new();
        let result = catch_unwind(AssertUnwindSafe(|| st.xdec.decode_au(data, &mut out)));
        let failed = match result {
            Ok(Err(e)) => {
                eprintln!("warning: LF AU decode error ({t_ns}): {e}");
                true
            }
            Err(_) => {
                eprintln!("warning: LF AU decoder panic ({t_ns}); replacing with silence");
                true
            }
            Ok(Ok(())) => false,
        };
        if failed {
            st.lf_errors += 1;
            out.clear();
            let n = st.xdec.output_samples() * st.xdec.num_channels();
            out.resize(n, 0.0);
        }
        let samples = out.len() / st.xdec.num_channels();
        if samples == 0 || samples * st.xdec.num_channels() != out.len() {
            return Err(DecodeError::Codec(
                "rxaac returned a non-integral frame".into(),
            ));
        }
        if st.lf_zero_prefix > 0 {
            let n = st.lf_zero_prefix.min(samples);
            out[..n * 2].fill(0.0);
            st.lf_zero_prefix -= n;
        }
        st.lf_raw.extend(out);
        st.lf_infos.push(LfAuInfo {
            start_core: st.lf_total_before,
            samples_core: samples as i128,
            bits: (data.len() * 8) as u64,
        });
        st.lf_total_before += samples as i128;
        Ok(())
    }

    /// Decode one xHE mid AU into the lane's raw queue (mirror of
    /// `decode_lf_au` at the 44.1 kHz mid rate).
    fn decode_mx_au(st: &mut State, data: &[u8], t_ns: u64) -> Result<(), DecodeError> {
        let (start_core, samples) = {
            let mx = st.mx.as_mut().unwrap();
            let mut out: Vec<f32> = Vec::new();
            let result = catch_unwind(AssertUnwindSafe(|| mx.dec.decode_au(data, &mut out)));
            let failed = match result {
                Ok(Err(e)) => {
                    eprintln!("warning: mid AU decode error ({t_ns}): {e}");
                    true
                }
                Err(_) => {
                    eprintln!("warning: mid AU decoder panic ({t_ns}); replacing with silence");
                    true
                }
                Ok(Ok(())) => false,
            };
            if failed {
                mx.errors += 1;
                out.clear();
                let n = mx.dec.output_samples() * mx.dec.num_channels();
                out.resize(n, 0.0);
            }
            let samples = out.len() / mx.dec.num_channels();
            if samples == 0 || samples * mx.dec.num_channels() != out.len() {
                return Err(DecodeError::Codec(
                    "rxaac (mid) returned a non-integral frame".into(),
                ));
            }
            if mx.zero_prefix > 0 {
                let n = mx.zero_prefix.min(samples);
                out[..n * 2].fill(0.0);
                mx.zero_prefix -= n;
            }
            let start_core = mx.total_before;
            mx.raw.extend(out);
            mx.total_before += samples as i128;
            (start_core, samples)
        };
        // Bit accounting rides the mid (OpusPacketInfo) slot: positions in
        // 48 kHz playable units via the constant warmup shift.
        let rate = sena_core::XFI_MID_RATE as i128;
        st.opus_infos.push(OpusPacketInfo {
            start_48k: (start_core - crate::pipeline::XHE_TRIM_CORE_SAMPLES as i128)
                * SAMPLE_RATE as i128 / rate,
            samples_48k: samples as i128 * SAMPLE_RATE as i128 / rate,
            bits: (data.len() * 8) as u64,
        });
        Ok(())
    }

    fn produce_chunk(&mut self) -> Result<(), DecodeError> {
        let (lf_skip, hf_skip, trim_after, mx_skip, mx_trim_after) = {
            let st = self.state.as_ref().unwrap();
            let lf_trim = if st.lf_trim_pending { st.lf_trim } else { 0 };
            let hf_trim = if st.first { st.hf_trim } else { 0 };
            let (mx_trim, mx_after) = match st.mx.as_ref() {
                Some(mx) => (
                    if mx.trim_pending { mx.trim } else { 0 },
                    mx.trim_pending && mx.trim_after,
                ),
                None => (0, false),
            };
            (lf_trim, hf_trim, st.lf_trim_pending && st.lf_trim_after, mx_trim, mx_after)
        };
        let (ctx, guard, raw_empty) = {
            let st = self.state.as_ref().unwrap();
            // Fresh = buffered beyond the retained left context and the
            // one-time leading trim (at a seek the trim region is itself
            // kept as context). A muted LF lane (ramp before the join) is
            // excluded from readiness: output proceeds without it.
            let fresh_lf = (st.lf_raw.len() / 2).saturating_sub(st.lf_ctx + if trim_after {
                0
            } else {
                lf_skip
            });
            // Mid lane readiness: Opus mid = the direct 48 kHz queue; xHE
            // mid = the 44.1 kHz lane (muted/ramping mid doesn't gate).
            let fresh_mid_zero = match st.mx.as_ref() {
                Some(mx) => {
                    !mx.muted
                        && mx.anchored
                        && (mx.raw.len() / 2).saturating_sub(
                            mx.ctx + if mx_trim_after { 0 } else { mx_skip },
                        ) == 0
                }
                None => (st.hf_raw.len() / 2).saturating_sub(hf_skip) == 0,
            };
            let fresh_tf = st.opus2.is_none() || tf_emittable(st) > 0;
            (
                st.lf_ctx,
                st.lf_resampler.guard_frames(),
                (!st.lf_muted && st.lf_anchored && fresh_lf == 0) || fresh_mid_zero || !fresh_tf,
            )
        };
        if raw_empty {
            let st = self.state.as_mut().unwrap();
            if st.finished {
                // Trailing unmatched stream data after the shorter layer ended.
                st.lf_raw.clear();
                st.hf_raw.clear();
                st.tf_raw.clear();
                if let Some(mx) = st.mx.as_mut() {
                    mx.raw.clear();
                }
                st.lf_infos.clear();
                st.opus_infos.clear();
                st.tf_infos.clear();
                st.first = false;
                st.lf_trim_pending = false;
                if let Some(mx) = st.mx.as_mut() {
                    mx.trim_pending = false;
                }
            }
            return Ok(());
        }

        let has_tf = self.state.as_ref().unwrap().opus2.is_some();
        struct TfOut {
            shifted: Vec<f64>,
            j_start: usize,
        }
        struct LfWin {
            lf48: Vec<f64>,
            /// Absolute playable frame of lf48 index 0 (i64::MAX/2 = no LF).
            abs0: i64,
        }
        let (lwin, mxw, hf_after, tf_out, take, start, rate, lf_pass_core, lf_consume_core, mx_pass_core, straddle) = {
            let st = self.state.as_ref().unwrap();
            let resampler = &st.lf_resampler;
            // The resample window is the whole pending LF raw: retained left
            // context plus fresh samples. Dropping the context's outputs
            // afterwards keeps every emitted output on the whole-file phase
            // grid with complete kernel support (chunked decode then equals
            // the whole-file pipeline).
            let raw_drop = if trim_after { 0 } else { lf_skip };
            // Context inside the resample window (its outputs are dropped
            // after resampling): the retained left context, plus the
            // pre-target region after a seek.
            let window_front_core =
                ctx.max(raw_drop) - raw_drop + if trim_after { lf_skip } else { 0 };
            let front_core = raw_drop + window_front_core;
            let lf_after: Vec<f64> = st
                .lf_raw
                .iter()
                .skip(raw_drop * 2)
                .map(|&s| f64::from(s))
                .collect();
            let hf_after: Vec<f64> = st
                .hf_raw
                .iter()
                .skip(hf_skip * 2)
                .map(|&s| f64::from(s))
                .collect();
            // Top band: upsample the whole pending 16 kHz window to 48 kHz
            // and shift the baseband content back up to the 12 kHz+ band.
            // The carrier phase is locked to the playable timeline.
            let tf_out = if has_tf {
                let rs = st.tf_resampler.as_ref().unwrap();
                let win16: Vec<f64> = st.tf_raw.iter().map(|&s| f64::from(s)).collect();
                let up = rs.process(&win16, 2);
                let origin =
                    st.tf_seek_base as i64 + 3 * st.tf_base16 as i64 - st.tf_trim as i64;
                let shifted = sena_dsp::shift_up_offset(
                    &up,
                    2,
                    sena_core::HF_SPLIT_HZ,
                    SAMPLE_RATE,
                    origin,
                );
                let j_start =
                    st.produced as i128 + st.tf_trim as i128 - 3 * st.tf_base16 as i128;
                if j_start < 0 {
                    return Err(DecodeError::Format(
                        "top-band window underrun (negative emit index)".into(),
                    ));
                }
                Some(TfOut {
                    shifted,
                    j_start: j_start as usize,
                })
            } else {
                None
            };
            let lf48 = resampler.process(&lf_after, 2);
            let lf_off = resampler.output_frames(window_front_core);
            // Without a right guard the window tail would ring; emit only
            // fully-supported outputs until the LF stream is finished (the
            // zero-padded tail at EOF matches the whole-file decode).
            let window_core = lf_after.len() / 2;
            let right_cap = if st.finished {
                lf48.len() / 2
            } else {
                resampler.output_frames(window_core.saturating_sub(guard))
            };
            // LF coverage on the absolute playable timeline: normally the
            // window's emit point aligns with the chunk start; a ramp join
            // in the future makes the pre-join region silent.
            let start_abs = (st.tf_seek_base + st.produced) as i64;
            let join = st.lf_join48.filter(|&j| start_abs < j);
            let lf_abs0 = if st.lf_muted || !st.lf_anchored {
                i64::MAX / 2 // no LF yet: never caps, reads as silence
            } else if let Some(j) = join {
                j // the joined window starts here (lf_off == 0 at the join)
            } else {
                start_abs - lf_off as i64
            };
            let lf_cap = if lf_abs0 >= i64::MAX / 4 {
                usize::MAX
            } else {
                (lf_abs0 + right_cap as i64 - start_abs).max(0) as usize
            };
            // xHE mid window: whole pending raw resampled with retained
            // left context; emit capped at the fully-supported region.
            let mxw = st
                .mx
                .as_ref()
                .map(|mx| mx_window(mx.as_ref(), mx_skip, mx_trim_after, st.finished, start_abs));
            let mid_cap = mxw.as_ref().map_or(hf_after.len() / 2, |w| w.cap);
            let n = lf_cap
                .min(mid_cap)
                .min(if let Some(t) = &tf_out {
                    let w48 = t.shifted.len() / 2;
                    let cap = if st.finished {
                        w48
                    } else {
                        let rs = st.tf_resampler.as_ref().unwrap();
                        rs.output_frames((st.tf_raw.len() / 2).saturating_sub(rs.guard_frames()))
                            .saturating_sub(HILBERT_DELAY)
                    };
                    cap.saturating_sub(t.j_start)
                } else {
                    usize::MAX
                });
            let remaining = self
                .playable()
                .saturating_sub(st.tf_seek_base + st.produced);
            let mut take = (n as u64).min(remaining) as usize;
            // A chunk that straddles a future ramp join ends at the join:
            // the silent part needs no LF window, and the window stays
            // untouched for the next chunk (which then starts exactly at
            // the join and runs aligned). Same for a future mid-xHE join.
            let straddle_join = [join, mxw.as_ref().and_then(|w| w.join)]
                .into_iter()
                .flatten()
                .min()
                .map(|j| (j - start_abs) as usize);
            if let Some(limit) = straddle_join {
                take = take.min(limit);
            }
            if take == 0 {
                return Ok(());
            }
            // The rational ratio only represents some output counts exactly;
            // trim up to 2 frames from the chunk so the resampler can map the
            // consumed core count one-to-one (the frames are emitted by the
            // next chunk). With an xHE mid, `take` must be representable on
            // BOTH resamplers (LF and 44.1 kHz mid); moreover the mid lane
            // must keep chunk ends on the resampler's *output* phase grid
            // (multiples of n=160): only then do window boundaries preserve
            // the whole-file phase (16k/32k get this for free from the
            // representability loop). At stream end the window tail is
            // zero-padded anyway, so the final chunk emits everything left
            // regardless of representability (the bookkeeping is moot once
            // drained).
            let (consume_core, mx_consume_core) = if straddle_join.is_some() {
                (0, 0)
            } else if st.finished {
                let mxc = mxw
                    .as_ref()
                    .map_or(0, |w| w.window_core.saturating_sub(w.front_core));
                (window_core.saturating_sub(front_core), mxc)
            } else if let Some(mx) = st.mx.as_ref() {
                let ngrid = mx.resampler.phases();
                take -= take % ngrid;
                loop {
                    if take == 0 {
                        return Ok(());
                    }
                    if resampler.input_frames_for_output(take).is_some() {
                        break;
                    }
                    take -= ngrid;
                }
                let lf_k = resampler.input_frames_for_output(take).unwrap();
                let mx_k = mx.resampler.input_frames_for_output(take).unwrap();
                (lf_k, mx_k)
            } else {
                loop {
                    let lf_k = resampler.input_frames_for_output(take);
                    let mx_k = match st.mx.as_ref() {
                        Some(mx) => mx.resampler.input_frames_for_output(take),
                        None => Some(0),
                    };
                    if let (Some(a), Some(b)) = (lf_k, mx_k) {
                        break (a, b);
                    }
                    take -= 1;
                }
            };
            // Core samples passed over from the `lf_raw` head up to the new
            // fresh base: context/trim region plus the consumed fresh part.
            let fresh_cap = window_core.saturating_sub(window_front_core);
            let lf_consume_core = consume_core.min(fresh_cap);
            let lf_pass_core = front_core + lf_consume_core;
            let mx_pass_core = mxw.as_ref().map_or(0, |w| {
                w.front_core + mx_consume_core.min(w.window_core.saturating_sub(w.front_core))
            });
            let start = st.produced;
            let end = start + take as u64;
            let rate = self.chunk_bits(&st.lf_infos, &st.opus_infos, &st.tf_infos, start, end);
            (
                LfWin { lf48, abs0: lf_abs0 },
                mxw,
                hf_after,
                tf_out,
                take,
                start,
                rate,
                lf_pass_core,
                lf_consume_core,
                mx_pass_core,
                straddle_join.is_some(),
            )
        };

        let gain = 1.0 / sena_core::PRE_GAIN;
        let start_abs = (self.state.as_ref().unwrap().tf_seek_base + start) as i64;
        let mut chunk_out = Vec::with_capacity(take * 2);
        for i in 0..take {
            // LF window index on the absolute timeline (negative when a
            // ramp join is still in the future: silence there).
            let lf_i = start_abs + i as i64 - lwin.abs0;
            let (l, r) = if lf_i >= 0 && (lf_i as usize) < lwin.lf48.len() / 2 {
                (
                    lwin.lf48[lf_i as usize * 2] * gain,
                    lwin.lf48[lf_i as usize * 2 + 1] * gain,
                )
            } else {
                (0.0, 0.0)
            };
            let (hl, hr) = if let Some(mw) = &mxw {
                // xHE mid window index on the absolute timeline (like LF).
                let mi = start_abs + i as i64 - mw.abs0;
                if mi >= 0 && (mi as usize) < mw.s48.len() / 2 {
                    (
                        mw.s48[mi as usize * 2] * gain,
                        mw.s48[mi as usize * 2 + 1] * gain,
                    )
                } else {
                    (0.0, 0.0)
                }
            } else {
                (hf_after[i * 2] * gain, hf_after[i * 2 + 1] * gain)
            };
            let (tl, tr) = if let Some(t) = &tf_out {
                (
                    t.shifted[(t.j_start + i) * 2] * gain,
                    t.shifted[(t.j_start + i) * 2 + 1] * gain,
                )
            } else {
                (0.0, 0.0)
            };
            chunk_out.push(l + hl + tl);
            chunk_out.push(r + hr + tr);
        }

        {
            let st = self.state.as_mut().unwrap();
            st.output.extend(chunk_out);
            st.produced = start + take as u64;
            if !straddle {
                // Retain the last `lf_guard` core samples of the passed
                // region as the next chunk's left context; drop everything
                // older. The drain must stay on the resampler's decimation
                // grid (multiple of m): an off-grid window base would shift
                // all later outputs by a fraction of a frame against the
                // whole-file decode.
                let grid = st.lf_resampler.decimation();
                let fresh_drain = lf_pass_core.saturating_sub(st.lf_resampler.guard_frames());
                let drain_core = fresh_drain - fresh_drain % grid;
                st.lf_raw.drain(..drain_core * 2);
                st.lf_ctx = lf_pass_core - drain_core;
                // `lf_consume_core` bookkeeping for the bit accounting.
                Self::trim_lf_infos(&mut st.lf_infos, lf_consume_core as i128);
                // xHE mid lane: same drain discipline at 44.1 kHz, plus a
                // second invariant the LF rates get for free: the retained
                // context must stay a multiple of m (147) so its output span
                // (ctx*160/147) is an exact integer and `abs0` stays on the
                // n=160 output grid. Draining `pass - ctx_target` keeps both
                // grids locked (the window base then satisfies
                // (D-1024) ≡ 0 mod 147 automatically).
                if let Some(mx) = st.mx.as_mut() {
                    let grid = mx.resampler.decimation();
                    let ctx_target =
                        mx.resampler.guard_frames().div_ceil(grid) * grid;
                    let drain_core = mx_pass_core.saturating_sub(ctx_target);
                    mx.raw.drain(..drain_core * 2);
                    mx.ctx = mx_pass_core - drain_core;
                }
            }
            if st.mx.is_none() {
                st.hf_raw.drain(..(hf_skip + take) * 2);
            }
            if st.opus2.is_some() {
                // Drain only raw 16 kHz frames whose 48 kHz support is fully
                // behind the next emit point: the shift needs HILBERT_DELAY
                // frames of left context and the resampler its guard.
                let rs = st.tf_resampler.as_ref().unwrap();
                let j_next = (st.produced + st.tf_trim as u64) as i128 - 3 * st.tf_base16 as i128;
                let keep_from16 =
                    ((j_next - HILBERT_DELAY as i128) / 3 - rs.guard_frames() as i128 - 1).max(0)
                        as usize;
                let cur16 = st.tf_raw.len() / 2;
                let drain = keep_from16.min(cur16);
                if drain > 0 {
                    st.tf_raw.drain(..drain * 2);
                    st.tf_base16 += drain;
                }
            }
            Self::trim_opus_infos(&mut st.opus_infos, take as i128);
            Self::trim_opus_infos(&mut st.tf_infos, take as i128);
            st.first = false;
            st.lf_trim_pending = false;
            if let Some(mx) = st.mx.as_mut() {
                mx.trim_pending = false;
            }
        }

        let mut last_bits = *self.bits_prefix.last().unwrap();
        for v in rate.iter().skip(1) {
            last_bits += v;
            self.bits_prefix.push(last_bits);
        }
        Ok(())
    }

    fn chunk_bits(
        &self,
        lf_infos: &[LfAuInfo],
        opus_infos: &[OpusPacketInfo],
        tf_infos: &[OpusPacketInfo],
        start: u64,
        end: u64,
    ) -> Vec<f64> {
        // Lane infos are absolute (raw stream positions); the playable
        // mapping uses the constant leading trims. `base` is the absolute
        // playable frame of `start`.
        let st = self.state.as_ref().unwrap();
        let lf_rate = st.lf_rate;
        let base = (st.tf_seek_base + start) as i128;
        let n = (end - start) as usize;
        let mut delta = vec![0.0f64; n + 1];
        for info in lf_infos {
            let s_core = info.start_core - crate::pipeline::XHE_TRIM_CORE_SAMPLES as i128;
            let s48 = s_core * 48_000 / lf_rate as i128;
            let e48 = (s_core + info.samples_core) * 48_000 / lf_rate as i128;
            add_bits_interval(&mut delta, n as i128, s48 - base, e48 - base, info.bits as f64);
        }
        for (infos, skip) in [
            (opus_infos, self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip) as i128),
            (
                tf_infos,
                self.params.tf_head.as_ref().map_or(0, |h| h.pre_skip) as i128,
            ),
        ] {
            for info in infos {
                let s = info.start_48k - skip;
                let e = s + info.samples_48k;
                add_bits_interval(&mut delta, n as i128, s - base, e - base, info.bits as f64);
            }
        }
        let mut rate = vec![0.0f64; n + 1];
        let mut acc = 0.0;
        for i in 0..n {
            acc += delta[i];
            rate[i + 1] = acc;
        }
        rate
    }

    fn trim_lf_infos(infos: &mut Vec<LfAuInfo>, mut frames: i128) {
        let mut remove = 0usize;
        for info in infos.iter_mut() {
            if frames <= 0 {
                break;
            }
            if frames >= info.samples_core {
                frames -= info.samples_core;
                remove += 1;
            } else {
                info.start_core += frames;
                info.samples_core -= frames;
                frames = 0;
            }
        }
        if remove != 0 {
            infos.drain(..remove);
        }
    }

    fn trim_opus_infos(infos: &mut Vec<OpusPacketInfo>, mut frames: i128) {
        let mut remove = 0usize;
        for info in infos.iter_mut() {
            if frames <= 0 {
                break;
            }
            if frames >= info.samples_48k {
                frames -= info.samples_48k;
                remove += 1;
            } else {
                info.start_48k += frames;
                info.samples_48k -= frames;
                frames = 0;
            }
        }
        if remove != 0 {
            infos.drain(..remove);
        }
    }

    fn fill(&mut self, want: u64) -> Result<(), DecodeError> {
        if self.state.is_none() {
            self.state = Some(self.make_state()?);
        }
        loop {
            let (available, finished) = {
                let st = self.state.as_ref().unwrap();
                ((st.output.len() / 2) as u64, st.finished)
            };
            if available >= want || finished {
                return Ok(());
            }
            // Decode blocks until the shorter ready stream covers a chunk.
            loop {
                let (ready, eof) = {
                    let st = self.state.as_ref().unwrap();
                    let lf_ready = {
                        if st.lf_muted || !st.lf_anchored {
                            u64::MAX // ramp: LF joins later; do not gate on it
                        } else {
                            let trim = if st.lf_trim_pending {
                                st.lf_trim as i128
                            } else {
                                0
                            };
                            // The retained context and (until EOF) the right
                            // guard are resampler context, not emittable output.
                            let reserve = st.lf_ctx as i128
                                + if st.finished {
                                    0
                                } else {
                                    st.lf_resampler.guard_frames() as i128
                                };
                            let core = (st.lf_raw.len() / 2) as i128 - trim - reserve;
                            (core * 48_000 / st.lf_rate as i128).max(0) as u64
                        }
                    };
                    let hf_ready = {
                        match st.mx.as_ref() {
                            Some(mx) => {
                                // xHE mid lane: same readiness shape as LF.
                                if mx.muted || !mx.anchored {
                                    u64::MAX
                                } else {
                                    let trim = if mx.trim_pending { mx.trim as i128 } else { 0 };
                                    let reserve = mx.ctx as i128
                                        + if st.finished {
                                            0
                                        } else {
                                            mx.resampler.guard_frames() as i128
                                        };
                                    let core = (mx.raw.len() / 2) as i128 - trim - reserve;
                                    (core * 48_000 / sena_core::XFI_MID_RATE as i128).max(0) as u64
                                }
                            }
                            None => {
                                let trim = if st.first { st.hf_trim as i128 } else { 0 };
                                ((st.hf_raw.len() / 2) as i128 - trim).max(0) as u64
                            }
                        }
                    };
                    let tf_ready = tf_emittable(st).max(0) as u64;
                    let ready = lf_ready.min(hf_ready);
                    (
                        if st.opus2.is_some() {
                            ready.min(tf_ready)
                        } else {
                            ready
                        },
                        st.finished,
                    )
                };
                if ready >= MIN_CHUNK_FRAMES || eof {
                    break;
                }
                match self.feed_next()? {
                    Some((t_ns, data)) => self.decode_block(t_ns, &data)?,
                    None => {
                        if self.feed_exhausted() {
                            self.state.as_mut().unwrap().finished = true;
                            break;
                        }
                        // Push mode without queued data: nothing more to do
                        // until the host pushes again.
                        return Ok(());
                    }
                }
            }
            let before = (self.state.as_ref().unwrap().output.len() / 2) as u64;
            self.produce_chunk()?;
            let (after, eof) = {
                let st = self.state.as_ref().unwrap();
                ((st.output.len() / 2) as u64, st.finished)
            };
            if after == before && eof {
                return Ok(());
            }
        }
    }

    /// Pull decoded PCM: interleaved 48 kHz stereo f32 frames. Returns the
    /// number of frames written; 0 means EOF (container or host-finished
    /// push) or starvation (push mode — push more blocks and retry).
    pub fn pull(&mut self, out: &mut [f32], max_frames: u64) -> Result<u64, DecodeError> {
        // Push mode: the first pushed block anchors the timeline.
        if self.store.is_none() && !self.push_started {
            if let Some(&(t_ns, _)) = self.queue.front() {
                self.cold_start(t_ns)?;
            } else {
                return Ok(0);
            }
        }
        // Discard output preceding the seek target (push-mode seeks deliver
        // from an earlier cue point).
        if let Some(target) = self.output_start {
            while self.cursor < target {
                let avail = {
                    let st = self.state.as_ref().unwrap();
                    st.output.len() / 2
                };
                if avail == 0 {
                    self.fill(target - self.cursor)?;
                    let st = self.state.as_mut().unwrap();
                    if st.output.is_empty() {
                        break;
                    }
                    continue;
                }
                let st = self.state.as_mut().unwrap();
                let drop = (target - self.cursor).min(avail as u64) as usize;
                st.output.drain(..drop * 2);
                self.cursor += drop as u64;
            }
            if self.cursor >= target {
                self.output_start = None;
            }
        }
        let remaining = self.playable().saturating_sub(self.cursor);
        let want = max_frames.min(remaining);
        if out.len() < want as usize * 2 {
            return Err(DecodeError::Format("output buffer too small".into()));
        }
        if want == 0 {
            self.last_read = ReadInfo {
                start_frame: self.playable(),
                frames: 0,
                payload_bits: 0,
            };
            return Ok(0);
        }
        self.fill(want)?;
        let start = self.cursor;
        let bits_start = self.bits_prefix[(start - self.bits_base) as usize];
        let mut got = 0u64;
        {
            let st = self.state.as_mut().unwrap();
            while got < want {
                if st.output.is_empty() {
                    break;
                }
                out[(got * 2) as usize] = st.output.pop_front().unwrap() as f32;
                out[(got * 2 + 1) as usize] = st.output.pop_front().unwrap() as f32;
                got += 1;
            }
        }
        self.cursor += got;
        let bits_end = self.bits_prefix[(self.cursor - self.bits_base) as usize];
        self.last_read = ReadInfo {
            start_frame: start,
            frames: got,
            payload_bits: (bits_end - bits_start).max(0.0).round() as u64,
        };
        // Entries at/below the cursor can never be read again (a seek resets
        // the prefix), so rebase the vector onto the cursor after every read.
        let consumed = (self.cursor - self.bits_base) as usize;
        if consumed > 0 {
            self.bits_prefix.drain(..consumed);
            self.bits_base = self.cursor;
        }
        Ok(got)
    }

    pub fn read_info(&self) -> ReadInfo {
        self.last_read
    }

    pub fn position(&self) -> u64 {
        self.cursor
    }

    /// Cumulative per-layer payload bytes decoded (for stats displays).
    pub fn layer_bytes(&self) -> (u64, u64, u64) {
        match &self.state {
            Some(st) => (st.lf_bytes, st.mid_bytes, st.tf_bytes),
            None => (0, 0, 0),
        }
    }

    /// Composite blocks decoded so far.
    pub fn blocks_decoded(&self) -> u64 {
        self.state.as_ref().map_or(0, |st| st.blocks_decoded)
    }

    /// Cold start of the push feed at the block with timestamp `t_ns`
    /// (mid-stream after a demuxer seek, or at 0 for a full decode). With
    /// no warmup data the LF lane anchors on the first delivered AU
    /// (immediate trim when it is independent, otherwise the ramp).
    fn cold_start(&mut self, t_ns: u64) -> Result<(), DecodeError> {
        let b0 = t_ns / es::BLOCK_NS;
        let mut st = self.make_state()?;
        self.push_started = true;
        // The LF lane always anchors on the first delivered AU
        // (decode_block); the b0 == 0 stream-start trims for LF are
        // superseded by the (block ts, au_index) anchoring.
        st.lf_anchored = false;
        st.lf_trim = 0;
        st.lf_trim_pending = false;
        st.lf_zero_prefix = 0;
        // Same for an xHE mid lane: it anchors on the first delivered mid AU.
        if let Some(mx) = st.mx.as_mut() {
            mx.anchored = false;
            mx.trim = 0;
            mx.trim_pending = false;
            mx.zero_prefix = 0;
        }
        if b0 == 0 {
            // Stream start: the make_state defaults apply to the mid/top
            // lanes (pre-skip trims).
            self.state = Some(st);
            return Ok(());
        }
        let ps_mid = self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip as u64);
        let ps_tf = self.params.tf_head.as_ref().map_or(0, |h| h.pre_skip as u64);
        // Emit from the earliest covered playable frame of the delivered
        // layers: the mid/top packets in block b0 cover [b0*960 - ps, ...).
        let emit_start = b0 * es::BLOCK_FRAMES - ps_mid.min(ps_tf);
        st.tf_seek_base = emit_start;
        st.hf_trim = (ps_mid - ps_mid.min(ps_tf)) as usize;
        st.tf_trim = (ps_tf - ps_mid.min(ps_tf)) as usize;
        st.mid_decode_from = b0;
        st.tf_decode_from = b0;
        st.tf_next_pkt = b0;
        st.opus_total_before = (b0 * es::BLOCK_FRAMES) as i128;
        st.tf_total_before = (b0 * es::BLOCK_FRAMES) as i128;
        self.cursor = emit_start;
        self.bits_base = emit_start;
        self.bits_prefix = vec![0.0];
        self.state = Some(st);
        Ok(())
    }

    /// Container seek: jump to the block covering the LF anchor and decode
    /// forward with per-lane skip counters. LF jumps to the previous USAC
    /// independency AU (not sample-exact — LPD prev-frame state and
    /// free-running eSBR counters are format-inherent, see v1 notes); Opus
    /// packets are independent and jump directly by index; the top band
    /// backs off up to 8 packets for the upsample/shift context.
    pub fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        if frame > self.playable() {
            return Err(DecodeError::Format("seek past playable length".into()));
        }
        if self.store.is_none() {
            return Err(DecodeError::Unsupported(
                "seek without a block source; use warmup_seek or the ramp".into(),
            ));
        }
        let lf_rate = self.params.init.lf_rate;
        let n_blocks = self.store.as_ref().unwrap().blocks().len() as u64;
        // playable core c = f * lf_rate / 48000; the raw core at that point
        // is c + 1024 (one warmup AU was trimmed at the stream start).
        let c = (frame as u128 * lf_rate as u128 / 48_000) as u64;
        let target_raw_core = c + crate::pipeline::XHE_TRIM_CORE_SAMPLES as u64;
        let au = target_raw_core / crate::pipeline::XHE_TRIM_CORE_SAMPLES as u64;
        let mut start_au = self.prev_indep_au(au);
        // The LF resampler needs ~1000 core samples of context before the
        // target; step one independency further back when needed.
        if start_au == au && start_au > 0 {
            start_au = self.prev_indep_au(start_au - 1);
        }
        let lf_trim = (target_raw_core - start_au * 1024) as usize;

        let xhe_mid = self.params.mid_xhe_asc.is_some();
        let ps_mid = self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip as u64);
        // xHE mid lane: the LF AU math at the 44.1 kHz mid rate.
        let (mx_start_au, mx_trim) = if xhe_mid {
            let rate = sena_core::XFI_MID_RATE as u64;
            let c = (frame as u128 * rate as u128 / 48_000) as u64;
            let target_raw = c + crate::pipeline::XHE_TRIM_CORE_SAMPLES as u64;
            let au = target_raw / crate::pipeline::XHE_TRIM_CORE_SAMPLES as u64;
            let mut start_au = self.prev_indep_mx_au(au);
            if start_au == au && start_au > 0 {
                start_au = self.prev_indep_mx_au(start_au - 1);
            }
            (
                Some(start_au),
                Some((target_raw - start_au * 1024) as usize),
            )
        } else {
            (None, None)
        };
        let j0 = (frame + ps_mid) / es::BLOCK_FRAMES;
        let hf_trim = (frame + ps_mid - j0 * es::BLOCK_FRAMES) as usize;

        // Top layer: own packet index / pre-skip with up to 8 packets of
        // context back-off for the upsample/shift chain.
        let (tf_j0, tf_trim) = match self.params.tf_head.as_ref() {
            Some(h) => {
                let ps = h.pre_skip as u64;
                let j0 = (frame + ps) / es::BLOCK_FRAMES;
                let ctx = j0.min(8);
                let j0 = j0 - ctx;
                (j0, (frame + ps - j0 * es::BLOCK_FRAMES) as usize)
            }
            None => (0, 0),
        };

        // Block jump: the earliest lane anchor (LF AU by-end block, top
        // packet's block == its index, mid packet's block == its index or,
        // for an xHE mid, the mid AU's by-end block).
        let mid_block = match mx_start_au {
            Some(au) => es::xhe_block(au, sena_core::XFI_MID_RATE, n_blocks),
            None => j0,
        };
        let b0 = es::lf_block(start_au, lf_rate, n_blocks)
            .min(mid_block)
            .min(tf_j0)
            .min(n_blocks - 1);

        self.cursor = frame;
        self.bits_base = frame;
        self.bits_prefix = vec![0.0];
        self.last_read = ReadInfo::default();
        self.output_start = None;
        self.state = None;
        let mut st = self.make_state()?;
        st.lf_trim = lf_trim;
        st.hf_trim = hf_trim;
        st.tf_trim = tf_trim;
        st.tf_seek_base = frame;
        st.lf_decode_from = start_au;
        st.lf_total_before = start_au as i128 * 1024;
        if let (Some(mx), Some(start_au), Some(trim)) =
            (st.mx.as_mut(), mx_start_au, mx_trim)
        {
            mx.decode_from = start_au;
            mx.total_before = start_au as i128 * 1024;
            mx.trim = trim;
            mx.trim_after = true;
            mx.trim_pending = true;
            mx.zero_prefix = if start_au == 0 {
                crate::pipeline::XHE_TRIM_CORE_SAMPLES
            } else {
                0
            };
        }
        st.mid_decode_from = j0;
        st.opus_total_before = j0 as i128 * es::BLOCK_FRAMES as i128;
        st.tf_decode_from = tf_j0;
        st.tf_next_pkt = b0;
        st.tf_total_before = tf_j0 as i128 * es::BLOCK_FRAMES as i128;
        st.lf_trim_after = true;
        st.lf_zero_prefix = if start_au == 0 {
            crate::pipeline::XHE_TRIM_CORE_SAMPLES
        } else {
            0
        };
        self.store_index = b0 as usize;
        self.state = Some(st);
        Ok(())
    }

    /// Largest LF AU index <= `au` whose payload starts with the USAC
    /// independency flag (bit 7 of the first byte), i.e. a bit-exact random
    /// access point. AU 0 is by construction such a point. Scans the block
    /// index backwards without allocating.
    fn prev_indep_au(&self, au: u64) -> u64 {
        let store = self.store.as_ref().unwrap();
        let blocks = store.blocks();
        if blocks.is_empty() {
            return 0;
        }
        // AUs are consecutive in block order; estimate the block that could
        // contain `au` and walk backwards from there. AU 0 is independent by
        // construction, so the loop always terminates.
        let est = es::lf_block(au, self.params.init.lf_rate, blocks.len() as u64) as usize;
        for b in blocks[..=est.min(blocks.len() - 1)].iter().rev() {
            if let Some(ai) = b.au_index {
                for (i, &first) in b.lf_first.iter().take(b.lf_count as usize).enumerate().rev() {
                    let idx = ai as u64 + i as u64;
                    if idx <= au && first & INDEP_BIT != 0 {
                        return idx;
                    }
                }
            }
        }
        0
    }

    /// Largest xHE-mid AU index <= `au` flagged independent (mirror of
    /// `prev_indep_au` over the mid AU map cached by the demux index).
    fn prev_indep_mx_au(&self, au: u64) -> u64 {
        let store = self.store.as_ref().unwrap();
        let blocks = store.blocks();
        if blocks.is_empty() {
            return 0;
        }
        let est =
            es::xhe_block(au, sena_core::XFI_MID_RATE, blocks.len() as u64) as usize;
        for b in blocks[..=est.min(blocks.len() - 1)].iter().rev() {
            if let Some(ai) = b.mx_au_index {
                for (i, &first) in b.mx_first.iter().take(b.mx_count as usize).enumerate().rev() {
                    let idx = ai as u64 + i as u64;
                    if idx <= au && first & INDEP_BIT != 0 {
                        return idx;
                    }
                }
            }
        }
        0
    }
}

/// Start of xHE-mid AU `k`'s content on the 48 kHz grid of its arrival
/// block — the rational-rate generalization of `lf_au_start48` (44.1 kHz
/// does not divide 48 kHz, so per-AU durations alternate 1114/1115).
fn mx_au_start48(k: u64, block_ts_ns: u64) -> i64 {
    let rate = sena_core::XFI_MID_RATE;
    if k == 0 {
        return -(es::xhe_au_end48(1, rate) as i64);
    }
    let ts48 = (block_ts_ns / 1_000_000 * 48) as i64;
    let end48 = es::xhe_au_end48(k, rate) as i64;
    let dur = end48 - es::xhe_au_end48(k - 1, rate) as i64;
    let r = (end48 - 1).rem_euclid(es::BLOCK_FRAMES as i64) + 1;
    ts48 + r - dur
}

/// xHE mid resample window (same window discipline as the LF lane).
struct MxWin {
    s48: Vec<f64>,
    abs0: i64,
    cap: usize,
    window_core: usize,
    front_core: usize,
    join: Option<i64>,
}

/// Compute the mid lane's resample window. `#[inline(never)]`: its
/// temporaries then live in their own frame instead of inflating
/// `produce_chunk`'s (the fb2k decode thread stack is tight, see
/// `decode_fits_small_host_stack`).
#[inline(never)]
fn mx_window(
    mx: &MxLane,
    mx_skip: usize,
    mx_trim_after: bool,
    finished: bool,
    start_abs: i64,
) -> MxWin {
    let raw_drop = if mx_trim_after { 0 } else { mx_skip };
    // LF 同构：window_front_core = 窗口内的上下文；front_core = raw_drop +
    // window_front_core 才是"从 raw 头越过的总量"（drain 记账用它）。
    let window_front_core =
        mx.ctx.max(raw_drop) - raw_drop + if mx_trim_after { mx_skip } else { 0 };
    let front_core = raw_drop + window_front_core;
    let mid_after: Vec<f64> = mx
        .raw
        .iter()
        .skip(raw_drop * 2)
        .map(|&s| f64::from(s))
        .collect();
    let mx48 = mx.resampler.process(&mid_after, 2);
    let mx_off = mx.resampler.output_frames(window_front_core);
    let window_core = mid_after.len() / 2;
    let right_cap = if finished {
        mx48.len() / 2
    } else {
        mx.resampler
            .output_frames(window_core.saturating_sub(mx.resampler.guard_frames()))
    };
    let join = mx.join48.filter(|&j| start_abs < j);
    let abs0 = if mx.muted || !mx.anchored {
        i64::MAX / 2
    } else if let Some(j) = join {
        j
    } else {
        start_abs - mx_off as i64
    };
    let cap = if abs0 >= i64::MAX / 4 {
        usize::MAX
    } else {
        (abs0 + right_cap as i64 - start_abs).max(0) as usize
    };
    MxWin {
        s48: mx48,
        abs0,
        cap,
        window_core,
        front_core,
        join,
    }
}

fn au_payload_indep(au: &[u8]) -> bool {
    au.first().map_or(false, |b| b & INDEP_BIT != 0)
}

/// Start of LF AU `k`'s *content* on the 48 kHz grid of the block it
/// arrives in (`block_ts_ns`). AUs are placed by their playable END: AU k
/// (k >= 1) covers playable [(k-1)*1024, k*1024) samples at lf_rate, so its
/// content ends inside its block and its start is reconstructible from
/// (block ts, au_index) alone, whether the demuxer's timestamps are
/// absolute or shifted to the cue point. AU 0 is the warmup AU covering
/// [-1024, 1024) core samples (its preroll half is trimmed).
fn lf_au_start48(k: u64, block_ts_ns: u64, lf_rate: u32) -> i64 {
    let unit48 = es::lf_au_frames_48k(lf_rate) as i64;
    if k == 0 {
        return -unit48;
    }
    let ts48 = (block_ts_ns / 1_000_000 * 48) as i64;
    // The AU's content ends at k*unit48 = block_start48 + r, r in (0, 960].
    let end48 = k as i64 * unit48;
    let r = ((end48 - 1).rem_euclid(es::BLOCK_FRAMES as i64)) + 1;
    ts48 + r - unit48
}

/// LF lane bookkeeping anchor (bit accounting): the raw core position such
/// that (x - 1024) maps to the AU's grid start.
fn lf_anchor_core(au_start48: i64, lf_rate: u32) -> i128 {
    1024 + au_start48 as i128 * lf_rate as i128 / SAMPLE_RATE as i128
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::{self, Demuxed};

    fn asset_bytes(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/../../assets/e2e/{name}.sena",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    fn store_params(bytes: &[u8]) -> (EsParams, Vec<BlockRef>) {
        let indexed = demux::index_container(
            &mut |pos, len| demux::mem_read_at(bytes, pos, len),
            true,
        )
        .unwrap();
        let mut params = parse_params(indexed.init).unwrap();
        params.info.audio_span_bytes = indexed.audio_span_bytes;
        (params, indexed.blocks)
    }

    fn decode_all(dec: &mut EsDecoder) -> Vec<f32> {
        let mut pcm = Vec::new();
        let mut buf = vec![0.0f32; 4096 * 2];
        loop {
            let n = dec.pull(&mut buf, 4096).unwrap();
            if n == 0 {
                if dec.at_eof() {
                    break;
                }
                // push-mode starvation is not expected in these tests
                panic!("starved with {} queued", dec.queued());
            }
            pcm.extend_from_slice(&buf[..n as usize * 2]);
        }
        pcm
    }

    fn store_decoder(bytes: &[u8]) -> EsDecoder {
        let (params, blocks) = store_params(bytes);
        EsDecoder::open_store(
            params,
            BlockStore::Memory {
                bytes: bytes.to_vec(),
                blocks,
            },
        )
        .unwrap()
    }

    fn push_decoder(bytes: &[u8]) -> EsDecoder {
        let (params, blocks) = store_params(bytes);
        let mut dec = EsDecoder::open_push(params).unwrap();
        for b in &blocks {
            let data = &bytes[b.offset as usize..b.offset as usize + b.len as usize];
            dec.push(b.t_ns, data).unwrap();
        }
        dec.finish();
        dec
    }

    /// Push mode (host demuxes, feeds blocks) must equal container mode
    /// bit-for-bit.
    #[test]
    fn push_matches_container_decode() {
        for asset in [
            "01__p300__lfa__hf144",
            "01__p600__3t__hf160+96",
            "01__p600xfi__hf96",
        ] {
            let bytes = asset_bytes(asset);
            let reference = decode_all(&mut store_decoder(&bytes));
            let pushed = decode_all(&mut push_decoder(&bytes));
            assert_eq!(pushed.len(), reference.len(), "{asset}: length");
            assert_eq!(pushed, reference, "{asset}: push != container");
        }
    }

    /// Warmup over a shadow block source must equal a container seek
    /// bit-for-bit (it *is* the same decode, just fed differently).
    #[test]
    fn warmup_seek_matches_container_seek() {
        let bytes = asset_bytes("01__p600__3t__hf160+96");
        for target in [123_456u64, 640_000, 959_900] {
            let reference = {
                let mut d = store_decoder(&bytes);
                d.seek(target).unwrap();
                decode_all(&mut d)
            };
            let warmed = {
                let (params, blocks) = store_params(&bytes);
                let mut d = EsDecoder::open_push(params).unwrap();
                let shadow = BlockStore::Memory {
                    bytes: bytes.clone(),
                    blocks: blocks.clone(),
                };
                d.warmup_seek(shadow, target).unwrap();
                // Simulate a demuxer delivering from the 1 s cue at/below
                // the target: push every block from the cue on; the warmup
                // overlap is deduped by timestamp.
                let cue_block = (target * 1_000_000_000 / SAMPLE_RATE as u64) / 1_000_000_000 * 50;
                for b in &blocks[cue_block as usize..] {
                    let data = &bytes[b.offset as usize..b.offset as usize + b.len as usize];
                    d.push(b.t_ns, data).unwrap();
                }
                d.finish();
                decode_all(&mut d)
            };
            assert_eq!(warmed.len(), reference.len(), "target {target}: length");
            assert_eq!(warmed, reference, "target {target}: warmup != seek");
        }
    }

    /// Cold mid-stream start (no warmup): exact length from the emit point,
    /// mid layer decodes immediately, and the LF layer joins at the first
    /// independency AU (ramp).
    #[test]
    fn cold_start_ramp() {
        let bytes = asset_bytes("01__p300__lfa__hf144");
        let (params, blocks) = store_params(&bytes);
        let playable = params.info.playable_frames;
        let ps = params.mid_head.unwrap().pre_skip as u64;
        let ps_tf = params.tf_head.as_ref().map_or(0, |h| h.pre_skip as u64);
        let three = params.tf_head.is_some();
        let mut d = EsDecoder::open_push(params).unwrap();
        let b0 = 500u64; // 10 s in
        for b in &blocks[b0 as usize..] {
            let data = &bytes[b.offset as usize..b.offset as usize + b.len as usize];
            d.push(b.t_ns, data).unwrap();
        }
        d.finish();
        let emit_start = b0 * es::BLOCK_FRAMES - if three { ps.min(ps_tf) } else { 0 };
        let out = decode_all(&mut d);
        assert_eq!(out.len() as u64, (playable - emit_start) * 2);
        // Mid layer decoded from the start...
        let early_peak = out[..48000 * 2].iter().fold(0.0f32, |a, &v| a.max(v.abs()));
        assert!(early_peak > 1e-3, "mid layer silent at ramp start");
        // ...and the LF layer joins (indep spacing is ~0.5 s in this asset):
        // full-band content must appear within the first second.
        let later: Vec<f32> = out[48000 * 2..96000 * 2].to_vec();
        let later_peak = later.iter().fold(0.0f32, |a, &v| a.max(v.abs()));
        assert!(later_peak > 1e-2, "no audio after ramp join");
    }

    /// The streaming/container decoder must equal the whole-file pipeline:
    /// with the zero-phase resampler guard context retained across chunks
    /// there are no chunk-boundary seams.
    #[test]
    fn streaming_matches_whole_file_pipeline() {
        for asset in [
            "01__p300__lfa__hf144",
            "01__p600__lfd__hf136",
            "01__p600__3t__hf160+96",
            "05__p300__lfa__hf144",
            "09__p600__lfd__hf136",
            "01__p600xfi__hf96",
        ] {
            let bytes = asset_bytes(asset);
            let whole = crate::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
            let whole_pcm = whole.pcm_f64();
            let streamed = decode_all(&mut store_decoder(&bytes));
            assert_eq!(streamed.len(), whole_pcm.len(), "{asset}: length");
            let max = streamed
                .iter()
                .zip(whole_pcm.iter())
                .map(|(s, w)| (*s as f64 - *w as f32 as f64).abs())
                .fold(0.0f64, f64::max);
            assert!(
                max <= 1e-6,
                "{asset}: max streaming-vs-whole-file diff {max}"
            );
        }
    }

    /// Three-track seek: bit accounting stays bounded and mid-file seeks
    /// keep exact length within the documented LF inexactness class.
    #[test]
    fn three_track_seek() {
        let bytes = asset_bytes("01__p600__3t__hf160+96");
        let whole = crate::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
        let playable = whole.info().playable_frames as usize;

        // seek(0): bit-close to the continuous decode.
        let mut d = store_decoder(&bytes);
        d.seek(0).unwrap();
        let got = decode_all(&mut d);
        assert_eq!(got.len(), playable * 2);
        let max = got
            .iter()
            .zip(whole_pcm.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        assert!(max < 1e-4, "seek(0) vs continuous: {max}");

        // Mid-file seek: exact length; the diff stays within the LF seek
        // inexactness class.
        let target = (playable / 3) as u64;
        let mut d = store_decoder(&bytes);
        d.seek(target).unwrap();
        let got = decode_all(&mut d);
        assert_eq!(got.len(), (playable - target as usize) * 2);
        let settle = 24000; // 0.5 s
        let mut max = 0.0f32;
        for i in settle..playable - target as usize {
            for c in 0..2 {
                let diff = (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                max = max.max(diff);
            }
        }
        assert!(max < 1.1, "mid-file seek diff {max}");
    }

    /// xHE mid (v3): the three-track layout with a 44.1 kHz USAC mid lane
    /// decodes whole-file, and a mid-file seek keeps exact length and
    /// converges to the continuous decode after the ramp.
    #[test]
    fn xhe_mid_seek() {
        let bytes = asset_bytes("01__p600xfi__hf96");
        let whole = crate::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
        let playable = whole.info().playable_frames as usize;

        let mut d = store_decoder(&bytes);
        d.seek(0).unwrap();
        let got = decode_all(&mut d);
        assert_eq!(got.len(), playable * 2);
        let max = got
            .iter()
            .zip(whole_pcm.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0f32, f32::max);
        // The USAC lanes (LF + xHE mid) are not sample-exact across a
        // restart (decoder state re-converges); the Opus-mid class (~1e-4)
        // does not apply here.
        assert!(max < 5e-3, "seek(0) vs continuous: {max}");

        let target = (playable / 3) as u64;
        let mut d = store_decoder(&bytes);
        d.seek(target).unwrap();
        let got = decode_all(&mut d);
        assert_eq!(got.len(), (playable - target as usize) * 2);
        let settle = 48000; // 1 s: USAC lanes converge after the indep join
        let mut max = 0.0f32;
        for i in settle..playable - target as usize {
            for c in 0..2 {
                let diff = (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                max = max.max(diff);
            }
        }
        assert!(max < 1.1, "mid-file seek diff {max}");
    }

    /// The bit-accounting prefix must stay bounded to produced-but-unread
    /// frames over a full decode and across a seek.
    #[test]
    fn bits_prefix_stays_bounded() {
        let bytes = asset_bytes("01__p300__lfa__hf144");
        let mut dec = store_decoder(&bytes);
        let playable = dec.info().playable_frames;
        let mut buf = vec![0.0f32; 4096 * 2];
        let mut total = 0u64;
        let mut max_prefix = 0usize;
        loop {
            let n = dec.pull(&mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            max_prefix = max_prefix.max(dec.bits_prefix.len());
        }
        assert_eq!(total, playable);
        assert!(max_prefix <= 100_000, "bits_prefix grew to {max_prefix}");

        dec.seek(playable / 2).unwrap();
        loop {
            let n = dec.pull(&mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            max_prefix = max_prefix.max(dec.bits_prefix.len());
        }
        assert!(
            max_prefix <= 100_000,
            "bits_prefix grew to {max_prefix} after seek"
        );
    }

    /// Regression guard for the foobar2000 macOS stack overflow: the fb2k
    /// playback decoding thread has a 544 KiB stack, and decoder init +
    /// decode must fit comfortably inside it. Release builds pass at
    /// 448 KiB; debug builds need more headroom for the same code.
    #[test]
    fn decode_fits_small_host_stack() {
        for asset in ["01__p300__lfa__hf144", "01__p600__3t__hf160+96"] {
            let bytes = asset_bytes(asset);
            let handle = std::thread::Builder::new()
                // 448 KiB mirrors the foobar2000 decode thread's 544 KiB
                // budget (release); debug builds need more headroom for the
                // same code (the v3 xHE-mid lane widened debug frames; 512
                // KiB no longer fits, 640 does).
                .stack_size(if cfg!(debug_assertions) { 640 * 1024 } else { 448 * 1024 })
                .spawn(move || {
                    let mut d = store_decoder(&bytes);
                    let playable = d.info().playable_frames;
                    let mut buf = vec![0.0f32; 4096 * 2];
                    let mut total = 0u64;
                    loop {
                        let n = d.pull(&mut buf, 4096).unwrap();
                        if n == 0 {
                            break;
                        }
                        total += n;
                    }
                    assert_eq!(total, playable);
                })
                .unwrap();
            handle.join().unwrap();
        }
    }
}
