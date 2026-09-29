//! Streaming container decoder: the ES core ([`EsDecoder`]) driven by a
//! container block store. Indexes the container at open time, then decodes
//! composite blocks incrementally as `read_f32` requests PCM. The zero-phase
//! rational resampler runs on whole LF prefixes, and chunks are cut at exact
//! resampler input/output boundaries.
//!
//! Block payloads are read lazily through [`BlockStore`]: over callback IO
//! (the foobar2000 plugin) only a small block index is kept in memory and
//! each block is fetched with a positioned read when decoded, so per-instance
//! memory stays bounded no matter the file size (32-bit hosts).

use crate::demux::{Demuxed, IndexedFile};
use crate::es::{BlockStore, EsDecoder, EsParams, parse_params};
use crate::pipeline::{DecodeError, DecodedInfo, ReadInfo};

pub struct StreamingDecoder {
    inner: EsDecoder,
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
        let blocks = std::mem::take(&mut indexed.blocks);
        let mut params = parse_params(indexed.init)?;
        params.info.audio_span_bytes = indexed.audio_span_bytes;
        params.warnings = indexed.warnings;
        Self::from_params(params, BlockStore::Memory { bytes, blocks })
    }

    /// Bounded-memory constructor: `indexed` came from
    /// [`crate::demux::index_container`], `store` serves block payloads.
    pub fn open_indexed(indexed: IndexedFile, store: BlockStore) -> Result<Self, DecodeError> {
        let mut params = parse_params(indexed.init)?;
        params.info.audio_span_bytes = indexed.audio_span_bytes;
        params.warnings = indexed.warnings;
        Self::from_params(params, store)
    }

    fn from_params(params: EsParams, store: BlockStore) -> Result<Self, DecodeError> {
        Ok(Self {
            inner: EsDecoder::open_store(params, store)?,
        })
    }

    pub fn warnings(&self) -> &[String] {
        &self.inner.warnings
    }

    pub fn info(&self) -> DecodedInfo {
        self.inner.info()
    }

    pub fn read_f32(&mut self, out: &mut [f32], max_frames: u64) -> Result<u64, DecodeError> {
        self.inner.pull(out, max_frames)
    }

    pub fn read_info(&self) -> ReadInfo {
        self.inner.read_info()
    }

    pub fn position(&self) -> u64 {
        self.inner.position()
    }

    /// Jump seek: LF jumps to the previous USAC independency AU (bit-exact
    /// random access; not sample-exact with the continuous decode — LPD
    /// prev-frame state and the free-running eSBR counters are not carried
    /// in the bitstream; format-inherent, see notes/ffmpeg-lav-plugin.md
    /// 4.2). Opus packets are independent and jump directly by index; the
    /// top band backs off up to 8 packets for the upsample/shift context.
    pub fn seek(&mut self, frame: u64) -> Result<(), DecodeError> {
        self.inner.seek(frame)
    }
}
