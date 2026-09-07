//! Streaming decoder: indexes the container at open time, then decodes codec
//! frames incrementally as `read_f32` requests PCM. LF and HF codec frames are
//! consumed independently and any unconsumed prefix is retained across chunks,
//! so a short chunk on one track never discards samples from the other track.
//! The zero-phase rational resampler runs on whole LF prefixes, and chunks are
//! cut at exact resampler input/output boundaries.
//!
//! Frame payloads are read lazily through [`FrameStore`]: over callback IO
//! (the foobar2000 plugin) only a small frame index is kept in memory and
//! each packet is fetched with a positioned read when decoded, so per-instance
//! memory stays bounded no matter the file size (32-bit hosts).

use std::collections::VecDeque;
use std::panic::{AssertUnwindSafe, catch_unwind};

use opus_decoder::{Channels, DecodeMode, Decoder as OpusDecoder};
use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_core::PRE_GAIN;
use sena_dsp::Resampler;

use crate::demux::{Demuxed, FrameRef, IndexedFile, Track};
use crate::pipeline::{
    DecodeError, DecodedInfo, LfAuInfo, OpusPacketInfo, ReadInfo, SAMPLE_RATE,
    add_bits_interval, parse_opus_head,
};

const MIN_CHUNK_FRAMES: u64 = 4800; // 100 ms @48k

/// Where codec frame payloads come from. Both variants share the same
/// `FrameRef` index; only the payload fetch differs.
pub enum FrameStore {
    /// Whole file in memory (non-seekable inputs, examples): payloads are
    /// copied out of the buffer on demand. Peak memory is ~1x file size.
    Memory { bytes: Vec<u8>, frames: Vec<FrameRef> },
    /// Lazy positioned reads through the host's io callbacks: the index is
    /// the only per-file state, decode fetches one packet at a time. A small
    /// read-ahead window keeps linear decode from doing a seek+read syscall
    /// per frame (~600 B blocks would otherwise mean one per frame).
    Lazy {
        frames: Vec<FrameRef>,
        read_at: Box<dyn FnMut(u64, usize) -> Result<Vec<u8>, String>>,
        /// `(absolute offset of window[0], window bytes)`.
        window: (u64, Vec<u8>),
    },
}

/// Read-ahead for lazy frame fetches; frames are consumed in file order, so
/// one window covers hundreds of blocks.
const LAZY_READ_AHEAD: usize = 256 * 1024;

impl FrameStore {
    fn frames(&self) -> &[FrameRef] {
        match self {
            FrameStore::Memory { frames, .. } => frames,
            FrameStore::Lazy { frames, .. } => frames,
        }
    }

    /// Fetch the payload of frame `index` (one bounded allocation per call).
    fn read(&mut self, index: usize) -> Result<Vec<u8>, DecodeError> {
        let fr = self.frames()[index];
        match self {
            FrameStore::Memory { bytes, .. } => {
                let start = usize::try_from(fr.offset)
                    .map_err(|_| DecodeError::Format("frame offset too large".into()))?;
                bytes
                    .get(start..start + fr.len as usize)
                    .map(|s| s.to_vec())
                    .ok_or_else(|| DecodeError::Format("frame range outside file".into()))
            }
            FrameStore::Lazy { read_at, window, .. } => {
                let len = fr.len as usize;
                let end = fr.offset + len as u64;
                let wend = window.0 + window.1.len() as u64;
                if fr.offset < window.0 || end > wend {
                    let fetch = len.max(LAZY_READ_AHEAD);
                    let buf = read_at(fr.offset, fetch).map_err(DecodeError::Io)?;
                    if buf.len() < len {
                        return Err(DecodeError::Format("truncated frame payload".into()));
                    }
                    *window = (fr.offset, buf);
                }
                let off = (fr.offset - window.0) as usize;
                Ok(window.1[off..off + len].to_vec())
            }
        }
    }
}

