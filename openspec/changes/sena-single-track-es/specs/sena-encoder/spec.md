# Sena Encoder Specification (v2: composite assembly)

## ADDED Requirements

### Requirement: Composite block assembly
After producing the layer elementary streams (unchanged: exhale xHE-AAC LF,
Opus mid, optional Opus top per the existing profile/accounting rules), the
encoder SHALL assemble composite 20 ms blocks per the container spec:

- block count and grid from the mid Opus packet stream (one packet per
  block);
- LF AU placement by the playable-start rule (AU 0 and AU 1 in block 0,
  then at most one AU per block), with `au_index` recorded;
- top-layer packet placement by its pre-skip-adjusted playable start
  (block 0 MAY carry two packets);
- the final block MAY be partially covered by the layers; trimming is the
  decoder's playable_samples rule.

#### Scenario: Deterministic assembly
- GIVEN fixed layer streams
- THEN the composite block sequence is fully determined by the placement
  rules.

### Requirement: INIT blob and WAVEFORMATEX writing
The encoder SHALL build the SENA-INIT blob (magic `SEN2`, version 2,
profile, three-track flag, lf_rate, playable_samples, audio_sha256 v3, ASC,
OpusHead×2) and wrap it in the WAVEFORMATEX header (wFormatTag 0x534E,
48 kHz, 2 channels, nBlockAlign 1, wBitsPerSample 32). `nAvgBytesPerSec`
SHALL be computed from the actual assembled block sizes before writing
(the encoder has all packets in hand; no backpatching).

#### Scenario: Accurate average bitrate
- GIVEN a freshly encoded file
- THEN nAvgBytesPerSec*8/duration equals the true average bitrate of the
  composite track.

### Requirement: Container writing
The encoder's muxer SHALL write: single track with `A_MS/ACM`,
CodecName `Sena multilayer audio`, CodecDelay 0, DefaultDuration 20 ms,
SeekPreRoll (informational, 500 ms), 1 s clusters, a Cues element with one
CuePoint per cluster, and Info Duration equal to the playable length. It
SHALL NOT write SENA_PROFILE/SENA_VERSION/SENA_PLAYABLE_SAMPLES/
SENA_AUDIO_SHA256 tags.

#### Scenario: No identification tags
- GIVEN a freshly encoded file
- THEN it carries no SENA_* Matroska tags and is identified solely by the
  CodecPrivate.

### Requirement: Unchanged codec parameters
Profiles, crossover FIRs, bitrate accounting (`account`/`account3`), codec
subprocess parameters, pre-gain, and the three-track threshold are
unchanged by this change; only the packaging differs.

The track `CodecName` SHALL be the layout's display name:
`Sena xAAC-Opus@300` / `Sena xAAC-Opus@600` (two-layer) or
`Sena xAAC-Opus@300-Opus@15600` / `Sena xAAC-Opus@600-Opus@15600`
(three-layer).

#### Scenario: Same layers
- GIVEN the same source and settings encoded by the v1 and v2 encoders
- THEN the layer elementary streams are identical; only container framing
  differs.
