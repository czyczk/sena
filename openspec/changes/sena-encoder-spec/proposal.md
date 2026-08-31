# Change: Sena encoder (senaenc)

## Why
Define the normative encoder: profiles, DSP constants, bitrate accounting,
binary orchestration, and validation gates.

## What Changes
- Profile definitions and constants (FIR, resampling, pad, delays).
- Channel policy: stereo P0, mono planned, >stereo out of scope.
- Zero-phase rational resampler (`sena_dsp::Resampler`) as the only product
  resampler (no soxr / rubato in the product).
- Bitrate accounting rules.
- Binary requirements (exhale, opusenc / opusenc-senav) and version markers.
- Output container production.
- Playable-length recording (SENA_PLAYABLE_SAMPLES = normalized source
  length) and exact block timing (one LF AU duration at the actual stream
  rate).

## Impact
- Decoder must implement matching constants.