pub struct StreamingDecoder {
    tracks: Vec<Track>,
    store: FrameStore,
    info: DecodedInfo,
    pub warnings: Vec<String>,
    cursor: u64,
    state: Option<State>,
    /// Absolute frame index of `bits_prefix[0]` (bits_prefix only covers
    /// frames produced since the last seek, to stay memory-bounded).
    bits_base: u64,
    bits_prefix: Vec<f64>,
    last_read: ReadInfo,
}

struct State {
    lf_rate: u32,
    /// Leading trim applied on the first produce/fill after (re)start: one
    /// 1024-sample core frame at stream start; at a seek, the raw samples
    /// from the jump AU up to the target frame. Reads the same as `first`.
    lf_trim: usize,
    hf_trim: usize,
    /// Per-track AU/packet counters still to skip (seek jump without decode).
    lf_skip_aus: usize,
    hf_skip_pkts: usize,
    /// Seek (true): keep the raw LF context before the trim point so the
    /// zero-phase resampler has real audio instead of zeros (start: false).
    lf_trim_after: bool,
    xdec: UsacDecoder,
    opus: OpusDecoder,
    frame_index: usize,
    lf_raw: VecDeque<f32>,
    hf_raw: VecDeque<f32>,
    lf_total_before: i128,
    opus_total_before: i128,
    lf_infos: Vec<LfAuInfo>,
    opus_infos: Vec<OpusPacketInfo>,
    lf_errors: usize,
    opus_errors: usize,
    output: VecDeque<f64>,
    first: bool,
    produced: u64,
    finished: bool,
}

impl StreamingDecoder {
    /// Whole-file constructor (examples/tests): re-indexes the parsed
    /// container's bytes so the decode path is identical to the lazy one.
    pub fn open(demux: Demuxed) -> Result<Self, DecodeError> {
        let bytes = demux.into_bytes();
        let mut indexed = {
            let slice = &bytes;
            crate::demux::index_container(
                &mut |pos: u64, len: usize| crate::demux::mem_read_at(slice, pos, len),
                true,
            )
            .map_err(DecodeError::Format)?
        };
        let frames = std::mem::take(&mut indexed.frames);
        Self::open_indexed(indexed, FrameStore::Memory { bytes, frames })
    }

    /// Bounded-memory constructor: `indexed` came from
    /// [`crate::demux::index_container`], `store` serves frame payloads.
    pub fn open_indexed(indexed: IndexedFile, store: FrameStore) -> Result<Self, DecodeError> {
        let (info, warnings) = crate::pipeline::probe(&indexed)?;
        Ok(Self {
            tracks: indexed.tracks,
            store,
            info,
            warnings,
            cursor: 0,
            state: None,
            bits_base: 0,
            bits_prefix: vec![0.0],
            last_read: ReadInfo::default(),
        })
    }

    fn track(&self, codec_id: &str) -> &Track {
        // probe() already validated the two Sena tracks exist.
        self.tracks.iter().find(|t| t.codec_id == codec_id).unwrap()
    }

    pub fn info(&self) -> DecodedInfo {
        self.info.clone()
    }

    fn init_state(&mut self) -> Result<(), DecodeError> {
        let lf_track = self.track("A_SENALF");
        let hf_track = self.track("A_OPUS");
        let lf_rate = lf_track.sample_rate.round() as u32;
        let asc = AudioSpecificConfig::parse(&lf_track.codec_private)
            .map_err(|e| DecodeError::Format(format!("ASC parse: {e}")))?;
        let opus_head = parse_opus_head(&hf_track.codec_private)?;
        let xdec = UsacDecoder::new(&asc)
            .map_err(|e| DecodeError::Codec(format!("rxaac init: {e}")))?;
        if xdec.num_channels() != 2 {
            return Err(DecodeError::Unsupported("rxaac channels != 2".into()));
        }
        let mut opus = OpusDecoder::new(SAMPLE_RATE, Channels::Stereo)
            .map_err(|e| DecodeError::Codec(format!("opus init: {e}")))?;
        if opus_head.output_gain != 0 {
            opus.set_gain(i32::from(opus_head.output_gain))
                .map_err(|e| DecodeError::Codec(format!("opus output gain: {e}")))?;
        }
        self.state = Some(State {
            lf_rate,
            lf_trim: crate::pipeline::XHE_TRIM_CORE_SAMPLES,
            hf_trim: opus_head.pre_skip as usize,
            lf_skip_aus: 0,
            hf_skip_pkts: 0,
            lf_trim_after: false,
            xdec,
            opus,
            frame_index: 0,
            lf_raw: VecDeque::new(),
            hf_raw: VecDeque::new(),
            lf_total_before: 0,
            opus_total_before: 0,
            lf_infos: Vec::new(),
            opus_infos: Vec::new(),
            lf_errors: 0,
            opus_errors: 0,
            output: VecDeque::new(),
            first: true,
            produced: 0,
            finished: false,
        });
        Ok(())
    }

