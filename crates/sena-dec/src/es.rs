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

use opus_decoder::{Channels, Decoder as OpusDecoder};
use rxaac_dec_lib::usac::UsacDecoder;
use sena_core::es;
use sena_core::es::Init;
use sena_dsp::{HILBERT_DELAY, Resampler};

use crate::demux::BlockRef;
use crate::pipeline::{
    DecodeError, DecodedInfo, ReadInfo, SAMPLE_RATE, XHE_TRIM_CORE_SAMPLES, add_bits_interval,
};

mod block_store;
mod opus_lane;
mod params;

pub use block_store::BlockStore;
pub use params::{EsParams, parse_params};

use opus_lane::{OpusLane, decode_opus_packet};

const MIN_CHUNK_FRAMES: u64 = 4800; // 100 ms @48k
/// USAC independency flag: bit 7 of the AU's first payload byte.
const INDEP_BIT: u8 = 0x80;

/// xHE mid lane state (v3). Field-for-field mirror of the LF lane's
/// trim/context/anchor machinery, at 44.1 kHz core rate.
/// One 44.1 kHz xHE-AAC lane's decode + window state: used by the mid
/// lane (resampled to the output rate) and, on 44.1 kHz-output streams, by
/// the top band (identity resampler + SSB shift at production). The USAC
/// restart bookkeeping (anchor trim classes, mid-stream preroll
/// normalization, content-timeline phase) is encoder-side behavior and
/// shared verbatim.
struct MxLane {
    dec: UsacDecoder,
    resampler: Resampler,
    raw: VecDeque<f32>,
    trim: usize,
    trim_pending: bool,
    trim_after: bool,
    anchored: bool,
    /// The pending trim was computed by seek() against a preroll-emitting
    /// (2048-sample) restart AU; decode_mx_au corrects it when the anchor
    /// AU proves to be a content-only (1024-sample) restart. Not set on the
    /// content-anchored join paths.
    trim_preroll_assumed: bool,
    muted: bool,
    join48: Option<i64>,
    decode_from: u64,
    ctx: usize,
    zero_prefix: usize,
    errors: usize,
    /// Total 44.1 kHz samples ever appended to `raw`; with
    /// `content_shift` this gives the content-timeline position of the
    /// raw-queue head (the resample window base), which the mid lane must
    /// hold on the m=147 decimation grid for phase consistency with the
    /// whole-file decode.
    total_appended: u64,
    /// Content-timeline position of the first sample ever appended to
    /// `raw` after the current anchor: set when the anchor AU is decoded
    /// (restart class dependent: a preroll-emitting restart starts one AU
    /// earlier than a content-only one). AU 0 anchors at -1024 (its
    /// warmup preroll).
    content_shift: i64,
}

/// Top layer (A_OPUSHF) decode chain: the 16 kHz Opus decoder and its
/// 16->48 kHz resampler are created (and replaced) together.
struct TopLane {
    dec: OpusDecoder,
    rs: Resampler,
}

struct State {
    lf_rate: u32,
    /// Leading output trims applied on the first produce after a (re)start:
    /// one 1024-sample core frame at stream start; at a seek, the raw
    /// samples from the jump AU up to the target frame.
    lf_trim: usize,
    /// The pending LF trim was computed by seek() against the AU grid
    /// (preroll-emitting restart assumed); decode_lf_au corrects it when
    /// the anchor proves content-only. False on the push/ramp paths whose
    /// trims are content-referenced.
    lf_trim_preroll_assumed: bool,
    mid_trim: usize,
    /// xHE mid lane (v3, 44.1 kHz): mirrors the LF lane (same warmup model:
    /// AU0 = 2048, trim 1024 core, steady 1024/AU; measured on Fraunhofer
    /// IIS CBR 128k streams via rxaac). None for an Opus mid. Boxed: a
    /// second UsacDecoder inline in State overflowed the foobar2000 decode
    /// thread's stack (see decode_fits_small_host_stack).
    mx: Option<Box<MxLane>>,
    /// xHE top band (v4, 44.1 kHz, xfi 288k): the same lane machinery as
    /// `mx` with an identity resampler; the 12 kHz SSB shift-back happens
    /// at production. None for an Opus (or absent) top band.
    tx: Option<Box<MxLane>>,
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
    /// LF lane content-timeline bookkeeping for the resample phase lock
    /// (only consulted on 44.1 kHz-output streams, where the LF decimation
    /// grid is 160/320 and trims can leave the window base off-grid; the
    /// 48 kHz paths hold the trivial m=1/2 grids by drain discipline).
    /// Same fields as the mid lane's: total core samples ever appended to
    /// `lf_raw`, and the content origin of the current anchor's emission.
    lf_total_appended: u64,
    lf_content_shift: i64,
    /// Core samples at the decode start to replace with zeros (seek anchored
    /// at AU 0: the warmup AU must not leak into the resampler context).
    lf_zero_prefix: usize,
    xdec: UsacDecoder,
    opus: OpusDecoder,
    top: Option<TopLane>,
    lf_raw: VecDeque<f32>,
    mid_raw: VecDeque<f32>,
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
    /// Per-block payload bits on the absolute playable timeline
    /// (block_start_frame, bits): the bitrate accounting spreads each
    /// composite block's payload uniformly over its 20 ms span — the
    /// display granularity is blocks anyway, and per-AU interval tracking
    /// drifted for non-block-aligned lanes (the 44.1 kHz xHE mid lost ~23%
    /// of its bits to window/trim desync).
    block_bits: std::collections::VecDeque<(u64, u64)>,
    lf_errors: usize,
    opus_errors: usize,
    tf_errors: usize,
    /// Cumulative payload bytes decoded per layer (stats display).
    lf_bytes: u64,
    mid_bytes: u64,
    tf_bytes: u64,
    blocks_decoded: u64,
    output: VecDeque<f64>,
    /// No chunk produced yet (the mid pre-skip trim applies to the first).
    first_chunk_pending: bool,
    produced: u64,
    finished: bool,
}

/// Core samples of the AU window preceding the current production point:
/// the lane trim when a mid-stream anchor/join starts before `start_abs`.
/// Core samples of the AU window preceding the production point, with
/// both timeline positions in output-rate units (48 kHz or 44.1 kHz).
fn trim_core_samples_out(start_abs: i64, au_start: i64, core_rate: u32, out_rate: u32) -> usize {
    ((start_abs - au_start) * core_rate as i64 / out_rate as i64).max(0) as usize
}

