# Sena Decoder Specification

## ADDED Requirements

### Requirement: Leading trim
The decoder SHALL obtain untrimmed PCM from the codec cores and trim the
leading padding in the sena-dec integration layer:

- xHE-AAC core (`rxaac-dec-lib`): drop 1024 samples per channel at the
  core's actual output rate before resampling (3072 samples at 48 kHz for
  a 16 kHz stream, 1536 samples at 48 kHz for a 32 kHz stream).
- Opus core (`opus-decoder` / ropus): drop the `OpusHead` `pre-skip`
  samples per channel at 48 kHz.

The decoder SHALL validate that each core's reported output rate and channel
count match its container track before trimming. After trimming, both bands
SHALL sit on the original 48 kHz timeline; the golden check measures a
cross-correlation lag of 0 with a tolerance of +/- 8 samples at 48 kHz.

#### Scenario: Natural alignment
- GIVEN a Sena file decoded with leading trim
- THEN the two decoded streams are time-aligned without any additional
  search.

### Requirement: Playable length (trailing trim)
After the two trimmed bands are resampled, restored, and summed, the
decoder SHALL truncate the output to the `SENA_PLAYABLE_SAMPLES` value
from the container (48 kHz stereo frames; missing or invalid value is a
container error). The playable output SHALL be exactly that long and SHALL
not be extended by any codec-side trailing padding.

If the trimmed, decoded, and mixed output is shorter than
`SENA_PLAYABLE_SAMPLES`, the decoder SHALL fail with a decode error; it
SHALL NOT pad or extend the output.

#### Scenario: Exact output length
- GIVEN a Sena file with SENA_PLAYABLE_SAMPLES = N
- WHEN decoded
- THEN the playable output has exactly N frames.

### Requirement: Gapless playback
Sena SHALL support gapless playback: each file is a complete, independent
segment of the source timeline.

- Per-file: the decoded playable output length equals the source segment
  length exactly; no silence or garbage remains at either end after the
  documented trims.
- Consecutive files: files SHALL NOT depend on cross-file decoder state
  (no encoder-side state carries over), so two consecutive files decode
  independently and their outputs concatenate sample-continuously; the
  concatenation equals decoding the concatenated source segments within
  codec error.

The decoder SHALL NOT insert, remove, or fade any samples beyond the
trims specified above.

#### Scenario: Gapless sequence
- GIVEN two Sena files encoded from consecutive source segments
- WHEN decoded and concatenated
- THEN the result equals the concatenated source segments in length and
  sample alignment (within codec error).

### Requirement: Processing chain
senadec SHALL process as follows:

1. Demux the two tracks, identified by codec ID (`A_OPUS` / `A_SENALF`),
   not by track number.
2. Decode each track with its core as untrimmed PCM.
3. Apply the leading trim described above.
4. Resample the low-frequency track to 48 kHz with `sena_dsp::Resampler`.
5. Multiply both 48 kHz tracks by `1 / 0.631` (the exact inverse of the
   encoder `PRE_GAIN`, not an approximate `10^(4/20)`).
6. Sum the two 48 kHz streams.
7. Truncate the sum to `SENA_PLAYABLE_SAMPLES` (playable length).
8. Convert and write the output in the selected format.

The decoder SHALL NOT apply spectral shaping, loudness normalization, or
fades; output-format dithering is a quantization step, not spectral shaping.

Per-frame error policy: an xHE-AAC AU decode error SHALL insert silence for
the core's reported output frame length and continue with a warning; an
invalid Opus packet SHALL be decoded through packet-loss concealment and
continue with a warning; malformed CodecPrivate / ASC / OpusHead data SHALL
be a fatal error.

#### Scenario: Sum equals reconstruction
- GIVEN a compliant encoder output
- THEN decoder output reconstructs the input within the codec error
  accepted by the reference-assets requirement.

### Requirement: Zero-phase rational resampler
The product decoder SHALL use `sena_dsp::Resampler` (pure Rust). The product
SHALL NOT use soxr or rubato.

