# Change: Sena encoder (senaenc)

## Why
Define the normative encoder: profiles, DSP constants, bitrate accounting,
binary orchestration, and validation gates.

## What Changes
- Profile definitions and constants (FIR, resampling, pad, delays).
- Bitrate accounting rules.
- Binary requirements (exhale, opusenc / opusenc-senav) and version markers.
- Output container production.

## Impact
- Decoder must implement matching constants.