/// Emittable top-band output frames (48 kHz, playable timeline) from the
/// current `tf_raw` window: upsample the whole pending 16 kHz window, keep
/// only fully-supported outputs (resampler guard on the right unless the
/// stream is finished, Hilbert half-length for the shift), and count what
/// lies ahead of the current emit point. Negative means "need more input".
fn tf_emittable(st: &State) -> i128 {
    let Some(rs) = st.top.as_ref().map(|t| &t.rs) else {
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
    fn new_decoder(params: EsParams, store: Option<BlockStore>) -> Result<Self, DecodeError> {
        let warnings = params.warnings.clone();
        Ok(Self {
            params,
            store,
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

    /// Container mode: decode from a block store.
    pub fn open_store(params: EsParams, store: BlockStore) -> Result<Self, DecodeError> {
        Self::new_decoder(params, Some(store))
    }

    /// Push mode: the host pushes composite blocks in timeline order.
    pub fn open_push(params: EsParams) -> Result<Self, DecodeError> {
        Self::new_decoder(params, None)
    }

    pub fn info(&self) -> &DecodedInfo {
        &self.params.info
    }

    pub fn init(&self) -> &Init {
        &self.params.init
    }

    /// True when the feed is exhausted and no more output can be produced.
    pub fn at_eof(&self) -> bool {
        let drained = self
            .state
            .as_ref()
            .map_or(false, |st| st.output.is_empty());
        // The playable cap means no more output will ever be produced once
        // the cursor reaches it — even if tail-padding blocks remain
        // undecoded (the xfi tail pad makes those routine).
        (self.cursor >= self.playable() && drained)
            || (self.feed_exhausted()
                && self
                    .state
                    .as_ref()
                    .map_or(false, |st| st.finished && st.output.is_empty()))
    }

    /// Decoded output rate of this stream (44.1 kHz when every band is
    /// xHE-AAC at 44.1 kHz — the xfi 288k variant; 48 kHz otherwise).
    fn out_rate(&self) -> u32 {
        self.params.init.output_rate()
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
        let out_rate = self.params.init.output_rate();
        let top = match self.params.tf_head.as_ref() {
            Some(head) => {
                let mut dec = OpusDecoder::new(sena_core::HF_TRACK_RATE, Channels::Stereo)
                    .map_err(|e| DecodeError::Codec(format!("opus init (top): {e}")))?;
                if head.output_gain != 0 {
                    dec.set_gain(i32::from(head.output_gain))
                        .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
                }
                Some(TopLane {
                    dec,
                    rs: Resampler::new(sena_core::HF_TRACK_RATE, SAMPLE_RATE),
                })
            }
            None => None,
        };
        // xHE top band (v4): a second USAC lane at 44.1 kHz with an
        // identity resampler (the lane rate IS the output rate).
        let tx = match &self.params.tf_xhe_asc {
            Some(asc) => {
                let dec = UsacDecoder::new(asc)
                    .map_err(|e| DecodeError::Codec(format!("rxaac init (top): {e}")))?;
                if dec.num_channels() != 2 {
                    return Err(DecodeError::Unsupported("rxaac top channels != 2".into()));
                }
                if dec.output_rate() != sena_core::XFI_MID_RATE {
                    return Err(DecodeError::Format(format!(
                        "rxaac top output rate {} != {}",
                        dec.output_rate(),
                        sena_core::XFI_MID_RATE
                    )));
                }
                Some(Box::new(MxLane {
                    dec,
                    resampler: Resampler::new(sena_core::XFI_MID_RATE, sena_core::XFI_MID_RATE),
                    raw: VecDeque::new(),
                    trim: XHE_TRIM_CORE_SAMPLES,
                    trim_pending: true,
                    trim_after: false,
                    anchored: true,
                    trim_preroll_assumed: false,
                    muted: false,
                    join48: None,
                    decode_from: 0,
                    ctx: 0,
                    zero_prefix: 0,
                    errors: 0,
                    total_appended: 0,
                    content_shift: 0,
                }))
            }
            None => None,
        };
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
                    resampler: Resampler::new_with_passband(
                        sena_core::XFI_MID_RATE,
                        out_rate,
                        sena_core::XFI_MID_RESAMPLE_PASS_HZ,
                    ),
                    raw: VecDeque::new(),
                    trim: XHE_TRIM_CORE_SAMPLES,
                    trim_pending: true,
                    trim_after: false,
                    anchored: true,
                    trim_preroll_assumed: false,
                    muted: false,
                    join48: None,
                    decode_from: 0,
                    ctx: 0,
                    zero_prefix: 0,
                    errors: 0,
                    total_appended: 0,
                    content_shift: 0,
                }))
            }
            None => None,
        };
        Ok(State {
            lf_rate: self.params.init.lf_rate,
            lf_trim: XHE_TRIM_CORE_SAMPLES,
            lf_trim_preroll_assumed: false,
            mid_trim: self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip as usize),
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
            lf_resampler: match out_rate {
                sena_core::XFI_MID_RATE => Resampler::new_with_passband(
                    self.params.init.lf_rate,
                    out_rate,
                    1_000.0,
                ),
                _ => Resampler::new(self.params.init.lf_rate, out_rate),
            },
            lf_ctx: 0,
            lf_total_appended: 0,
            lf_content_shift: 0,
            lf_zero_prefix: 0,
            xdec,
            opus,
            top,
            tx,
            lf_raw: VecDeque::new(),
            mid_raw: VecDeque::new(),
            tf_raw: VecDeque::new(),
            tf_base16: 0,
            tf_seek_base: 0,
            block_bits: std::collections::VecDeque::new(),
            lf_errors: 0,
            opus_errors: 0,
            tf_errors: 0,
            lf_bytes: 0,
            mid_bytes: 0,
            tf_bytes: 0,
            blocks_decoded: 0,
            output: VecDeque::new(),
            first_chunk_pending: true,
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
        es::decode_block(data, self.params.init.mid_codec, self.params.init.hf_codec)
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
        let block = es::decode_block(payload, self.params.init.mid_codec, self.params.init.hf_codec)
            .map_err(|e| DecodeError::Format(e.to_string()))?;
        let out_rate = self.params.init.output_rate();
        let st = self.state.as_mut().unwrap();
        st.blocks_decoded += 1;
        // Bit accounting: the block's whole payload spread uniformly over
        // its 20 ms span (see State::block_bits).
        st.block_bits.push_back((
            t_ns / es::BLOCK_NS * es::block_frames_out(out_rate),
            payload.len() as u64 * 8,
        ));
        match block.mid {
            es::MidSection::Opus(pkt) => {
                let mid_pkt = t_ns / es::BLOCK_NS;
                if mid_pkt >= st.mid_decode_from {
                    st.mid_bytes += pkt.len() as u64;
                    let mut lane = OpusLane {
                        dec: &mut st.opus,
                        name: "Opus",
                        errors: &mut st.opus_errors,
                        raw: &mut st.mid_raw,
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
                            let au_start48 = mx_au_start_out(au_idx, t_ns, out_rate);
                            if au_idx == 0 && au_start48 < 0 {
                                mx.decode_from = 0;
                                mx.trim = XHE_TRIM_CORE_SAMPLES;
                                mx.trim_after = false;
                                mx.trim_pending = true;
                            } else if au_payload_indep(au) {
                                mx.decode_from = au_idx;
                                let start_abs = (st.tf_seek_base + st.produced) as i64;
                                if au_start48 <= start_abs {
                                    mx.trim = trim_core_samples_out(
                                        start_abs,
                                        au_start48,
                                        sena_core::XFI_MID_RATE,
                                        out_rate,
                                    );
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
                            let au_start48 = mx_au_start_out(au_idx, t_ns, out_rate);
                            let start_abs = (st.tf_seek_base + st.produced) as i64;
                            mx.muted = false;
                            mx.decode_from = au_idx;
                            if au_start48 <= start_abs {
                                mx.trim = trim_core_samples_out(
                                    start_abs,
                                    au_start48,
                                    sena_core::XFI_MID_RATE,
                                    out_rate,
                                );
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
                    Self::decode_mx_au(st, au_idx, au, t_ns)?;
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
                    let au_start48 = lf_au_start_out(au_idx, t_ns, st.lf_rate, out_rate);
                    if au_idx == 0 && au_start48 < 0 {
                        // True stream start: trim the 1024-sample warmup
                        // without keeping it as resampler context.
                        st.lf_decode_from = 0;
                        st.lf_trim = XHE_TRIM_CORE_SAMPLES;
                        st.lf_trim_after = false;
                        st.lf_trim_pending = true;
                    } else if au_payload_indep(au) {
                        st.lf_decode_from = au_idx;
                        // Decide against the CURRENT production point: the
                        // muted mid lane may already have run past the AU's
                        // start, in which case the covered part of the LF
                        // window is trimmed (its frames were emitted as
                        // zeros while muted and are unrecoverable).
                        let start_abs = (st.tf_seek_base + st.produced) as i64;
                        if au_start48 <= start_abs {
                            st.lf_trim = trim_core_samples_out(start_abs, au_start48, st.lf_rate, out_rate);
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
                    let au_start48 = lf_au_start_out(au_idx, t_ns, st.lf_rate, out_rate);
                    let start_abs = (st.tf_seek_base + st.produced) as i64;
                    st.lf_muted = false;
                    st.lf_decode_from = au_idx;
                    if au_start48 <= start_abs {
                        st.lf_trim = trim_core_samples_out(start_abs, au_start48, st.lf_rate, out_rate);
                        st.lf_trim_after = true;
                        st.lf_trim_pending = true;
                        st.lf_join48 = None;
                    } else {
                        st.lf_trim = 0;
                        st.lf_join48 = Some(au_start48);
                    }
                }
                st.lf_bytes += au.len() as u64;
                Self::decode_lf_au(st, au_idx, au, t_ns)?;
            }
        }
        if let Some(au_index) = block.hf_au_index {
            // xHE top band (v4): the same anchor/ramp discipline as the
            // xHE mid lane, at the 44.1 kHz lane rate.
            for (i, au) in block.hf.iter().enumerate() {
                let au_idx = au_index as u64 + i as u64;
                {
                    let tx = st.tx.as_mut().unwrap();
                    if !tx.anchored {
                        tx.anchored = true;
                        let au_start = mx_au_start_out(au_idx, t_ns, out_rate);
                        if au_idx == 0 && au_start < 0 {
                            tx.decode_from = 0;
                            tx.trim = XHE_TRIM_CORE_SAMPLES;
                            tx.trim_after = false;
                            tx.trim_pending = true;
                        } else if au_payload_indep(au) {
                            tx.decode_from = au_idx;
                            let start_abs = (st.tf_seek_base + st.produced) as i64;
                            if au_start <= start_abs {
                                tx.trim = trim_core_samples_out(
                                    start_abs,
                                    au_start,
                                    sena_core::XFI_MID_RATE,
                                    out_rate,
                                );
                                tx.trim_after = true;
                                tx.trim_pending = true;
                            } else {
                                tx.trim = 0;
                                tx.join48 = Some(au_start);
                            }
                        } else {
                            tx.muted = true;
                            tx.decode_from = au_idx;
                        }
                    }
                    if au_idx < tx.decode_from {
                        continue;
                    }
                    if tx.muted {
                        if !au_payload_indep(au) {
                            continue;
                        }
                        let au_start = mx_au_start_out(au_idx, t_ns, out_rate);
                        let start_abs = (st.tf_seek_base + st.produced) as i64;
                        tx.muted = false;
                        tx.decode_from = au_idx;
                        if au_start <= start_abs {
                            tx.trim = trim_core_samples_out(
                                start_abs,
                                au_start,
                                sena_core::XFI_MID_RATE,
                                out_rate,
                            );
                            tx.trim_after = true;
                            tx.trim_pending = true;
                            tx.join48 = None;
                        } else {
                            tx.trim = 0;
                            tx.join48 = Some(au_start);
                        }
                    }
                }
                st.tf_bytes += au.len() as u64;
                Self::decode_usac_au(st.tx.as_mut().unwrap(), "top", au_idx, au, t_ns)?;
            }
        } else {
            for pkt in block.hf.iter() {
                let j = st.tf_next_pkt;
                st.tf_next_pkt += 1;
                if j < st.tf_decode_from {
                    continue;
                }
                st.tf_bytes += pkt.len() as u64;
                let mut lane = OpusLane {
                    dec: st
                        .top
                        .as_mut()
                        .map(|t| &mut t.dec)
                        .ok_or_else(|| DecodeError::Format("top-band frame without a decoder".into()))?,
                    name: "A_OPUSHF",
                    errors: &mut st.tf_errors,
                    raw: &mut st.tf_raw,
                };
                decode_opus_packet(&mut lane, pkt, t_ns)?;
            }
        }
        Ok(())
    }

    fn decode_lf_au(st: &mut State, au_idx: u64, data: &[u8], t_ns: u64) -> Result<(), DecodeError> {
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
        // Anchor restart class (same as the mid lane's trim-class fix):
        // the *seek* LF trim formula assumes a preroll-emitting (2048)
        // restart; a content-only (1024) anchor means the queue holds one
        // AU less than the formula accounts for, so the trim must skip one
        // less. Only the seek formula carries the preroll assumption — the
        // push/ramp anchoring trims are content-referenced already, so the
        // fix is gated on `lf_trim_preroll_assumed`.
        if au_idx == st.lf_decode_from
            && au_idx != 0
            && st.lf_trim_pending
            && st.lf_trim_preroll_assumed
            && samples == XHE_TRIM_CORE_SAMPLES
        {
            st.lf_trim = st.lf_trim.saturating_sub(XHE_TRIM_CORE_SAMPLES);
        }
        // Mid-stream preroll hazard, same class as the xHE mid lane but
        // OPPOSITE layout: on a diverged post-restart trajectory the FhG
        // decoder emits [content 1024][extra 1024] here (verified by direct
        // comparison against the continuous decode's same AU), so the tail
        // is dropped. (The mid lane's 2048 emission is [preroll][content]
        // and drops the head — the two lanes' restart paths differ.)
        let mut out = out;
        if au_idx != st.lf_decode_from
            && samples == 2 * XHE_TRIM_CORE_SAMPLES
            && st.lf_trim_preroll_assumed
        {
            out.truncate(XHE_TRIM_CORE_SAMPLES * 2);
        }
        // Content-timeline bookkeeping (the 44.1 kHz-output phase lock;
        // inert on 48 kHz streams). Anchor class mirrors the mid lane: a
        // preroll-emitting restart starts one AU before the content.
        if au_idx == st.lf_decode_from {
            st.lf_content_shift = au_idx as i64 * XHE_TRIM_CORE_SAMPLES as i64
                - if samples == XHE_TRIM_CORE_SAMPLES {
                    0
                } else {
                    XHE_TRIM_CORE_SAMPLES as i64
                };
        }
        st.lf_total_appended += (out.len() / 2) as u64;
        st.lf_raw.extend(out);
        Ok(())
    }

    /// Decode one xHE mid AU into the lane's raw queue (mirror of
    /// `decode_lf_au` at the 44.1 kHz mid rate).
    fn decode_mx_au(st: &mut State, au_idx: u64, data: &[u8], t_ns: u64) -> Result<(), DecodeError> {
        Self::decode_usac_au(st.mx.as_mut().unwrap(), "mid", au_idx, data, t_ns)
    }

    /// Decode one AU of a 44.1 kHz xHE lane (mid or top band) with the
    /// shared USAC restart bookkeeping (anchor trim classes, zero prefix,
    /// mid-stream preroll normalization, content-timeline accounting).
    fn decode_usac_au(
        mx: &mut MxLane,
        what: &'static str,
        au_idx: u64,
        data: &[u8],
        t_ns: u64,
    ) -> Result<(), DecodeError> {
        {
            let mx = &mut *mx;
            let mut out: Vec<f32> = Vec::new();
            let result = catch_unwind(AssertUnwindSafe(|| mx.dec.decode_au(data, &mut out)));
            let failed = match result {
                Ok(Err(e)) => {
                    eprintln!("warning: {what} AU decode error ({t_ns}): {e}");
                    true
                }
                Err(_) => {
                    eprintln!("warning: {what} AU decoder panic ({t_ns}); replacing with silence");
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
                return Err(DecodeError::Codec(format!(
                    "rxaac ({what}) returned a non-integral frame"
                )));
            }
            // The seek trim was computed against a preroll-emitting restart
            // (2048 samples, like AU 0). Some independency AUs emit only
            // their 1024 content samples on restart, with no preroll —
            // there the trim would skip one AU too much (measured: content
            // lands exactly one AU late). Detect the class by the actual
            // emitted length and shift the pending trim back one AU.
            if au_idx == mx.decode_from {
                // Content-timeline origin of this restart's emission:
                // a preroll-emitting restart covers the anchor AU's
                // content plus one AU of preroll before it; a content-only
                // restart starts exactly at the content.
                mx.content_shift = au_idx as i64
                    * XHE_TRIM_CORE_SAMPLES as i64
                    - if samples == XHE_TRIM_CORE_SAMPLES {
                        0
                    } else {
                        XHE_TRIM_CORE_SAMPLES as i64
                    };
            }
            if au_idx == mx.decode_from
                && au_idx != 0
                && mx.trim_pending
                && mx.trim_preroll_assumed
                && samples == XHE_TRIM_CORE_SAMPLES
            {
                mx.trim = mx.trim.saturating_sub(XHE_TRIM_CORE_SAMPLES);
            }
            if mx.zero_prefix > 0 {
                let n = mx.zero_prefix.min(samples);
                out[..n * 2].fill(0.0);
                mx.zero_prefix -= n;
            }
            // A non-anchor AU must contribute exactly its 1024 content
            // samples to the timeline. With a diverged post-restart state
            // the codec may emit a mid-stream preroll (2048 = preroll +
            // content) at a later independency AU; if those 1024 extra
            // samples enter the queue, every later sample shifts one AU
            // off the playable timeline (permanently, silently). Drop the
            // preroll; the anchor AU itself is handled by the trim/class
            // logic above. Gated on the seek path (trim_preroll_assumed):
            // the push cold-start's coverage bookkeeping predates this
            // normalization and needs its own audit first.
            let mut out = out;
            let mut samples = samples;
            if au_idx != mx.decode_from
                && mx.trim_preroll_assumed
                && samples == 2 * XHE_TRIM_CORE_SAMPLES
            {
                out.drain(..XHE_TRIM_CORE_SAMPLES * 2);
                samples = XHE_TRIM_CORE_SAMPLES;
            }
            mx.total_appended += samples as u64;
            mx.raw.extend(out);
            samples
        };

        Ok(())
    }

    fn produce_chunk(&mut self) -> Result<(), DecodeError> {
        let out_rate = self.params.init.output_rate();
        let (lf_skip, mid_skip, trim_after, mx_skip, mx_trim_after, tx_skip, tx_trim_after) = {
            let st = self.state.as_ref().unwrap();
            let lf_trim = if st.lf_trim_pending { st.lf_trim } else { 0 };
            let mid_trim = if st.first_chunk_pending { st.mid_trim } else { 0 };
            let (mx_trim, mx_after) = match st.mx.as_ref() {
                Some(mx) => (
                    if mx.trim_pending { mx.trim } else { 0 },
                    mx.trim_pending && mx.trim_after,
                ),
                None => (0, false),
            };
            let (tx_trim, tx_after) = match st.tx.as_ref() {
                Some(tx) => (
                    if tx.trim_pending { tx.trim } else { 0 },
                    tx.trim_pending && tx.trim_after,
                ),
                None => (0, false),
            };
            (lf_trim, mid_trim, st.lf_trim_pending && st.lf_trim_after, mx_trim, mx_after, tx_trim, tx_after)
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
                None => (st.mid_raw.len() / 2).saturating_sub(mid_skip) == 0,
            };
            let fresh_tf = match st.tx.as_ref() {
                // xHE top: same readiness shape as the xHE mid lane.
                Some(tx) => {
                    tx.muted
                        || !tx.anchored
                        || (tx.raw.len() / 2).saturating_sub(
                            tx.ctx + if tx_trim_after { 0 } else { tx_skip },
                        ) > 0
                }
                None => st.top.is_none() || tf_emittable(st) > 0,
            };
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
                st.mid_raw.clear();
                st.tf_raw.clear();
                if let Some(mx) = st.mx.as_mut() {
                    mx.raw.clear();
                }
                if let Some(tx) = st.tx.as_mut() {
                    tx.raw.clear();
                }
                st.block_bits.clear();
                st.first_chunk_pending = false;
                st.lf_trim_pending = false;
                if let Some(mx) = st.mx.as_mut() {
                    mx.trim_pending = false;
                }
                if let Some(tx) = st.tx.as_mut() {
                    tx.trim_pending = false;
                }
            }
            return Ok(());
        }

        // (production lives in `top_window`; see its doc for the frame
        // budget reason)
        struct LfWin {
            lf48: Vec<f64>,
            /// Absolute playable frame of lf48 index 0 (i64::MAX/2 = no LF).
            abs0: i64,
        }
        let (lwin, mxw, mid_after, tf_out, take, start, rate, lf_pass_core, mx_pass_core, tx_pass_core, straddle) = {
            let st = self.state.as_ref().unwrap();
            let resampler = &st.lf_resampler;
            let start_abs = (st.tf_seek_base + st.produced) as i64;
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
            // 44.1 kHz-output streams: the LF decimation grid is 160/320
            // and AU-quantized trims (1024 % 160 != 0) can leave the window
            // base off the whole-file phase grid after a seek. Same
            // correction as the mid lane's (which the 48 kHz paths never
            // needed: their m=1/2 grids are held by drain discipline). The
            // emit content mapping is preserved (front_core is unchanged).
            let (raw_drop, window_front_core) = if out_rate == sena_core::XFI_MID_RATE {
                let grid = resampler.decimation() as i64;
                let base_content = st.lf_content_shift + st.lf_total_appended as i64
                    - (st.lf_raw.len() / 2) as i64
                    + raw_drop as i64;
                let pc = (grid - base_content.rem_euclid(grid)).rem_euclid(grid) as usize;
                if pc > 0 && window_front_core >= pc {
                    (raw_drop + pc, window_front_core - pc)
                } else {
                    (raw_drop, window_front_core)
                }
            } else {
                (raw_drop, window_front_core)
            };
            let front_core = raw_drop + window_front_core;

            let mid_after: Vec<f64> = st
                .mid_raw
                .iter()
                .skip(mid_skip * 2)
                .map(|&s| f64::from(s))
                .collect();
            // Top band: production (xHE identity window + shift, or the
            // Opus 16 kHz upsample + shift) lives in `top_window`.
            let (txw, tf_out) = top_window(
                st.tx.as_deref(),
                st.top.as_ref(),
                &st.tf_raw,
                st.tf_base16,
                st.tf_trim,
                st.tf_seek_base,
                st.produced,
                st.finished,
                tx_skip,
                tx_trim_after,
                start_abs,
            )?;
            let join = st.lf_join48.filter(|&j| start_abs < j);
            // xHE mid window: whole pending raw resampled with retained
            // left context; emit capped at the fully-supported region.
            let mxw = st
                .mx
                .as_ref()
                .map(|mx| mx_window(mx.as_ref(), mx_skip, mx_trim_after, st.finished, start_abs));
            let mid_cap = mxw.as_ref().map_or(mid_after.len() / 2, |w| w.cap);
            let tf_cap = if let Some(t) = &tf_out {
                if let Some(w) = &txw {
                    // xHE top: the mx_window cap is chunk-relative; the
                    // shift additionally needs HILBERT_DELAY of right
                    // support (the finished tail is zero-padded anyway).
                    if st.finished {
                        w.cap
                    } else {
                        w.cap.saturating_sub(HILBERT_DELAY)
                    }
                } else {
                    let w48 = t.shifted.len() / 2;
                    let cap = if st.finished {
                        w48
                    } else {
                        let rs = &st.top.as_ref().expect("tf_out implies a top chain").rs;
                        rs.output_frames((st.tf_raw.len() / 2).saturating_sub(rs.guard_frames()))
                            .saturating_sub(HILBERT_DELAY)
                    };
                    cap.saturating_sub(t.j_start.max(0) as usize)
                }
            } else {
                usize::MAX
            };
            let remaining = self
                .playable()
                .saturating_sub(st.tf_seek_base + st.produced);
            // LF: resample only the window this chunk can emit. The output
            // phase grid anchors at the retained-context boundary and every
            // emitted output keeps full kernel support (the guard stays
            // inside the window), so the bounded window is bit-identical
            // to the whole-pending one while skipping work that would be
            // discarded (2.65x overhead measured).
            let lf_off = resampler.output_frames(window_front_core);
            let pending_full = (st.lf_raw.len() / 2).saturating_sub(raw_drop);
            let lf_fresh_bound = if st.finished {
                usize::MAX
            } else {
                resampler
                    .output_frames(pending_full.saturating_sub(guard))
                    .saturating_sub(lf_off)
            };
            let take_est = mid_cap
                .min(tf_cap)
                .min(remaining as usize)
                .min(lf_fresh_bound);
            let window_target = if st.finished {
                usize::MAX
            } else {
                match resampler.input_frames_for_output(lf_off + take_est) {
                    Some(k) => k + guard,
                    None => usize::MAX,
                }
            };
            let lf_after: Vec<f64> = st
                .lf_raw
                .iter()
                .skip(raw_drop * 2)
                .take(pending_full.min(window_target) * 2)
                .map(|&s| f64::from(s))
                .collect();
            let lf48 = resampler.process(&lf_after, 2);
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
            let n = lf_cap.min(mid_cap).min(tf_cap);
            let mut take = (n as u64).min(remaining) as usize;
            // A chunk that straddles a future ramp join ends at the join:
            // the silent part needs no LF window, and the window stays
            // untouched for the next chunk (which then starts exactly at
            // the join and runs aligned). Same for a future mid-xHE join.
            let straddle_join = [join, mxw.as_ref().and_then(|w| w.join), txw.as_ref().and_then(|w| w.join)]
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
            let (consume_core, mx_consume_core, tx_consume_core) = if straddle_join.is_some() {
                (0, 0, 0)
            } else if st.finished {
                let mxc = mxw
                    .as_ref()
                    .map_or(0, |w| w.window_core.saturating_sub(w.front_core));
                let txc = txw
                    .as_ref()
                    .map_or(0, |w| w.window_core.saturating_sub(w.front_core));
                (window_core.saturating_sub(front_core), mxc, txc)
            } else if let Some(mx) = st.mx.as_ref() {
                // Chunk-size quantization must land on an exactly-covering
                // grid of EVERY resampling lane: the xHE mid's n=160 at 48
                // kHz output; on 44.1 kHz-output streams the mid lane is
                // identity and the LF lane's n=441 rules (round-mode
                // output_frames would otherwise emit more outputs than the
                // consumed cores cover — a fraction of a sample per chunk,
                // accumulating into audible LF drift).
                let ngrid = if out_rate == sena_core::XFI_MID_RATE {
                    resampler.phases()
                } else {
                    mx.resampler.phases()
                };
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
                // xHE top rides an identity lane: consumed core == output.
                (lf_k, mx_k, take)
            } else {
                loop {
                    let lf_k = resampler.input_frames_for_output(take);
                    let mx_k = match st.mx.as_ref() {
                        Some(mx) => mx.resampler.input_frames_for_output(take),
                        None => Some(0),
                    };
                    if let (Some(a), Some(b)) = (lf_k, mx_k) {
                        break (a, b, take);
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
            let tx_pass_core = txw.as_ref().map_or(0, |w| {
                w.front_core + tx_consume_core.min(w.window_core.saturating_sub(w.front_core))
            });
            let start = st.produced;
            let end = start + take as u64;
            let rate = self.chunk_bits(start, end);
            (
                LfWin { lf48, abs0: lf_abs0 },
                mxw,
                mid_after,
                tf_out,
                take,
                start,
                rate,
                lf_pass_core,
                mx_pass_core,
                tx_pass_core,
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
                (mid_after[i * 2] * gain, mid_after[i * 2 + 1] * gain)
            };
            let (tl, tr) = if let Some(t) = &tf_out {
                let ti = t.j_start + i as i64;
                if ti >= 0 && (ti as usize) < t.shifted.len() / 2 {
                    (
                        t.shifted[ti as usize * 2] * gain,
                        t.shifted[ti as usize * 2 + 1] * gain,
                    )
                } else {
                    (0.0, 0.0)
                }
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
                if out_rate == sena_core::XFI_MID_RATE {
                    // 44.1 kHz output: retain one grid step beyond the
                    // guard so the phase-lock correction can shift the next
                    // window's front by up to m-1 samples.
                    let ctx_target =
                        (st.lf_resampler.guard_frames().div_ceil(grid) + 1) * grid;
                    let drain_core = lf_pass_core.saturating_sub(ctx_target);
                    st.lf_raw.drain(..drain_core * 2);
                    st.lf_ctx = lf_pass_core - drain_core;
                } else {
                    let fresh_drain = lf_pass_core.saturating_sub(st.lf_resampler.guard_frames());
                    let drain_core = fresh_drain - fresh_drain % grid;
                    st.lf_raw.drain(..drain_core * 2);
                    st.lf_ctx = lf_pass_core - drain_core;
                }
                // xHE mid lane: same drain discipline at 44.1 kHz. The
                // retained context is one grid step (147) deeper than the
                // guard so `mx_window`'s phase-lock correction can shift
                // the window front by up to m-1 samples without ever
                // undercutting the kernel's left support. The absolute
                // phase invariant itself (window base on the content m-grid)
                // is enforced in `mx_window`, not here: drain history alone
                // cannot hold it across seek anchors.
                if let Some(mx) = st.mx.as_mut() {
                    let grid = mx.resampler.decimation();
                    let ctx_target =
                        (mx.resampler.guard_frames().div_ceil(grid) + 1) * grid;
                    let drain_core = mx_pass_core.saturating_sub(ctx_target);
                    mx.raw.drain(..drain_core * 2);
                    mx.ctx = mx_pass_core - drain_core;
                }
                if let Some(tx) = st.tx.as_mut() {
                    // xHE top (identity lane): retain the shift's Hilbert
                    // half-length of left context (the identity resampler's
                    // guard is zero).
                    let ctx_target = HILBERT_DELAY + 1;
                    let drain_core = tx_pass_core.saturating_sub(ctx_target);
                    tx.raw.drain(..drain_core * 2);
                    tx.ctx = tx_pass_core - drain_core;
                }
            }
            if st.mx.is_none() {
                st.mid_raw.drain(..(mid_skip + take) * 2);
            }
            if let Some(top) = st.top.as_ref() {
                // Drain only raw 16 kHz frames whose 48 kHz support is fully
                // behind the next emit point: the shift needs HILBERT_DELAY
                // frames of left context and the resampler its guard.
                let rs = &top.rs;
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
            // Drop blocks fully behind the emit point (bit accounting).
            // (`produced` has already advanced past this chunk.)
            let emit_next = st.tf_seek_base + st.produced;
            let bspan = es::block_frames_out(self.params.init.output_rate());
            while st
                .block_bits
                .front()
                .map_or(false, |(s, _)| *s + bspan <= emit_next)
            {
                st.block_bits.pop_front();
            }
            st.first_chunk_pending = false;
            st.lf_trim_pending = false;
            if let Some(mx) = st.mx.as_mut() {
                mx.trim_pending = false;
            }
            if let Some(tx) = st.tx.as_mut() {
                tx.trim_pending = false;
            }
        }

        let mut last_bits = *self.bits_prefix.last().unwrap();
        for v in rate.iter().skip(1) {
            last_bits += v;
            self.bits_prefix.push(last_bits);
        }
        Ok(())
    }

    /// Per-frame payload-bit rate over [start, end): each decoded composite
    /// block's payload spread uniformly over its 20 ms span (the display
    /// granularity is blocks; per-AU interval tracking drifted for the
    /// non-block-aligned 44.1 kHz mid lane).
    fn chunk_bits(&self, start: u64, end: u64) -> Vec<f64> {
        let st = self.state.as_ref().unwrap();
        let base = (st.tf_seek_base + start) as i128;
        let n = (end - start) as usize;
        let span = es::block_frames_out(self.out_rate()) as i128;
        let mut delta = vec![0.0f64; n + 1];
        for &(bs, bits) in &st.block_bits {
            let bs = bs as i128;
            add_bits_interval(&mut delta, n as i128, bs - base, bs + span - base, bits as f64);
        }
        let mut rate = vec![0.0f64; n + 1];
        let mut acc = 0.0;
        for i in 0..n {
            acc += delta[i];
            rate[i + 1] = acc;
        }
        rate
    }

    fn fill(&mut self, want: u64) -> Result<(), DecodeError> {
        if self.state.is_none() {
            self.state = Some(self.make_state()?);
        }
        let mut force_feed = false;
        let mut spin_guard = 0u32;
        loop {
            // Livelock guard: every outer iteration must produce or feed.
            // (The 44.1 kHz-output take quantization once spun here.)
            spin_guard += 1;
            if spin_guard > 1_000_000 {
                return Err(DecodeError::Format("fill made no progress".into()));
            }
            let (available, finished) = {
                let st = self.state.as_ref().unwrap();
                ((st.output.len() / 2) as u64, st.finished)
            };
            if available >= want || finished {
                return Ok(());
            }
            // Decode blocks until the shorter ready stream covers a chunk.
            // `force_feed` covers the no-progress case: the readiness
            // estimate can clear MIN_CHUNK while the exact caps leave less
            // than one quantization grid (441 on 44.1 kHz-output streams),
            // so produce_chunk emits nothing and a plain re-check would
            // spin without ever feeding.
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
                            (core * SAMPLE_RATE as i128 / st.lf_rate as i128).max(0) as u64
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
                                    (core * SAMPLE_RATE as i128 / sena_core::XFI_MID_RATE as i128)
                                        .max(0) as u64
                                }
                            }
                            None => {
                                let trim = if st.first_chunk_pending { st.mid_trim as i128 } else { 0 };
                                ((st.mid_raw.len() / 2) as i128 - trim).max(0) as u64
                            }
                        }
                    };
                    let tf_ready = tf_emittable(st).max(0) as u64;
                    let ready = lf_ready.min(hf_ready);
                    (
                        if st.top.is_some() {
                            ready.min(tf_ready)
                        } else {
                            ready
                        },
                        st.finished,
                    )
                };
                if !force_feed && (ready >= MIN_CHUNK_FRAMES || eof) {
                    break;
                }
                force_feed = false;
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
            if after == before {
                if eof {
                    return Ok(());
                }
                // No production progress (sub-quantum take): feed before
                // re-producing, or the readiness gate would spin forever.
                force_feed = true;
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
        st.mid_trim = (ps_mid - ps_mid.min(ps_tf)) as usize;
        st.tf_trim = (ps_tf - ps_mid.min(ps_tf)) as usize;
        st.mid_decode_from = b0;
        st.tf_decode_from = b0;
        st.tf_next_pkt = b0;

        self.cursor = emit_start;
        self.bits_base = emit_start;
        self.bits_prefix = vec![0.0];
        self.state = Some(st);
        Ok(())
    }

    /// Mid-lane seek settle window, in AUs (~2.0 s at the 44.1 kHz mid
    /// rate): long enough that a mid-stream preroll event at the first
    /// indep AU after the anchor (max indep gap 86 AUs, mean 71) lands
    /// inside the silent window. The mid restart transient itself is mild
    /// (band corr ~0.98 from sample 0 even without settle).
    const XFI_MID_SETTLE_AUS: u64 = 88;

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
        let out_rate = self.params.init.output_rate();
        let n_blocks = self.store.as_ref().unwrap().blocks().len() as u64;
        // playable core c = f * lf_rate / out_rate; the raw core at that
        // point is c + 1024 (one warmup AU was trimmed at the stream start).
        let c = (frame as u128 * lf_rate as u128 / out_rate as u128) as u64;
        let target_raw_core = c + XHE_TRIM_CORE_SAMPLES as u64;
        // LF settle: the LF (USAC) lane's restart transient is
        // anchor-content-dependent and runs up to ~2 s at bad anchors (the
        // <600 Hz band fully decorrelated meanwhile); anchor 4 s of core
        // content before the target so the transient decays inside the
        // silent trim. Cost: ~60 ms of extra decode per seek (native).
        let lf_settle_aus = (lf_rate as u64 * 4).div_ceil(1024);
        let settle_raw = target_raw_core.saturating_sub(lf_settle_aus * 1024);
        let au = settle_raw / XHE_TRIM_CORE_SAMPLES as u64;
        let mut start_au = self.prev_indep_au(au);
        // The LF resampler needs ~1000 core samples of context before the
        // target; step one independency further back when needed.
        if start_au == au && start_au > 0 {
            start_au = self.prev_indep_au(start_au - 1);
        }
        let lf_trim = (target_raw_core - start_au * 1024) as usize;

        let xhe_mid = self.params.mid_xhe_asc.is_some();
        let ps_mid = self.params.mid_head.as_ref().map_or(0, |h| h.pre_skip as u64);
        // xHE mid lane: the LF AU math at the 44.1 kHz mid rate, plus a
        // settle window: the restart transient after a cold AU anchor needs
        // up to ~1.5 s to reconverge (worst measured anchor: corr 0.18 in
        // the first 0.5 s), so the lane anchors ~2 s of content before the
        // target and the transient plays out silently inside the trim. The
        // LF and top lanes need no settle (bit-exact / one-packet
        // convergence respectively) and simply skip the earlier blocks.
        // warmup_seek shares this seek via its shadow store, so push-mode
        // hosts get the identical window.
        let (mx_start_au, mx_trim) = if xhe_mid {
            let rate = sena_core::XFI_MID_RATE as u64;
            let c = (frame as u128 * rate as u128 / out_rate as u128) as u64;
            let target_raw = c + XHE_TRIM_CORE_SAMPLES as u64;
            let settle_raw = target_raw
                .saturating_sub(Self::XFI_MID_SETTLE_AUS * XHE_TRIM_CORE_SAMPLES as u64);
            let au = settle_raw / XHE_TRIM_CORE_SAMPLES as u64;
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
        // xHE top band (v4): same anchor math as the xHE mid (same
        // encoder, same AU model — verified on CBR128@44.1k streams), with
        // the same settle window class.
        let xhe_top = self.params.tf_xhe_asc.is_some();
        let (tx_start_au, tx_trim) = if xhe_top {
            let rate = sena_core::XFI_MID_RATE as u64;
            let c = (frame as u128 * rate as u128 / out_rate as u128) as u64;
            let target_raw = c + XHE_TRIM_CORE_SAMPLES as u64;
            let settle_raw = target_raw
                .saturating_sub(Self::XFI_MID_SETTLE_AUS * XHE_TRIM_CORE_SAMPLES as u64);
            let au = settle_raw / XHE_TRIM_CORE_SAMPLES as u64;
            let mut start_au = self.prev_indep_tx_au(au);
            if start_au == au && start_au > 0 {
                start_au = self.prev_indep_tx_au(start_au - 1);
            }
            (
                Some(start_au),
                Some((target_raw - start_au * 1024) as usize),
            )
        } else {
            (None, None)
        };
        let j0 = (frame + ps_mid) / es::BLOCK_FRAMES;
        let mid_trim = (frame + ps_mid - j0 * es::BLOCK_FRAMES) as usize;

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
        let tx_block = match tx_start_au {
            Some(au) => es::xhe_block(au, sena_core::XFI_MID_RATE, n_blocks),
            None => tf_j0,
        };
        let b0 = es::lf_block(start_au, lf_rate, n_blocks)
            .min(mid_block)
            .min(tx_block)
            .min(n_blocks - 1);

        self.cursor = frame;
        self.bits_base = frame;
        self.bits_prefix = vec![0.0];
        self.last_read = ReadInfo::default();
        self.output_start = None;
        self.state = None;
        let mut st = self.make_state()?;
        st.lf_trim = lf_trim;
        st.lf_trim_preroll_assumed = true;
        st.mid_trim = mid_trim;
        st.tf_trim = tf_trim;
        st.tf_seek_base = frame;
        st.lf_decode_from = start_au;

        if let (Some(mx), Some(start_au), Some(trim)) =
            (st.mx.as_mut(), mx_start_au, mx_trim)
        {
            mx.decode_from = start_au;
            mx.trim = trim;
            mx.trim_after = true;
            mx.trim_pending = true;
            mx.trim_preroll_assumed = true;
            mx.zero_prefix = if start_au == 0 {
                XHE_TRIM_CORE_SAMPLES
            } else {
                0
            };
        }
        if let (Some(tx), Some(start_au), Some(trim)) =
            (st.tx.as_mut(), tx_start_au, tx_trim)
        {
            tx.decode_from = start_au;
            tx.trim = trim;
            tx.trim_after = true;
            tx.trim_pending = true;
            tx.trim_preroll_assumed = true;
            tx.zero_prefix = if start_au == 0 {
                XHE_TRIM_CORE_SAMPLES
            } else {
                0
            };
        }
        st.mid_decode_from = j0;

        st.tf_decode_from = tf_j0;
        st.tf_next_pkt = b0;

        st.lf_trim_after = true;
        st.lf_zero_prefix = if start_au == 0 {
            XHE_TRIM_CORE_SAMPLES
        } else {
            0
        };
        self.store_index = b0 as usize;
        self.state = Some(st);
        Ok(())
    }

    /// Largest AU index <= `au` whose payload starts with the USAC
    /// independency flag (bit 7 of the first byte), i.e. a bit-exact random
    /// access point. AU 0 is by construction such a point. Scans the block
    /// index backwards from an estimate without allocating. `lane` maps a
    /// block to (first AU index, per-AU first-payload-byte flags, AU count).
    fn prev_indep_lane(
        &self,
        au: u64,
        est_block: impl Fn(u64) -> u64,
        lane: impl Fn(&BlockRef) -> (Option<u32>, &[u8], usize),
    ) -> u64 {
        let store = self.store.as_ref().unwrap();
        let blocks = store.blocks();
        if blocks.is_empty() {
            return 0;
        }
        // AUs are consecutive in block order; estimate the block that could
        // contain `au` and walk backwards from there. AU 0 is independent by
        // construction, so the loop always terminates.
        let est = est_block(blocks.len() as u64) as usize;
        for b in blocks[..=est.min(blocks.len() - 1)].iter().rev() {
            let (ai, first, count) = lane(b);
            if let Some(ai) = ai {
                for (i, &f) in first.iter().take(count).enumerate().rev() {
                    let idx = ai as u64 + i as u64;
                    if idx <= au && f & INDEP_BIT != 0 {
                        return idx;
                    }
                }
            }
        }
        0
    }

    /// Largest LF AU index <= `au` flagged independent.
    fn prev_indep_au(&self, au: u64) -> u64 {
        let lf_rate = self.params.init.lf_rate;
        self.prev_indep_lane(
            au,
            |n_blocks| es::lf_block(au, lf_rate, n_blocks),
            |b| (b.au_index, &b.lf_first, b.lf_count as usize),
        )
    }

    /// Largest xHE-mid AU index <= `au` flagged independent (mirror of
    /// `prev_indep_au` over the mid AU map cached by the demux index).
    fn prev_indep_mx_au(&self, au: u64) -> u64 {
        self.prev_indep_lane(
            au,
            |n_blocks| es::xhe_block(au, sena_core::XFI_MID_RATE, n_blocks),
            |b| (b.mx_au_index, &b.mx_first, b.mx_count as usize),
        )
    }

    /// Largest xHE-top AU index <= `au` flagged independent (mirror over
    /// the top-band AU map; v4 streams).
    fn prev_indep_tx_au(&self, au: u64) -> u64 {
        self.prev_indep_lane(
            au,
            |n_blocks| es::xhe_block(au, sena_core::XFI_MID_RATE, n_blocks),
            |b| (b.hf_au_index, &b.hf_first, b.hf_count as usize),
        )
    }
}

/// Start of xHE-mid AU `k`'s content on the 48 kHz grid of its arrival
/// block — the rational-rate generalization of `lf_au_start48` (44.1 kHz
/// does not divide 48 kHz, so per-AU durations alternate 1114/1115).
/// Start of xHE AU `k`'s content on the output timeline (48 kHz grid for
/// 48 kHz-output streams, 44.1 kHz for the all-xHE variant). The placement
/// residue rides the encoder's 48 kHz block grid; AU 0 anchors at -1024
/// (its warmup preroll).
fn mx_au_start_out(k: u64, block_ts_ns: u64, out_rate: u32) -> i64 {
    let rate = sena_core::XFI_MID_RATE;
    if k == 0 {
        return -(es::xhe_au_end_out(1, rate, out_rate) as i64);
    }
    let ts_out = (block_ts_ns / 1_000_000 * (out_rate as u64 / 1000)) as i64;
    let end_out = es::xhe_au_end_out(k, rate, out_rate) as i64;
    let dur = end_out - es::xhe_au_end_out(k - 1, rate, out_rate) as i64;
    let r = (end_out - 1).rem_euclid(es::block_frames_out(out_rate) as i64) + 1;
    ts_out + r - dur
}

/// Top-band production output for one chunk.
struct TfOut {
    shifted: Vec<f64>,
    /// Emit index of the chunk start on the shifted window (may be
    /// negative while an xHE top band ramps toward a future join: the
    /// mixer reads zeros there).
    j_start: i64,
}

/// Produce the top band's shifted window for one chunk: the xHE top (v4)
/// rides an identity mx-style window at 44.1 kHz with the SSB shift-back
/// at the lane rate (carrier locked to the playable timeline); the Opus
/// top upsamples the pending 16 kHz queue to 48 kHz and shifts there.
/// `#[inline(never)]`: its temporaries live in their own frame instead of
/// inflating `produce_chunk`'s (the fb2k decode thread stack is tight, see
/// `decode_fits_small_host_stack`).
#[inline(never)]
#[allow(clippy::too_many_arguments)]
fn top_window(
    tx: Option<&MxLane>,
    top: Option<&TopLane>,
    tf_raw: &VecDeque<f32>,
    tf_base16: usize,
    tf_trim: usize,
    tf_seek_base: u64,
    produced: u64,
    finished: bool,
    tx_skip: usize,
    tx_trim_after: bool,
    start_abs: i64,
) -> Result<(Option<MxWin>, Option<TfOut>), DecodeError> {
    if let Some(tx) = tx {
        if tx.muted || !tx.anchored {
            return Ok((None, None));
        }
        let w = mx_window(tx, tx_skip, tx_trim_after, finished, start_abs);
        let shifted = sena_dsp::shift_up_offset(
            &w.s48,
            2,
            sena_core::HF_SPLIT_HZ,
            sena_core::XFI_MID_RATE,
            w.abs0,
        );
        let j_start = start_abs - w.abs0;
        return Ok((Some(w), Some(TfOut { shifted, j_start })));
    }
    if let Some(top) = top {
        let rs = &top.rs;
        let win16: Vec<f64> = tf_raw.iter().map(|&s| f64::from(s)).collect();
        let up = rs.process(&win16, 2);
        let origin = tf_seek_base as i64 + 3 * tf_base16 as i64 - tf_trim as i64;
        let shifted =
            sena_dsp::shift_up_offset(&up, 2, sena_core::HF_SPLIT_HZ, SAMPLE_RATE, origin);
        let j_start = produced as i128 + tf_trim as i128 - 3 * tf_base16 as i128;
        if j_start < 0 {
            return Err(DecodeError::Format(
                "top-band window underrun (negative emit index)".into(),
            ));
        }
        return Ok((
            None,
            Some(TfOut {
                shifted,
                j_start: j_start as i64,
            }),
        ));
    }
    Ok((None, None))
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
    // Same shape as the LF lane: window_front_core is the context inside
    // the window; front_core = raw_drop + window_front_core is the total
    // passed over from the raw head (the drain bookkeeping uses it).
    let window_front_core =
        mx.ctx.max(raw_drop) - raw_drop + if mx_trim_after { mx_skip } else { 0 };
    // Phase-lock the window onto the whole-file decode's grid: the
    // resampler anchors its phase at the window start, so the window
    // base's content position must stay a multiple of m (147). The drain
    // history alone does NOT guarantee this after a seek (a clamped drain
    // can leave the base one AU = 1024 samples off, a 5/147-sample phase
    // error that permanently decorrelates the band); correct it here by
    // shifting the window front by the residue. The emit content mapping
    // is preserved (front_core is unchanged), and with pc = 0 on the
    // whole-file trajectory this is a no-op there.
    let grid = mx.resampler.decimation() as i64;
    let base_content =
        mx.total_appended as i64 - (mx.raw.len() / 2) as i64 + raw_drop as i64 + mx.content_shift;
    let pc = base_content.rem_euclid(grid) as usize;
    let (raw_drop, window_front_core) = if pc > 0 && window_front_core >= pc {
        (raw_drop + pc, window_front_core - pc)
    } else {
        // Anchor trim can be shorter than the residue: one off-grid chunk
        // inside the restart transient, self-correcting next chunk.
        (raw_drop, window_front_core)
    };
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

/// [`lf_au_start48`] on the output timeline of a 44.1 kHz-output stream:
/// the placement residue is reconstructed on the encoder's 48 kHz block
/// grid (the wire blocks live there) and the result is scaled to the
/// output rate.
fn lf_au_start_out(k: u64, block_ts_ns: u64, lf_rate: u32, out_rate: u32) -> i64 {
    if out_rate == SAMPLE_RATE as u32 {
        return lf_au_start48(k, block_ts_ns, lf_rate);
    }
    let s48 = lf_au_start48(k, block_ts_ns, lf_rate);
    s48 * out_rate as i64 / SAMPLE_RATE as i64
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

        // The xHE mid trim-class fix made mid-file seeks content-exact
        // (before it, a content-only restart AU landed the mid one AU late:
        // diff ~0.7). Tight bound at half the track after a 1 s settle
        // (the restart state transient is a separate, gated matter).
        let target = (playable / 2) as u64;
        let mut d = store_decoder(&bytes);
        d.seek(target).unwrap();
        let got = decode_all(&mut d);
        assert_eq!(got.len(), (playable - target as usize) * 2);
        let mut max = 0.0f32;
        for i in 48000..playable - target as usize {
            for c in 0..2 {
                let diff = (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                max = max.max(diff);
            }
        }
        assert!(max < 0.02, "mid-file seek diff {max}");
    }

    /// Regression pin for the mid-stream preroll defect: a diverged
    /// post-restart xHE state can make a later independency AU emit
    /// preroll+content (2048) instead of content-only (1024); if the
    /// preroll enters the queue the whole mid lane shifts one AU off the
    /// timeline forever (found as seeks diverging ~3.5 s in, card 80).
    #[test]
    fn xhe_mid_seek_residual_divergence() {
        let bytes = asset_bytes("01__p600xfi__hf96");
        let whole = crate::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
        let playable = whole.info().playable_frames as usize;
        for frac in [4usize, 3] {
            let target = (playable / frac) as u64;
            let mut d = store_decoder(&bytes);
            d.seek(target).unwrap();
            let got = decode_all(&mut d);
            let mut max = 0.0f32;
            for i in 48000..got.len() / 2 {
                for c in 0..2 {
                    let diff =
                        (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                    max = max.max(diff);
                }
            }
            assert!(max < 0.02, "seek to 1/{frac} residual diff {max}");
            // The settle windows must place the restart transient before
            // the target: the first 100 ms are then near-clean even at bad
            // anchors (pre-settle values here reached 0.17-1.6).
            let mut early = 0.0f32;
            for i in 0..4800.min(got.len() / 2) {
                for c in 0..2 {
                    let diff =
                        (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                    early = early.max(diff);
                }
            }
            assert!(early < 0.05, "seek to 1/{frac} first-100ms diff {early}");
        }
    }

    /// xHE top band (v4, the xfi 288k nominal): the 44.1 kHz-output stream
    /// must decode identically between the batch and streaming pipelines
    /// (the golden gate), on both LF rates (the 441/160 and 441/320
    /// resample grids).
    #[test]
    fn xfi288_stream_matches_batch() {
        for name in ["01__p600xfi__hf128x", "01__p300xfi__hf128x"] {
            let bytes = asset_bytes(name);
            let whole = crate::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
            assert_eq!(whole.info().sample_rate, 44100);
            let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
            let playable = whole.info().playable_frames as usize;
            let got = {
                let mut d = store_decoder(&bytes);
                decode_all(&mut d)
            };
            assert_eq!(got.len(), playable * 2, "{name}: length");
            let max = got
                .iter()
                .zip(whole_pcm.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0f32, f32::max);
            assert!(max < 1e-5, "{name}: streaming vs batch max diff {max}");
        }
    }

    /// v4 seek acceptance: the settle windows place the restart transients
    /// before the target; residual content-dependent transients are the
    /// same class as the 48 kHz xfi paths (gated there).
    #[test]
    fn xfi288_seek() {
        let bytes = asset_bytes("01__p600xfi__hf128x");
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
        assert!(max < 5e-3, "seek(0) vs continuous: {max}");

        for frac in [4usize, 2] {
            let target = (playable / frac) as u64;
            let mut d = store_decoder(&bytes);
            d.seek(target).unwrap();
            let got = decode_all(&mut d);
            assert_eq!(got.len(), (playable - target as usize) * 2);
            let mut max = 0.0f32;
            for i in 44100..got.len() / 2 {
                for c in 0..2 {
                    let diff =
                        (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                    max = max.max(diff);
                }
            }
            assert!(max < 0.02, "seek to 1/{frac} residual diff {max}");
            let mut early = 0.0f32;
            for i in 0..4410.min(got.len() / 2) {
                for c in 0..2 {
                    let diff =
                        (got[i * 2 + c] - whole_pcm[target as usize * 2 + i * 2 + c]).abs();
                    early = early.max(diff);
                }
            }
            assert!(early < 0.05, "seek to 1/{frac} first-100ms diff {early}");
        }
    }

    /// The push/pull bit-identity contract holds on 44.1 kHz-output
    /// streams: warmup_seek rides the same seek() through the shadow store.
    #[test]
    fn xfi288_warmup_seek_matches_container_seek() {
        let bytes = asset_bytes("01__p600xfi__hf128x");
        let rate = 44100u64; // the v4 stream's output rate
        for target in [111_111u64, 600_000, 881_900] {
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
                let cue_block = (target * 1_000_000_000 / rate) / 1_000_000_000 * 50;
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
        for asset in ["01__p300__lfa__hf144", "01__p600__3t__hf160+96", "01__p600xfi__hf128x"] {
            let bytes = asset_bytes(asset);
            let handle = std::thread::Builder::new()
                // 448 KiB mirrors the foobar2000 decode thread's 544 KiB
                // budget (release); debug builds need more headroom for the
                // same code (512 -> 640 for the v3 xHE-mid lane, -> 768 for
                // the v4 xHE top lane's extra match arms in produce_chunk).
                .stack_size(if cfg!(debug_assertions) { 768 * 1024 } else { 448 * 1024 })
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
