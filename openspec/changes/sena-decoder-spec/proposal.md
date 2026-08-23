# Change: Sena decoder (senadec)

## Why
Define the normative decoder: warmup trimming, delay handling, upsampling,
mixing, output formats, and the library boundaries for plugins.

## What Changes
- Decoder library API and processing chain.
- Output formats of the CLI decoder.
- Acceptance criteria for the ported codec cores.

## Impact
- Encoder's alignment constants must be matched.
- Plugin layer (foobar2000, ffmpeg, DirectShow, VLC, wasm) consumes the
  same core.
