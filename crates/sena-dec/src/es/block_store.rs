//! Composite block payload sources: whole-file memory or lazy positioned
//! reads through the host's io callbacks.

use crate::demux::BlockRef;
use crate::pipeline::DecodeError;

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
    pub(crate) fn blocks(&self) -> &[BlockRef] {
        match self {
            BlockStore::Memory { blocks, .. } => blocks,
            BlockStore::Lazy { blocks, .. } => blocks,
        }
    }

    /// Fetch the payload of block `index` (one bounded allocation per call).
    pub(crate) fn read(&mut self, index: usize) -> Result<Vec<u8>, DecodeError> {
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
