# Change: Sena container format

## Why
Sena files must carry two audio streams (a low-frequency xHE-AAC stream and a
high-frequency Opus stream) with decoder-side alignment, streaming-friendly
layout, and unambiguous identification.

## What Changes
- Define the Matroska-based container: track layout, codec identifiers,
  alignment metadata, cluster interleaving, extension policy.
- Define the tag-stripping policy for elementary streams.
- Define container-tag placement: immutable Sena tags early, user-editable
  tags at the Segment tail, with a void-and-append in-place update rule
  (see `notes/foobar2000-plugin.md`).
- Define playable-length metadata (SENA_PLAYABLE_SAMPLES) and exact
  segment/block timing rules (the basis for gapless playback).
- CodecDelay formulas use each track's actual rate, not per-profile fixed
  constants.

## Impact
- Decoder: reads CodecDelay metadata for cross-checks; relies on the
  OpusHead pre-skip and the 1024-frame LF trim as authority.
- Encoder: muxes the two tracks; strips embedded tags.