The normative design is: symmetric windowed-sinc, zero-phase rational ratio,
Kaiser window beta = 9.0, passband at 0.98 * min(Nyquist) when upsampling and
0.90 * min(Nyquist) when downsampling, taps = ceil(12 / normalized transition)
rounded to odd, processed block-wise with guard samples so the result is
independent of block partitioning.

#### Scenario: Resampler acceptance
- GIVEN the resampler test suite
- THEN impulse responses are symmetric around the corresponding output sample
  (zero phase), block-partitioned results equal whole-buffer results, and
  sine fidelity error is below 3e-4 for the supported Sena rates.

### Requirement: Reference assets and end-to-end acceptance
`assets/e2e` contains `.sena` files plus 16-bit and 32-bit float WAV
references. The references were produced by the archived reference pipeline:
soxr (HQ) resampling, AOSP libxaac (`xhedec`), and `opusdec`, then gain
restore, sum, and truncate. The archive documents how the assets were made;
the product decoder is not required to reproduce that pipeline internally.

senadec output SHALL NOT be required to match those references bit-exactly.
When it does not match, the difference SHALL be scientifically attributed to
documented acceptable causes (different resampler design, float paths, or
dithering); unexplained differences are bugs.

For every reference asset, senadec SHALL pass:

- output length equals `SENA_PLAYABLE_SAMPLES`;
- post-trim cross-correlation lags are 0 +/- 8 samples at 48 kHz;
- mono-mix correlation against the source is greater than 0.99;
- low-frequency envelope sigma (the metric in `tests/golden/validate.py`)
  differs from the archived reference decode by no more than 0.02 dB for
  that same asset.

The archived reference is the authority for source-vs-decode quality; a
per-asset delta is used instead of one global absolute threshold because
different source material has different reference sigma (the existing
assets range from 0.03 dB to 0.14 dB).

#### Scenario: Reference asset gate
- GIVEN the assets/e2e reference set
- THEN a passing run, or a written analysis for every non-bit-exact delta,
  is required before any senadec release.

### Requirement: Output formats
The CLI decoder SHALL support these `--format` values:

- `wav-f32` (default), `wav-s24`, `wav-s16`;
- `raw-f32`, `raw-s24`, `raw-s16`: headerless, interleaved, little-endian
  PCM on stdout (`s24` packed as 3 bytes per sample); logs and diagnostics
  SHALL go to stderr only.

`--dump-tracks <prefix>` SHALL write two 48 kHz stereo f32 WAV files,
`<prefix>.lf.wav` and `<prefix>.hf.wav`, after each track's trim, upsample,
and gain restore but before summing (for debugging).

`-o <file>` writes to a file; `-o -` or an omitted `-o` writes to stdout.

PCM conversion SHALL be:

- f32: IEEE float, written as-is;
- s24: `q = clamp(round(x * 8388608.0), -8388608, 8388607)`, no dither;
- s16: `q = clamp(round(x * 32768.0 + (u1 + u2 - 1.0)), -32768, 32767)`,
  where `u1, u2` are independent uniform [0, 1) values and
  `(u1 + u2 - 1.0)` is TPDF dither at +/- 1 LSB. s16 dither is ON by
  default using a deterministic fixed-seed PRNG; `--dither none` disables it.

#### Scenario: Format selection
- GIVEN a Sena file
- WHEN decoding with a selected output format flag
- THEN the output matches the requested format, bit depth, and dither policy.

### Requirement: Codec core acceptance
The decoder SHALL consume the external Rust cores without modifying them:

- Opus core: `opus-decoder` from ropus. Acceptance is by reference to
  ropus `VALIDATION.md` (19/19 corpus cases bit-exact for f32, s16, and s24
  against the fixed-point libopus 1.6.1 reference).
