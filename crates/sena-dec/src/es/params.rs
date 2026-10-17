//! INIT blob parsing: static per-file parameters distilled into
//! decoder-ready form.

use rxaac_dec_lib::asc::AudioSpecificConfig;
use sena_core::es;
use sena_core::es::Init;

use crate::pipeline::{DecodeError, DecodedInfo, SAMPLE_RATE, parse_opus_head};

/// The top layer's pre-skip must stay below one block for the by-end
/// placement to line up (OpusHead pre-skip is 312 in practice).
const MAX_TF_PRESKIP: u16 = 959;

/// Static per-file parameters distilled from the INIT blob.
pub struct EsParams {
    pub init: Init,
    pub asc: AudioSpecificConfig,
    /// Mid-layer OpusHead (Opus mid only).
    pub mid_head: Option<crate::pipeline::OpusHead>,
    /// Mid-layer xHE-AAC config (xHE mid only, v3).
    pub mid_xhe_asc: Option<AudioSpecificConfig>,
    /// Top-band OpusHead (Opus top only).
    pub tf_head: Option<crate::pipeline::OpusHead>,
    /// Top-band xHE-AAC config (xHE top only, v4).
    pub tf_xhe_asc: Option<AudioSpecificConfig>,
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
    let (tf_head, tf_xhe_asc) = match &init.hf_head {
        Some(h) => match init.hf_codec {
            sena_core::HfCodec::Opus => {
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
                (Some(head), None)
            }
            sena_core::HfCodec::Xhe => {
                let asc = AudioSpecificConfig::parse(h)
                    .map_err(|e| DecodeError::Format(format!("top ASC parse: {e}")))?;
                (None, Some(asc))
            }
        },
        None => (None, None),
    };
    if init.three_track != (tf_head.is_some() || tf_xhe_asc.is_some()) {
        return Err(DecodeError::Format(
            "three-track flag/head mismatch in SENA-INIT".into(),
        ));
    }
    if init.hf_codec == sena_core::HfCodec::Xhe
        && init.mid_codec != sena_core::MidCodec::Xhe
    {
        return Err(DecodeError::Format(
            "xHE top band without an xHE mid (44.1 kHz output requires both)".into(),
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
            sample_rate: init.output_rate(),
            channels: 2,
            // playable_samples rides the 48 kHz source timeline in the
            // header (block grid + hash stability); the decoder reports it
            // in output-rate units (44.1 kHz streams: floor x441/480).
            playable_frames: init.playable_samples * init.output_rate() as u64 / SAMPLE_RATE as u64,
            profile: init.profile.crossover_hz() as u32,
            three_track: init.three_track,
            mid_codec: init.mid_codec,
            hf_codec: init.hf_codec,
            sena_version: es::ES_VERSION as u32,
            audio_sha256,
            audio_span_bytes: 0, // filled by the container layer
        },
        asc,
        mid_head,
        mid_xhe_asc,
        tf_head,
        tf_xhe_asc,
        init,
        warnings: Vec::new(),
    })
}