    fn decode_next_frame(&mut self) -> Result<bool, DecodeError> {
        let frame_index = self.state.as_ref().unwrap().frame_index;
        if frame_index >= self.store.frames().len() {
            self.state.as_mut().unwrap().finished = true;
            return Ok(false);
        }
        // Fetch the payload before borrowing the decode state (the store
        // needs &mut self for lazy io reads).
        let frame = self.store.frames()[frame_index];
        let data = self.store.read(frame_index)?;
        let lf_num = self.track("A_SENALF").number;
        let st = self.state.as_mut().unwrap();
        st.frame_index += 1;
        if frame.track == lf_num {
            if st.lf_skip_aus > 0 {
                st.lf_skip_aus -= 1;
                return Ok(true);
            }
            // Decode into a scratch buffer so a panicking decoder can never
            // leave partial samples in the stream queue.
            let mut out: Vec<f32> = Vec::new();
            let result = catch_unwind(AssertUnwindSafe(|| {
                st.xdec.decode_au(&data, &mut out)
            }));
            let failed = match result {
                Ok(Err(e)) => {
                    eprintln!("warning: LF AU decode error ({}): {e}", frame.t_ns);
                    true
                }
                Err(_) => {
                    eprintln!("warning: LF AU decoder panic ({}); replacing with silence", frame.t_ns);
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
                return Err(DecodeError::Codec("rxaac returned a non-integral frame".into()));
            }
            st.lf_raw.extend(out);
            st.lf_infos.push(LfAuInfo {
                start_core: st.lf_total_before,
                samples_core: samples as i128,
                bits: (data.len() * 8) as u64,
            });
            st.lf_total_before += samples as i128;
        } else {
            if st.hf_skip_pkts > 0 {
                st.hf_skip_pkts -= 1;
                return Ok(true);
            }
            let mut buf = vec![0.0f32; 5760 * 2];
            let mut n = match st.opus.decode_float(Some(&data), &mut buf, DecodeMode::Normal) {
                Ok(n) => n,
                Err(e) => {
                    eprintln!("warning: Opus packet decode error ({}): {e}; using PLC", frame.t_ns);
                    st.opus_errors += 1;
                    st.opus.decode_float(None, &mut buf, DecodeMode::Normal)
                        .map_err(|e| DecodeError::Codec(format!("opus PLC: {e}")))?
                }
            };
            n = n.min(5760);
            st.hf_raw.extend(buf[..n * 2].iter().copied());
            st.opus_infos.push(OpusPacketInfo {
                start_48k: st.opus_total_before,
                samples_48k: n as i128,
                bits: (data.len() * 8) as u64,
            });
            st.opus_total_before += n as i128;
        }
        Ok(true)
    }

    fn chunk_bits(
        &self,
        lf_rate: u32,
        lf_trim: usize,
        hf_trim: usize,
        lf_infos: &[LfAuInfo],
        opus_infos: &[OpusPacketInfo],
        start: u64,
        end: u64,
    ) -> Vec<f64> {
        let n = (end - start) as usize;
        let mut delta = vec![0.0f64; n + 1];
        let base = start as i128;
        // infos are relative to the raw stream start (AU skip counters and
        // the leading trim keep them aligned with the playable timeline).
        for info in lf_infos {
            let s_core = info.start_core - lf_trim as i128;
            let s48 = s_core * 48_000 / lf_rate as i128;
            let e48 = (s_core + info.samples_core) * 48_000 / lf_rate as i128;
            add_bits_interval(&mut delta, n as i128, s48 - base, e48 - base, info.bits as f64);
        }
        for info in opus_infos {
            let s = info.start_48k - hf_trim as i128;
            let e = s + info.samples_48k;
            add_bits_interval(&mut delta, n as i128, s - base, e - base, info.bits as f64);
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

    fn produce_chunk(&mut self) -> Result<(), DecodeError> {
        let lf_skip = {
            let st = self.state.as_ref().unwrap();
            if st.first { st.lf_trim } else { 0 }
        };
        let hf_skip = {
            let st = self.state.as_ref().unwrap();
            if st.first { st.hf_trim } else { 0 }
        };
        let raw_empty = {
            let st = self.state.as_ref().unwrap();
            st.lf_raw.len() <= lf_skip * 2 || st.hf_raw.len() <= hf_skip * 2
        };
        if raw_empty {
            let st = self.state.as_mut().unwrap();
            if st.finished {
                // Trailing unmatched stream data after the shorter track ended.
                st.lf_raw.clear();
                st.hf_raw.clear();
                st.lf_infos.clear();
                st.opus_infos.clear();
                st.first = false;
            }
            return Ok(());
        }

        let (lf48, lf_off, hf_after, take, start, rate, lf_consume_core) = {
            let st = self.state.as_ref().unwrap();
            let trim_after = st.lf_trim_after;
            let lf_after: Vec<f64> = st
                .lf_raw
                .iter()
                .skip(if trim_after { 0 } else { lf_skip * 2 })
                .map(|&s| f64::from(s))
                .collect();
            let hf_after: Vec<f64> = st
                .hf_raw
                .iter()
                .skip(hf_skip * 2)
                .map(|&s| f64::from(s))
                .collect();
            let resampler = Resampler::new(st.lf_rate, SAMPLE_RATE);
            let lf48 = resampler.process(&lf_after, 2);
            // Seek: the pre-target raw stays as the resampler context; the
            // corresponding output prefix is skipped after resampling.
            let lf_off = if trim_after { resampler.output_frames(lf_skip) } else { 0 };
            let n = (lf48.len() / 2).saturating_sub(lf_off).min(hf_after.len() / 2);
            let remaining = self.info.playable_frames.saturating_sub(st.produced);
            let mut take = (n as u64).min(remaining) as usize;
            // The rational ratio only represents some output counts exactly;
            // trim up to 2 frames from the chunk so the resampler can map the
            // raw count one-to-one (the frames are emitted by the next chunk).
            let consume_core = loop {
                if take == 0 {
                    return Ok(());
                }
                if let Some(k) = resampler.input_frames_for_output(take) {
                    break k;
                }
                take -= 1;
            };
            // Raw consumed for `take` outputs counted from the trimmed
            // (seek) point; the pre-target raw stays as context.
            let lf_consume_core = consume_core.min(lf_after.len() / 2);
            let start = st.produced;
            let end = start + take as u64;
            let rate = self.chunk_bits(st.lf_rate, st.lf_trim, st.hf_trim, &st.lf_infos, &st.opus_infos, start, end);
            (lf48, lf_off, hf_after, take, start, rate, lf_consume_core)
        };

        let gain = 1.0 / PRE_GAIN;
        let mut chunk_out = Vec::with_capacity(take * 2);
        for i in 0..take {
            let lf = lf48[(i + lf_off) * 2] * gain;
            let r = lf48[(i + lf_off) * 2 + 1] * gain;
            let hl = hf_after[i * 2] * gain;
            let hr = hf_after[i * 2 + 1] * gain;
            chunk_out.push(lf + hl);
            chunk_out.push(r + hr);
        }

        {
            let st = self.state.as_mut().unwrap();
            st.output.extend(chunk_out);
            st.produced = start + take as u64;
            // The pre-target raw stays as resampler context on seeks; the
            // consumed raw covers the trimmed part plus the take part.
            for _ in 0..(lf_skip + lf_consume_core) * 2 {
                st.lf_raw.pop_front();
            }
            for _ in 0..(hf_skip + take) * 2 {
                st.hf_raw.pop_front();
            }
            Self::trim_lf_infos(&mut st.lf_infos, lf_consume_core as i128);
            Self::trim_opus_infos(&mut st.opus_infos, take as i128);
            st.first = false;
        }

        let mut last_bits = *self.bits_prefix.last().unwrap();
        for v in rate.iter().skip(1) {
            last_bits += v;
            self.bits_prefix.push(last_bits);
        }
        Ok(())
    }

    fn fill(&mut self, want: u64) -> Result<(), DecodeError> {
        if self.state.is_none() {
            self.init_state()?;
        }
        loop {
            let (available, finished) = {
                let st = self.state.as_ref().unwrap();
                ((st.output.len() / 2) as u64, st.finished)
            };
            if available >= want || finished {
                return Ok(());
            }
            // Decode codec frames until the shorter ready stream covers a chunk.
            loop {
                let (ready, eof) = {
                    let st = self.state.as_ref().unwrap();
                    let lf_ready = {
                        let trim = if st.first { st.lf_trim as i128 } else { 0 };
                        let core = (st.lf_raw.len() / 2) as i128 - trim;
                        (core * 48_000 / st.lf_rate as i128).max(0) as u64
                    };
                    let hf_ready = {
                        let trim = if st.first { st.hf_trim as i128 } else { 0 };
                        ((st.hf_raw.len() / 2) as i128 - trim).max(0) as u64
                    };
                    (lf_ready.min(hf_ready), st.finished)
                };
                if ready >= MIN_CHUNK_FRAMES || eof {
                    break;
                }
                if !self.decode_next_frame()? {
                    break;
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

    pub fn read_f32(&mut self, out: &mut [f32], max_frames: u64) -> Result<u64, DecodeError> {
        let remaining = self.info.playable_frames.saturating_sub(self.cursor);
        let want = max_frames.min(remaining);
        if out.len() < want as usize * 2 {
            return Err(DecodeError::Format("output buffer too small".into()));
        }
        if want == 0 {
            self.last_read = ReadInfo { start_frame: self.info.playable_frames, frames: 0, payload_bits: 0 };
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
        // Without this the prefix grows by 8 B per produced frame for the
        // whole track (~90 MB for a 4 min track) - steady in-track growth on
        // the host, and fatal on a 32-bit address space.
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

    /// Jump seek: xHE-AAC streams mark independent random-access AUs with
    /// the USAC independency flag (the first payload bit); a fresh decoder
    /// started at such an AU is bit-exact with sequential decoding, so the
    /// seek decodes only a handful of AUs instead of the whole prefix.
    /// Opus packets are independent and jump directly by index.
    pub fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        if frame > self.info.playable_frames {
            return Err(DecodeError::Format("seek past playable length".into()));
        }
        let f = frame as usize;
        let lf_track = self.track("A_SENALF").clone();
        let hf_track = self.track("A_OPUS").clone();
        let lf_rate = lf_track.sample_rate.round() as u32;
        // playable core c = f * lf_rate / 48000; the raw core at that point is
        // c + 1024 (one warmup AU was trimmed at the stream start).
        let c = (f as u128 * lf_rate as u128 / 48_000) as usize;
        let target_raw_core = c + crate::pipeline::XHE_TRIM_CORE_SAMPLES;
        let au = target_raw_core / crate::pipeline::XHE_TRIM_CORE_SAMPLES;
        let mut start_au = self.prev_indep_au(au);
        // The LF resampler needs ~1000 core samples of context before the
        // target; step one independency further back when needed.
        if start_au == au && start_au > 0 {
            start_au = self.prev_indep_au(start_au - 1);
        }
        let lf_trim = target_raw_core - start_au * crate::pipeline::XHE_TRIM_CORE_SAMPLES;

        let pre_skip = parse_opus_head(&hf_track.codec_private)?.pre_skip as usize;
        let j0 = (f + pre_skip) / 960;
        let hf_trim = f + pre_skip - j0 * 960;

        self.cursor = frame;
        self.bits_base = frame;
        self.bits_prefix = vec![0.0];
        self.last_read = ReadInfo::default();
        self.state = None;
        self.init_state()?;
        {
            let st = self.state.as_mut().unwrap();
            st.lf_trim = lf_trim;
            st.hf_trim = hf_trim;
            st.lf_skip_aus = start_au;
            st.hf_skip_pkts = j0;
            st.lf_trim_after = true;
        }
        Ok(())
    }

    /// Largest LF AU index <= `au` whose payload starts with the USAC
    /// independency flag (bit 7 of the first byte), i.e. a bit-exact random
    /// access point. AU 0 is by construction such a point.
    fn prev_indep_au(&self, au: usize) -> usize {
        let lf_num = self.track("A_SENALF").number;
        let positions: Vec<usize> = self
            .store
            .frames()
            .iter()
            .enumerate()
            .filter(|(_, f)| f.track == lf_num)
            .map(|(i, _)| i)
            .collect();
        let mut idx = au.min(positions.len().saturating_sub(1));
        loop {
            let f = self.store.frames()[positions[idx]];
            if f.first >> 7 != 0 || idx == 0 {
                return idx;
            }
            idx -= 1;
        }
    }

}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::Demuxed;

    /// The bit-accounting prefix must stay bounded to produced-but-unread
    /// frames; it used to grow by one f64 per frame for the whole track
    /// (steady memory growth visible in the host during playback).
    #[test]
    fn bits_prefix_stays_bounded_over_full_decode() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let bytes = std::fs::read(path).unwrap();
        let mut dec = StreamingDecoder::open(Demuxed::parse(bytes).unwrap()).unwrap();
        let playable = dec.info().playable_frames;
        let mut buf = vec![0.0f32; 4096 * 2];
        let mut total = 0u64;
        let mut max_prefix = 0usize;
        loop {
            let n = dec.read_f32(&mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            total += n;
            max_prefix = max_prefix.max(dec.bits_prefix.len());
        }
        assert_eq!(total, playable);
        // A read produces at most a couple of ~100 ms decode chunks ahead of
        // the cursor; before the fix this reached playable_frames + 1
        // (960001 entries for this asset).
        assert!(max_prefix <= 100_000, "bits_prefix grew to {max_prefix} entries");

        // Seeking mid-stream and reading on must stay bounded too.
        dec.seek(playable / 2).unwrap();
        loop {
            let n = dec.read_f32(&mut buf, 4096).unwrap();
            if n == 0 {
                break;
            }
            max_prefix = max_prefix.max(dec.bits_prefix.len());
        }
        assert!(max_prefix <= 100_000, "bits_prefix grew to {max_prefix} entries after seek");
    }

    /// Regression guard for the foobar2000 macOS stack overflow: the fb2k
    /// playback decoding thread has a 544 KiB stack, and decoder init +
    /// decode must fit comfortably inside it. (rxaac-dec's MpsDec used to be
    /// a ~76 KiB inline field of UsacDecoder whose by-value construction
    /// needed >640 KiB of stack = instant SIGBUS on the Mac thread. Now
    /// boxed; the whole pipeline fits in ~256 KiB. We test at 448 KiB for
    /// toolchain headroom.)
    #[test]
    fn decode_fits_small_host_stack() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let bytes = std::fs::read(path).unwrap();
        let handle = std::thread::Builder::new()
            .stack_size(448 * 1024)
            .spawn(move || {
                let mut dec = StreamingDecoder::open(Demuxed::parse(bytes).unwrap()).unwrap();
                let playable = dec.info().playable_frames;
                let mut buf = vec![0.0f32; 4096 * 2];
                let mut total = 0u64;
                loop {
                    let n = dec.read_f32(&mut buf, 4096).unwrap();
                    if n == 0 {
                        break;
                    }
                    total += n;
                }
                assert_eq!(total, playable);
            })
            .unwrap();
        // A stack overflow aborts the whole process (no unwind), so join
        // succeeding at all is the assertion.
        handle.join().unwrap();
    }
}