- xHE-AAC core: `rxaac-dec-lib`. It SHALL pass its own validation suite and
  the Sena lock vectors: the `assets/e2e` set plus small-AU and warmup-prone
  streams, with leading lag equal to one 1024-sample core frame (+/- 8
  samples at 48 kHz after resampling) and zero unexplained deviations from
  its reference class.

Streaming semantics SHALL be verified by feeding lock vectors in windows
(one AU / one Opus packet, or 100 ms windows) and checking that concatenated
windowed output is consistent with whole-file decode within the declared
tolerance.

Integration notes and issues for the external cores are tracked outside the
spec (see `notes/`).

#### Scenario: Acceptance gate
- GIVEN a core candidate or an update to ropus / rxaac-dec
- THEN all lock vectors and the reference-asset gate pass before the core
  may be used in any decoder release.

### Requirement: Dynamic bitrate reporting
The decoder SHALL track, for each successful `read_f32` block, the payload
bits of both tracks whose presentation interval contributes to the returned
playable frames, and SHALL expose that accounting to realtime hosts so they
can display instantaneous bitrate (foobar2000 `%bitrate%` dynamic display).

`payload_bits` is the sum of Opus packet payload bits and xHE-AAC AU payload
bits mapped to the block's playable frame interval after clipping the
leading-padding and trailing-padding intervals. A packet whose interval
spans multiple blocks SHALL have its bits attributed in proportion to the
overlapping frame counts. Padding-only packets contribute zero. After a
seek, accounting SHALL restart from the seek target; hosts average over
their display update interval and SHALL NOT infer bitrate from raw
`SenaDecIo` read-callback byte counts (the decoder may buffer/read ahead).

#### Scenario: Dynamic bitrate
- GIVEN a Sena file with varying per-packet payload sizes
- WHEN a host reads consecutive blocks and averages the reported
  payload bits over each display interval
- THEN the displayed bitrate changes with the content and is not a
  file-wide average.

### Requirement: C-compatible plugin ABI
The decoder SHALL expose a C-compatible ABI so that plugin hosts
(foobar2000, ffmpeg, DirectShow, VLC, wasm) can consume the core without
re-implementing the chain. The foobar2000 shim design (input service choice,
trim ownership, dynamic-bitrate integration, tag writing, and the
Windows/macOS architecture matrix) is researched in
`notes/foobar2000-plugin.md` and SHALL be followed when the shim is built.

The P0 ABI is:

