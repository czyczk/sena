# Sena Decoder Specification (v2: ES core)

## ADDED Requirements

### Requirement: Elementary-stream decode core
The decoder SHALL be structured around an elementary-stream (ES) core that
operates on individual composite blocks, independent of the container:

- initialization from a SENA-INIT blob (container spec) — wrong magic,
  version, profile/lf_rate mismatch, or truncated head sections SHALL be
  rejected;
- `push(timestamp, block)` accepts one composite block in timeline order
  and queues its layer payloads;
- `pull(out)` yields mixed, trimmed 48 kHz stereo f32 frames in timeline
  order;
- `reset()` returns the core to the just-initialized state.

The ES core SHALL decode the layers on concurrent workers (LF xHE-AAC
decode + resample, mid Opus, and in the three-track layout top Opus +
upsample + SSB shift-up) and mix with `1/PRE_GAIN` gain, truncating to
`playable_samples`.

#### Scenario: Push/pull equivalence
- GIVEN a Sena file
- WHEN its blocks are pushed individually and PCM pulled in chunks of any
  size
- THEN the output is bit-identical to whole-file decoding.

### Requirement: Leading trims (unchanged layer rules)
The ES core SHALL trim leading padding inside the elementary stream:

- xHE-AAC core: drop 1024 samples per channel at the core's output rate
  before resampling.
- Opus cores: drop the `OpusHead` pre-skip samples per channel (mid: 48 kHz
  units; top: 48 kHz units per RFC 7845 despite the 16 kHz stream).

After trimming, all bands SHALL sit on the original 48 kHz timeline; block
timestamps and `au_index` are the alignment authority (no CodecDelay is
consulted).

#### Scenario: Natural alignment
- GIVEN a Sena file decoded with leading trim
- THEN the layers are time-aligned without additional search, on any
  decode start at block 0.

### Requirement: Mid-stream start and re-anchoring
A decoder starting mid-stream (first block is not block 0) SHALL
re-anchor each layer without container assistance:

- mid layer: the pushed block timestamp gives the grid index directly;
- LF layer: the first AU's absolute index comes from the block's
  `au_index` field;
- top layer: the packet index is derived from the block timestamp and the
  top layer's pre-skip by inverting the container placement rule.

If the first delivered LF AU is not an independency AU and no warmup data
is available, the LF layer SHALL output silence until the first
independency AU arrives (bit 7 of the first AU payload byte); the other
layers decode normally. This ramp SHALL be bounded by the stream's
independency spacing.

#### Scenario: Cold start ramp
- GIVEN a block sequence starting at an arbitrary cluster boundary without
  warmup
- THEN mid/top layers decode immediately and correctly, and LF fades in at
  the first independency AU.

### Requirement: Seek warmup from a block source
When random access to the container is available, the ES core SHALL
support full-quality seek warmup: given a block source and a target frame,
decode-and-discard the window from the warmup anchor to the first block
that will be delivered live, where the anchor is the earliest of:

- the previous LF independency AU, minus one context AU (or AU 0 with a
  zero prefix),
- the DSP guard window (resampler/Hilbert context) before the target,
- the top-layer 8-packet context back-off (three-track layout).

After warmup, `output_start` SHALL be set to the target frame so `pull`
yields exactly from the target. Opus-layer seeks remain sample-exact; LF
seek output is format-inherently not sample-exact (LPD state, free-running
eSBR counters), as in v1.

#### Scenario: Warmed seek equals reference
- GIVEN a Sena file and a seek target at a cue boundary
- WHEN seeking with warmup from the container block source
- THEN the output after the target equals whole-file decode at that offset
  under the v1 seek semantics.

### Requirement: Playable length and gapless playback
The ES core SHALL truncate output to `playable_samples` and SHALL fail with
a decode error rather than pad if the decoded output is short. Files SHALL
NOT depend on cross-file decoder state; consecutive files concatenate
sample-continuously within codec error.

#### Scenario: Exact output length
- GIVEN a Sena file with playable_samples = N
- WHEN decoded
- THEN the playable output has exactly N frames.

### Requirement: Bit accounting
The ES core SHALL account payload bits per pulled chunk from the composite
block section sizes, and SHALL expose cumulative per-layer byte counts, so
hosts can display the combined and per-layer bitrates (dynamic and
average).

#### Scenario: Dynamic bitrate
- GIVEN a decoder pulling chunks
- THEN each chunk reports the payload bits of the blocks it covers, and
  per-layer sums are available.

### Requirement: Container reader
The container-level reader SHALL parse the Matroska structure, require
exactly one Sena track per the container spec, extract the SENA-INIT blob,
and lazily fetch block payloads. It SHALL reject v1 multi-track files
(clean break). Tags and Attachments handling is unchanged; there are no
immutable Sena tags anymore (identification lives in CodecPrivate).

#### Scenario: v1 rejection
- GIVEN a v1 two-track Sena file
- WHEN opened by the new reader
- THEN it fails with a clear unsupported-format error.

### Requirement: C ABI (v3)
The shared library SHALL keep the v2 container API (`sena_dec_open`,
`sena_dec_read_f32`, `sena_dec_seek`, `sena_dec_probe_info`, tags/art
functions) with unchanged signatures and semantics, reimplemented over the
ES core, and SHALL add the ES push/pull API (`sena_es_open` takes the bare
SENA-INIT blob — a host strips the 18-byte WAVEFORMATEX shell itself,
which is exactly what ffmpeg extradata carries):

```
int  sena_es_open(const uint8_t *init_blob, size_t len, SenaEs **out, char *err, size_t err_len);
int  sena_es_push(SenaEs *es, const uint8_t *block, size_t len, uint64_t ts_frames);
int  sena_es_pull(SenaEs *es, float *interleaved, uint64_t frames, uint64_t *out_frames);
int  sena_es_reset(SenaEs *es);
int  sena_es_set_output_start(SenaEs *es, uint64_t frame);
int  sena_es_warmup_seek(SenaEs *es, const char *path, uint64_t target_frame);
int  sena_es_get_stats(SenaEs *es, SenaEsStats *stats);
void sena_es_close(SenaEs *es);
```

`sena_es_warmup_seek` opens the same file as a shadow block source and
performs the seek-warmup requirement; it MAY fail on non-local sources, in
which case the host falls back to the cold-start ramp. `SENA_DEC_ABI_VERSION`
SHALL be 3.

#### Scenario: ABI coexistence
- GIVEN a host using the v2 container entry points and a host using
  sena_es_*
- THEN both decode identically from the same file.
