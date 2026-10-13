//! One Opus decode lane (the 48 kHz mid layer or the 16 kHz top layer).

use std::collections::VecDeque;

use opus_decoder::{DecodeMode, Decoder as OpusDecoder};

use crate::pipeline::{DecodeError, MAX_OPUS_FRAME_SAMPLES};

/// One Opus decode lane's borrowed state for a single packet decode.
pub(crate) struct OpusLane<'a> {
    pub dec: &'a mut OpusDecoder,
    pub name: &'static str,
    pub errors: &'a mut usize,
    pub raw: &'a mut VecDeque<f32>,
}

/// Decode one Opus packet with PLC fallback and append the PCM to the
/// lane's raw queue.
pub(crate) fn decode_opus_packet(
    lane: &mut OpusLane,
    data: &[u8],
    t_ns: u64,
) -> Result<(), DecodeError> {
    let mut buf = vec![0.0f32; MAX_OPUS_FRAME_SAMPLES * 2];
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
    n = n.min(MAX_OPUS_FRAME_SAMPLES);
    lane.raw.extend(buf[..n * 2].iter().copied());
    Ok(())
}
