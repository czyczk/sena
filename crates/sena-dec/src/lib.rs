//! Sena decoder core.
pub mod attachments;
pub mod demux;
pub mod ffi;
pub mod output;
pub mod pipeline;
pub mod stream;
pub mod tags;

pub use demux::{Demuxed, Track, Frame};
pub use pipeline::{Decoded, DecodeError, Decoder};
