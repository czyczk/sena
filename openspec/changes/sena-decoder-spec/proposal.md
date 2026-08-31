# Change: Sena decoder (senadec)

## Why
Define the normative decoder: leading/trailing trim, delay handling,
upsampling, mixing, output formats, reference-asset acceptance, and the
library boundaries for plugins.

## What Changes
- Decoder library API and processing chain (untrimmed cores + sena-dec trims).
- Output formats of the CLI decoder, raw piping, and deterministic s16 TPDF.
- Acceptance criteria for the external Rust codec cores and reference assets.
- Playable-length (trailing) trimming, driven by SENA_PLAYABLE_SAMPLES.
- Gapless-playback rules: per-file length conservation and independent,
  sample-continuous consecutive files.
- C-compatible plugin ABI, including dynamic-bitrate accounting and an
  in-place Matroska tag writer for plugin hosts.
- Foobar2000 shim reference notes (`notes/foobar2000-plugin.md`).

## Impact
- Encoder's alignment constants must be matched.
- Plugin layer (foobar2000, ffmpeg, DirectShow, VLC, wasm) consumes the
  same core; the foobar2000 component is `foo_input_sena` (not `foo_pd`).
