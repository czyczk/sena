# Sena Decoder Specification

## ADDED Requirements

### Requirement: Warmup trimming
Each codec core SHALL trim its own leading warmup before emitting audio:

- xHE-AAC core: drop the leading warmup output (1024 samples at 16 kHz for
  preset 1, 512 samples at 16 kHz for preset 5).
- Opus core: apply the standard OpusHead pre-skip.

After trimming, both decoded streams SHALL be aligned to the original
timeline with zero residual (verified by measurement).

#### Scenario: Natural alignment
- GIVEN a Sena file decoded with warmup trimming
- THEN the two decoded streams are time-aligned without any additional
  search.

### Requirement: Processing chain
senadec SHALL process as follows:

1. Demux the two tracks (container-level CodecDelay is a cross-check only).
2. Decode each track with its core (warmup/pre-skip trimmed).
3. Resample the low-frequency track from 16 kHz to 48 kHz (rubato,
   windowed-sinc).
4. Restore the -4 dB pre-gain pad (+4 dB) on both tracks.
5. Sum the two 48 kHz streams into the output.

The decoder SHALL NOT apply any spectral shaping; the crossover exists only
on the encoder side.

#### Scenario: Sum equals reconstruction
- GIVEN a compliant encoder output
- THEN decoder output reconstructs the input within codec error.

### Requirement: Output formats
The CLI decoder SHALL support: 32-bit float WAV, 24-bit and 16-bit
fixed-point WAV, raw PCM (each bit depth), and dual-track raw output for
debugging.

#### Scenario: Format selection
- GIVEN a Sena file
- WHEN decoding with a selected output format flag
- THEN the output matches the requested format and bit depth.

### Requirement: Codec core acceptance
Ported codec cores SHALL pass:

- lock-vector conformance (reference vectors including small-AU and
  warmup-prone streams),
- bit-exact or tolerance-matched output against the reference cores,
- streaming semantics consistent with windowed input feeding.

#### Scenario: Acceptance gate
- GIVEN a ported core candidate
- THEN all lock vectors pass and output matches the reference core within
  the declared tolerance before the core may be used in any decoder.

### Requirement: Plugin consumption
The decoder SHALL expose a C-compatible ABI so that plugin hosts
(foobar2000, ffmpeg, DirectShow, VLC, wasm) can consume the core without
re-implementing the chain.

#### Scenario: ABI use
- GIVEN a plugin host
- THEN it can decode a Sena file through the C ABI without touching
  container or codec internals.
