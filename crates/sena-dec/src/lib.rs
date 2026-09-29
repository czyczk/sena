//! Sena decoder core.
pub mod attachments;
pub mod demux;
pub mod es;
pub mod ffi;
pub mod output;
pub mod pipeline;
pub mod stream;
pub mod tags;

pub use demux::{Block, Demuxed, Track};
pub use pipeline::{Decoded, DecodeError, Decoder};
