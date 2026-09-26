# Sena Decoder Specification (three-track HF delta)

## MODIFIED Requirements

### Requirement: Processing chain
For three-track files the decoder SHALL decode all three tracks and mix
`lf48 + mid + top` (each restored by 1/PRE_GAIN) after applying each
track's leading trim (LF 1024-core warmup; per-track Opus pre-skip). The
`A_OPUSHF` track is decoded at 16 kHz, upsampled back to 48 kHz with the
zero-phase rational resampler, and shifted back up to the 15.6 kHz+ band
(analytic signal times exp(+j*2*pi*15600*t), real part) with the carrier
phase locked to the playable timeline (phase 0 at playable frame 0); the
pre-skip trim is applied on the 48 kHz timeline (the OpusHead pre-skip
counts 48 kHz units per RFC 7845). The streaming decoder SHALL consume all
tracks concurrently (chunk production gated on the least-ready track,
including the shift chain's context needs) and SHALL NOT require the whole
file or any full-track decode before emitting PCM. Seeking jumps the opus
tracks by packet index (per-track pre-skip) and the LF track by USAC
independency AU, as before; for `A_OPUSHF` the jump backs off a few packets
so the zero-phase upsampler and the shift-up filter have real context at
the target frame.

#### Scenario: streaming equals whole-file
- GIVEN a three-track asset
- THEN chunked `read_f32` output equals the whole-file pipeline decode
  (bit-exact on the same platform), and memory stays bounded.

#### Scenario: bitrate reporting
- THEN per-read payload bits include all three tracks' blocks.
