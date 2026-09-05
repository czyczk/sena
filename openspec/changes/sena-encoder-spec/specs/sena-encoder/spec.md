# Sena Encoder Specification

## ADDED Requirements

### Requirement: Profiles
Sena defines two profiles, both with a fixed linear-phase FIR crossover:

- `xAAC-Opus@300`: crossover 300 Hz; low-frequency codec configured at
  16 kHz input, exhale non-eSBR preset 1 (nominal 64 kbit/s, measured
  ~20 kbit/s actual on low-frequency-only content).
- `xAAC-Opus@600`: crossover 600 Hz; low-frequency codec configured at
  32 kHz input, exhale non-eSBR preset 5 (nominal 128 kbit/s, measured
  ~32 kbit/s actual).

When no profile is specified, the encoder SHALL default to `xAAC-Opus@600`.

#### Scenario: Profile default
- GIVEN no profile argument
- THEN the encoder uses xAAC-Opus@600.

### Requirement: Channel policy
The encoder SHALL support stereo input as P0. Mono input is a planned
future mode and SHALL be encoded as true mono tracks (one channel per
codec track), not as duplicated stereo. Input with more than two channels
is explicitly out of scope.

#### Scenario: Channel policy
- GIVEN a stereo source
- THEN the encoder encodes both tracks as two-channel streams.
- GIVEN a mono or multichannel source during the P0 milestone
- THEN the encoder fails with a clear unsupported-channel error.

### Requirement: Crossover filter
The crossover SHALL be a linear-phase FIR low-pass with subtractive
complement (high-pass = delayed input minus low-pass output):

- sample rate 48000 Hz
- cutoff fc = 300 or 600 Hz per profile
- transition width = 0.2 * fc
- taps N = ceil(10 / (transition / fs)) rounded to odd
- window: Kaiser, beta = 9.0

Both bands SHALL share the same filter delay; the sum of the two bands SHALL
reconstruct the original signal exactly (identity up to the common delay).

#### Scenario: Complementary reconstruction
- GIVEN the two band signals
- THEN low + high equals the delayed original within floating-point noise.

### Requirement: Pre-gain pad
Both bands SHALL be attenuated by a fixed pre-gain of -4 dB (0.631) before
encoding and restored (+4 dB) after decoding. This is a profile constant
protecting codec inputs from FIR overshoot.

#### Scenario: Pad symmetry
- GIVEN encoder pad p
- THEN decoder restores 1/p exactly.

### Requirement: Zero-phase rational resampler
The encoder SHALL use `sena_dsp::Resampler` (pure Rust) for all sample-rate
changes. The product SHALL NOT use soxr or rubato.

The normative design is: symmetric windowed-sinc, zero-phase rational ratio,
Kaiser window beta = 9.0, passband at 0.98 * min(Nyquist) when upsampling
and 0.90 * min(Nyquist) when downsampling, taps =
ceil(12 / normalized transition) rounded to odd, processed block-wise with
guard samples so the result is independent of block partitioning.

#### Scenario: Resampler acceptance
- GIVEN the resampler test suite
- THEN impulse responses are symmetric around the corresponding output
  sample (zero phase), block-partitioned results equal whole-buffer
  results, and sine fidelity error is below 3e-4 for the supported rates.

### Requirement: Input rate normalization
The encoder SHALL accept lossless sources at any sample rate (e.g. 44100,
48000, 96000 Hz) and resample the input to 48 kHz with the zero-phase
rational resampler before the crossover split (the FIR tables and the
alignment constants are defined at 48 kHz). Resampling SHALL NOT introduce
a time offset: the zero-phase resampler aligns the 48 kHz output to the
input timeline. The normalized length in 48 kHz frames SHALL be
`round(src_frames * 48000 / src_rate)`.

#### Scenario: 44.1 kHz source
- GIVEN a 44100 Hz stereo source
- WHEN encoded
- THEN the input is normalized to 48 kHz with no timing offset and the
  encode passes the same alignment checks as a 48 kHz source.

### Requirement: Low-frequency input resampling
The low-frequency band SHALL be resampled from 48 kHz to the profile's
input rate with the zero-phase rational resampler (`sena_dsp::Resampler`,
symmetric windowed-sinc, block-wise; no fractional group delay): 16 kHz for
@300, 32 kHz for @600. The encoder's core rate follows the input rate
deterministically (16 kHz in -> 16 kHz out; preset >= 5 floors the core at
32 kHz); the stored track rate is whatever the encoder emits (16 kHz or
32 kHz).

#### Scenario: Stored rate
- GIVEN a Sena encode at any profile
- THEN the low-frequency track in the container is sampled at the rate the
  encoder actually emitted.

### Requirement: Bitrate accounting
Bitrate accounting SHALL follow these rules (rule B):

- Hard minimum total bitrate: the profile's xHE-AAC allotment plus a 32
  kbit/s Opus floor - 64 kbit/s for @600, 56 kbit/s for @300. Lower
  requests SHALL be rejected.