```c
#define SENA_DEC_ABI_VERSION 1

typedef struct SenaDec SenaDec;

typedef struct {
    void    *user_data;
    int64_t (*read)(void *user_data, void *buf, uint64_t len);
    int64_t (*seek)(void *user_data, int64_t offset, int whence);
    int64_t (*tell)(void *user_data);
    uint64_t (*size)(void *user_data);
} SenaDecIo;

typedef struct {
    uint32_t sample_rate;      /* 48000 */
    uint32_t channels;         /* 2 (P0) */
    uint64_t playable_frames;  /* SENA_PLAYABLE_SAMPLES */
    uint32_t profile;          /* 300 or 600 */
    uint32_t sena_version;     /* 1 */
} SenaDecInfo;

typedef struct {
    uint64_t start_frame;      /* first playable frame of the last block */
    uint64_t frames;           /* frames returned by the last read_f32 */
    uint64_t payload_bits;     /* payload bits attributed to that interval */
} SenaDecReadInfo;

typedef struct {
    void    *user_data;
    int64_t (*read)(void *user_data, void *buf, uint64_t len);
    int64_t (*write)(void *user_data, const void *buf, uint64_t len);
    int64_t (*seek)(void *user_data, int64_t offset, int whence);
    int64_t (*tell)(void *user_data);
    uint64_t (*size)(void *user_data);
} SenaFileIo;

typedef struct {
    const char *key;           /* UTF-8, non-empty */
    const char *value;         /* UTF-8, may be empty */
} SenaMetaEntry;

int  sena_dec_open(const SenaDecIo *io, SenaDec **out,
                   char *err, size_t err_len);
/* Parse/validate the container and return SenaDecInfo without decoding
   audio; used by fast info-read paths (playlist probing, tag reads). */
int  sena_dec_probe_info(const SenaDecIo *io, SenaDecInfo *info,
                         char *err, size_t err_len);
void sena_dec_close(SenaDec *dec);
int  sena_dec_get_info(SenaDec *dec, SenaDecInfo *info);
int  sena_dec_read_f32(SenaDec *dec, float *interleaved,
                       uint64_t frames, uint64_t *out_frames);
int  sena_dec_get_read_info(SenaDec *dec, SenaDecReadInfo *info);
int  sena_dec_seek(SenaDec *dec, uint64_t frame);
const char *sena_dec_version(void);

/* In-place Matroska tag editing: void old user Tags elements, append the
   replacement at the Segment tail, patch Segment size and SeekHead refs.
   SENA_PROFILE / SENA_VERSION / SENA_PLAYABLE_SAMPLES are preserved. */
int  sena_file_write_tags(const SenaFileIo *io,
                          const SenaMetaEntry *entries, uint32_t count,
                          char *err, size_t err_len);
int  sena_file_remove_tags(const SenaFileIo *io,
                           char *err, size_t err_len);

/* User-tag enumeration. Returned strings are valid until sena_tags_close. */
typedef struct SenaTags SenaTags;
int  sena_file_read_tags(const SenaFileIo *io, SenaTags **out,
                         char *err, size_t err_len);
uint32_t sena_tags_count(const SenaTags *tags);
const char *sena_tags_key(const SenaTags *tags, uint32_t index);
const char *sena_tags_value(const SenaTags *tags, uint32_t index);
void sena_tags_close(SenaTags *tags);
```

ABI semantics:

- input is callback-based; `seek`/`tell`/`size` may be NULL for sequential
  streams;
- `read_f32` returns interleaved 48 kHz stereo f32 in caller memory; 0
  frames at EOF;
- `get_read_info` returns the accounting for the last successful `read_f32`
  block; after an EOF block it SHALL return `start_frame =
  playable_frames`, `frames = 0`, `payload_bits = 0`;
- `seek` targets a playable frame index in [0, playable_frames);
- tag functions operate on an existing file opened read/write by the host
  and do not decode audio or alter clusters/cues;
- one handle is not thread-safe for concurrent use; independent handles are;
- no global mutable state; errors are returned as negative codes plus an
  optional message buffer;
- the CLI is a thin client of this ABI.

#### Scenario: ABI use
- GIVEN a plugin host
- THEN it can decode a Sena file through the C ABI without touching
  container or codec internals, and it can report dynamic bitrate and
  rewrite container tags through the same shared core.

### Requirement: Input validation and channel policy
senadec P0 SHALL support stereo only. A file with any other channel count
SHALL fail with an unsupported-channel error. Mono is a planned future input
mode; more than two channels is explicitly out of scope.

The decoder SHALL reject a file as a fatal error when:

- `SENA_PROFILE` is missing or not `300` / `600`;
- `SENA_VERSION` is not the supported version `1`;
- `SENA_PLAYABLE_SAMPLES` is missing, non-numeric, or zero;
- track codec IDs / rates / channel counts do not match the container
  specification;
- the LF track rate is not 16000 Hz or 32000 Hz, or the ASC is invalid;
- `OpusHead` mapping family is not 0 or input sample rate is not 48000.

Container `CodecDelay` is a cross-check only. Expected values are
`1024 * 1e9 / lf_rate` ns for the LF track and `pre_skip * 1e9 / 48000` ns
for the Opus track; a mismatch SHALL produce a warning and decoding SHALL
continue using the OpusHead and 1024-frame rules as authority.

#### Scenario: Unsupported input
- GIVEN a file missing SENA_PROFILE or using an unsupported channel count
- THEN senadec fails with a clear unsupported-format error before emitting
  audio.
