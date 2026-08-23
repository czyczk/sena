# Change: Sena container format

## Why
Sena files must carry two audio streams (a low-frequency xHE-AAC stream and a
high-frequency Opus stream) with decoder-side alignment, streaming-friendly
layout, and unambiguous identification.

## What Changes
- Define the Matroska-based container: track layout, codec identifiers,
  alignment metadata, cluster interleaving, extension policy.
- Define the tag-stripping policy for elementary streams.

## Impact
- Decoder: reads CodecDelay metadata for cross-checks; relies on hard-coded
  per-profile alignment constants.
- Encoder: muxes the two tracks; strips embedded tags.
