# External dependency notes: ropus (Opus decoder)

Path: `../ropus`; consumed crate: `opus-decoder` (core `opus-core`).

## Integration contract
- The lib decoder is an **untrimmed PCM core**: `Decoder::decode_*` does not
  apply OpusHead pre-skip or granule-based end trim. senadec owns leading
  pre-skip trim and final `SENA_PLAYABLE_SAMPLES` truncation.
- senadec parses OpusHead itself and must validate: channels = 2, mapping
  family = 0, input sample rate = 48000. Apply `output_gain` if non-zero
  (Sena files should normally have 0).
- Invalid packet policy is decided by senadec (currently: PLC decode +
  warning, continue).

## SIMD / features
- sena-dec feature plan: `default = ["simd"]`, `simd = ["opus-decoder/simd"]`;
  consume `opus-decoder` with `default-features = false`.
- ropus repo sets `x86-64-v3` / `wasm simd128` via its own
  `.cargo/config.toml`; Cargo does **not** inherit that when ropus is used as
  a path dependency from another workspace. If the same codegen baseline is
  wanted here, add the equivalent config to the sena workspace later.

## Toolchain / reproducibility
- ropus is edition 2024 and requires Rust >= 1.88. The sena workspace has
  been moved to edition 2024 to align.
- Deliberately NOT pinned for now (pre-release, expected to keep updating).
  Observed integration-time HEAD: `9153529` (informational only).

## Validation
- ropus `VALIDATION.md`: 19/19 corpus cases bit-exact for f32, s16, s24
  against the fixed-point libopus 1.6.1 reference. sena decoder spec accepts
  the core by reference to this suite.

## Do not modify
- All issues are recorded here or raised upstream; never edit `../ropus`
  from the sena side.