- Below 128 kbit/s a plain Opus encode is recommended instead; senaenc
  SHALL refuse to encode unless `--bypass-recommendations` is given.
- Deduction for the xHE-AAC track (its measured average spend): 24 kbit/s
  for @300, 32 kbit/s for @600. The xHE-AAC encoder parameters themselves
  are unchanged by this number.
- Remaining budget goes to the Opus track as its nominal VBR bitrate.
- Opus' own VBR float above the nominal parameter (approximately 16
  kbit/s) is accepted, so the delivered total lands one float step above
  the request (e.g. 160k requested at @600 -> ~176k delivered).

#### Scenario: 160 kbit/s with @300
- GIVEN total 160 kbit/s and profile @300
- THEN xHE-AAC is allotted ~24 kbit/s and Opus the remainder.

#### Scenario: 100 kbit/s request
- GIVEN total 100 kbit/s at any profile
- THEN senaenc refuses and recommends a plain Opus encode, unless
  `--bypass-recommendations` is given.

### Requirement: Opus encoder selection
- If the requested total bitrate is <= 192 kbit/s, the standard `opusenc`
  binary (libopus >= 1.6.1) SHALL be used by default.
- If the total is > 192 kbit/s, the tuned binary `opusenc-senav` SHALL be
  used by default.
- Flags `--opus-original` and `--opus-senav` SHALL override the default.
- `opusenc-senav --version` output SHALL contain the marker `Opus SenaV`.

#### Scenario: Version marker
- GIVEN the tuned binary
- THEN its version string contains "Opus SenaV".

### Requirement: Binary requirements
senaenc SHALL require, in the same directory or on the search path:
`exhale` version >= 1.2.2, and either `opusenc` (>= 1.6.1) or `opusenc-senav`
(marked "Opus SenaV"). senaenc invokes them as subprocesses; it SHALL NOT
link or embed their code.

#### Scenario: Missing binary
- GIVEN a missing or outdated required binary
- THEN senaenc fails with a clear error naming the binary and the required version.

### Requirement: Alignment constants
The encoder SHALL write Matroska `CodecDelay` as the leading-padding trim
amount, not as a relative lead between tracks:

- LF track: `1024 * 1e9 / actual_lf_rate` ns. Equivalent at 48 kHz:
  `1024 * 48000 / actual_lf_rate` samples (typically 3072 for a 16 kHz
  stream and 1536 for a 32 kHz stream).
- Opus track: `pre_skip * 1e9 / 48000` ns, where `pre_skip` comes from the
  emitted `OpusHead`.

Both values SHALL be written from these formulas at mux time, without any
measurement. After each core's leading padding has been trimmed, the Opus
track sits at timeline 0 and the LF track sits at timeline 0 after
upsampling.

#### Scenario: Constant delays
- GIVEN a profile and an actual LF stream rate
- THEN the recorded CodecDelay equals the formula value for that actual
  rate without any measurement at encode time.

### Requirement: Playable length
The encoder SHALL record the input length as the `SENA_PLAYABLE_SAMPLES`
container tag. For a source already at 48 kHz, the value is
`src_frames`; for other source rates it is
`round(src_frames * 48000 / src_rate)`, i.e. the normalized 48 kHz frame
count produced by input rate normalization. Encoding SHALL NOT change the
timeline: after the decoder has applied the documented trims, the playable
output SHALL be exactly as long as the normalized input. The encoder feeds
the codecs with the deterministically padded band signals and does not
further crop the output; the playable length is the single source of truth
for where the playable output ends.

#### Scenario: Length conservation
- GIVEN an input of N source frames at rate R
- THEN the Sena file carries SENA_PLAYABLE_SAMPLES =
  round(N * 48000 / R) and a decoded playable output of exactly that
  length.

### Requirement: Block timing
The LF track's AU timestamps SHALL advance by one AU duration
(1024 samples at the actual stream rate: 64 ms at 16 kHz, 32 ms at
32 kHz); the Opus track's timestamps SHALL advance by 20 ms. The Segment
`Info Duration` SHALL equal the playable length. This guarantees that the
container's time axis matches the decoded timeline and that cluster
interleaving stays correct for streaming and seeking.

#### Scenario: Correct frame timestamps
- GIVEN a Sena file
- THEN consecutive LF AU timestamps differ by 64 ms (16 kHz stream) or
  32 ms (32 kHz stream), and Info Duration equals the playable length.

### Requirement: Encoder validation
A golden-output suite SHALL verify: FIR coefficients match the reference
design exactly; crossover reconstruction identity; resampler quality;
container parseability and delay metadata; end-to-end decode matches the
reference pipeline within tolerance.

`assets/e2e` references were produced by an archived external pipeline
(soxr HQ, AOSP libxaac, opusdec); they are final validation answers, not a
product pipeline to reproduce internally. Non-bit-exact differences SHALL
be attributed to documented acceptable causes before release.

#### Scenario: Golden suite
- GIVEN the golden suite
- THEN a passing run is required before any senaenc release.
