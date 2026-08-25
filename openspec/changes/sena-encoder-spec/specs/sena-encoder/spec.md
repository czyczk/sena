# Sena Encoder Specification

## ADDED Requirements

### Requirement: Profiles
Sena defines two profiles, both with a fixed linear-phase FIR crossover:

- `xAAC-Opus@300`: crossover 300 Hz; low-frequency codec configured at
  16 kHz input, exhale non-eSBR preset 1 (nominal 64 kbit/s, measured
  ~20 kbit/s actual on low-frequency-only content).
- `xAAC-Opus@600`: crossover 600 Hz; low-frequency codec configured at
  16 kHz input, exhale non-eSBR preset 5 (nominal 128 kbit/s, measured
  ~32 kbit/s actual).

When no profile is specified, the encoder SHALL default to `xAAC-Opus@600`.

#### Scenario: Profile default
- GIVEN no profile argument
- THEN the encoder uses xAAC-Opus@600.

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

### Requirement: Low-frequency input resampling
The low-frequency band SHALL be resampled from 48 kHz to the profile's
input rate with high-quality resampling (rubato, windowed-sinc, sinc length
>= 256): 16 kHz for @300, 32 kHz for @600. The encoder's core rate follows
the input rate deterministically (16 kHz in -> 16 kHz out; preset >= 5
floors the core at 32 kHz); the stored track rate is whatever the encoder
emits (16 kHz or 32 kHz).

#### Scenario: Stored rate
- GIVEN a Sena encode at any profile
- THEN the low-frequency track in the container is sampled at the rate the
  encoder actually emitted.

#### Scenario: Stored rate
- GIVEN a Sena encode at any profile
- THEN the low-frequency track in the container is sampled at 16 kHz.

### Requirement: Bitrate accounting
Bitrate accounting SHALL follow these rules (rule B):

- Total requested bitrate below 160 kbit/s: encode plain Opus only.
- Deduction for the xHE-AAC track: 16 kbit/s for @300, 24 kbit/s for @600.
- Remaining budget goes to the Opus track as its nominal VBR bitrate.
- Actual spend of the xHE-AAC track may exceed the deduction by up to the
  measured float (approximately 4-8 kbit/s); this float is accepted.

#### Scenario: 160 kbit/s with @300
- GIVEN total 160 kbit/s and profile @300
- THEN xHE-AAC is allotted ~16 kbit/s and Opus the remainder.

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
The encoder SHALL record per-profile track delays (in 48 kHz samples):

- @300: xHE-AAC track leads by 3072 samples (one 1024-sample warmup frame
  at 16 kHz).
- @600: xHE-AAC track leads by 1536 samples (512 warmup samples at 16 kHz).

The Opus track delay is 0 after pre-skip. These constants SHALL be written
as CodecDelay metadata and SHALL match the decoder's warmup trimming.

#### Scenario: Constant delays
- GIVEN a profile
- THEN the recorded delay equals the fixed per-profile constant without any
  measurement at encode time.

### Requirement: Encoder validation
A golden-output suite SHALL verify: FIR coefficients match the reference
design exactly; crossover reconstruction identity; resampler quality;
container parseability and delay metadata; end-to-end decode matches the
reference pipeline within tolerance.

#### Scenario: Golden suite
- GIVEN the golden suite
- THEN a passing run is required before any senaenc release.
