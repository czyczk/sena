# Sena Decoder Specification (three-track HF delta)

## MODIFIED Requirements

### Requirement: Processing chain
For three-track files the decoder SHALL decode all three tracks and mix
`lf48 + mid + top` (each restored by 1/PRE_GAIN) after applying each
track's leading trim (LF 1024-core warmup; per-track Opus pre-skip). The
streaming decoder SHALL consume all tracks concurrently (chunk production
gated on the least-ready track) and SHALL NOT require the whole file or
any full-track decode before emitting PCM. Seeking jumps the opus tracks
by packet index (per-track pre-skip) and the LF track by USAC independency
AU, as before.

#### Scenario: streaming equals whole-file
- GIVEN a three-track asset
- THEN chunked `read_f32` output equals the whole-file pipeline decode
  (bit-exact on the same platform), and memory stays bounded.

#### Scenario: bitrate reporting
- THEN per-read payload bits include all three tracks' blocks.
