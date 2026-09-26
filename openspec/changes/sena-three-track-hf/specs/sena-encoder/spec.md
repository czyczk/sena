# Sena Encoder Specification (three-track HF delta)

## ADDED Requirements

### Requirement: Three-track layout at >=256 kbit/s
For nominal totals >= 256 kbit/s the encoder SHALL split the 600 Hz-high
band a second time at 15600 Hz (the Opus b19 edge at 48 kHz / 20 ms frames)
and produce three tracks:

- `A_SENALF`: unchanged exhale LF encode at the profile's deduction.
- `A_OPUS` (mid): 600 Hz - 15600 Hz at `total - deduct - 64` kbit/s.
- `A_OPUSHF` (top): the 15600 Hz+ band, SSB-shifted down to baseband and
  carried as a 16 kHz stream at a fixed 64 kbit/s nominal.

Rationale for the shift: a track whose content sits only above 15.6 kHz
starves under the codec's content-blind band allocation (the empty low
bands keep their share and the coded band set collapses); shifted to
baseband, the same content is coded normally and the 64 kbit/s nominal is
actually spent on it.

This applies to both profiles (@300/@600) and both opus modes
(`--opus-senav` and `--opus-original`); the opus mode selects the binary
used for both opus tracks. The three codec subprocesses SHALL run
concurrently.

#### Scenario: 256 kbit/s senav encode
- GIVEN a stereo source, `--profile 600 --opus-senav 256`
- THEN exhale codes the LF band (32 kbit/s deducted), the mid band is
  encoded by opusenc-senav at 160 kbit/s, the shifted top band by
  opusenc-senav at 64 kbit/s, and `SENA_PROFILE` is `600@15600`.

#### Scenario: totals above 256
- GIVEN a total of 320 kbit/s with the three-track layout
- THEN the extra budget goes to the mid track (320 - 32 - 64 = 224 kbit/s);
  the LF deduction and the 64 kbit/s top track stay fixed.

### Requirement: HF crossover filter
The 15600 Hz split SHALL use a linear-phase FIR low-pass with subtractive
complement, Kaiser beta 9.0, 2001 taps, -6 dB cutoff at 15480 Hz and a
240 Hz transition (stopband at 15600 Hz), so the mid track carries
essentially nothing at or above b19 and the top band carries b19+b20.
Mid + top SHALL reconstruct the 600 Hz-high band exactly.

#### Scenario: Complementary reconstruction at 15600 Hz
- GIVEN the mid and top band signals
- THEN mid + top equals the 600 Hz-high band within floating-point noise.

### Requirement: Top-band shift chain
The top band SHALL be shifted down by the split frequency before encoding:
the analytic signal (windowed Hilbert FIR, 8001 taps, Kaiser beta 9,
zero-phase aligned) is multiplied by exp(-j*2*pi*15600*t) and the real part
is resampled 48 kHz -> 16 kHz with the zero-phase rational resampler. The
carrier phase SHALL be locked to the source timeline (phase 0 at the first
input frame) so the decoder can restore the band coherently. The shifted
stream is what `A_OPUSHF` carries; the shift/resample roundtrip (without
the codec) SHALL restore the band with only transition-band ripple at the
carrier edges.

#### Scenario: shift roundtrip
- GIVEN band-limited content above 15600 Hz, shifted down and back up
- THEN the reconstruction matches the original top band within the Hilbert
  ripple, and the band reappears at its original frequencies.

### Requirement: SENA_PROFILE tag for the three-track layout
Three-track files SHALL carry `SENA_PROFILE` = `<lf>@<hf>` where `<lf>` is
`300` or `600` and `<hf>` is `15600`. Two-track files keep the plain
numeric tag. `SENA_VERSION` remains `1`.

#### Scenario: profile tag round trip
- GIVEN a three-track file
- THEN `SENA_PROFILE` parses to (profile, Some(15600)) and the decoder
  requires the `A_OPUSHF` track; GIVEN a two-track file, an `A_OPUSHF`
  track is rejected.

### Requirement: opus-topband-stereo control
senaenc SHALL accept `--opus-topband-stereo <1-500>` and pass the value
unmodified to the opusenc-senav **mid** encode via `AUDIFF_TOPBAND_STEREO`.
When opus-senav is active and the nominal total is in [192, 256) kbit/s,
the default value SHALL be the opus budget (total minus deduction). The
flag SHALL be ignored with a stderr warning under `--opus-original`. The
`A_OPUSHF` encode SHALL never receive the knob.

#### Scenario: default at 192
- GIVEN `--opus-senav 192` with profile @600
- THEN opusenc-senav runs with `AUDIFF_TOPBAND_STEREO=160`.

#### Scenario: explicit override
- GIVEN `--opus-senav 256 --opus-topband-stereo 200`
- THEN the mid encode receives 200 and the top encode receives no knob.
