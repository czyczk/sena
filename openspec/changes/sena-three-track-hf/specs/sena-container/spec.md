# Sena Container Specification (three-track HF delta)

## MODIFIED Requirements

### Requirement: Container format is Matroska with audio tracks
A Sena file is Matroska with exactly one `A_OPUS` track, exactly one
`A_SENALF` track, and optionally (three-track layout, `SENA_PROFILE`
`<lf>@15600`) exactly one `A_OPUSHF` track. The `A_OPUSHF` track carries
an Opus stream (OpusHead private data, 16000 Hz, 2 channels, 20 ms
packets) holding the 15600 Hz+ band SSB-shifted down to baseband, with
its own CodecDelay matching its OpusHead pre-skip (48 kHz units). The
track's Matroska sampling rate is 16000 Hz. `A_OPUSHF` without the
three-track profile tag (or vice versa) is a format error.

#### Scenario: three-track file
- GIVEN `SENA_PROFILE=600@15600`
- THEN the container holds A_OPUS + A_SENALF + A_OPUSHF and decoding mixes
  all three.

#### Scenario: mismatched layout
- GIVEN `SENA_PROFILE=600` and an A_OPUSHF track
- THEN the decoder rejects the file.

### Requirement: Audio content hash
Three-track files hash all three elementary streams under the domain
string `SENA encoded audio sha256 v2\0` (stream order A_OPUS, A_SENALF,
A_OPUSHF); two-track files keep the v1 domain and bytes unchanged.
