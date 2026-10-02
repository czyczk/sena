# Sena Container Specification (v2: single-track composite elementary stream)

## ADDED Requirements

### Requirement: Container format is Matroska with exactly one composite audio track
The Sena container SHALL be Matroska (EBML), DocType `matroska`. File
extensions `.sena` (primary) and `.mka` (accepted) SHALL both be
recognized.

The container SHALL contain exactly one audio track ("the Sena track"):

- `TrackType` = 2 (audio)
- `CodecID` = `A_MS/ACM`
- `CodecName` = the layout's display name: `Sena xAAC-Opus@300` /
  `Sena xAAC-Opus@600` (two-layer), `Sena xAAC-Opus@300-Opus@12000` /
  `Sena xAAC-Opus@600-Opus@12000` (three-layer)
- `CodecPrivate` = the WAVEFORMATEX wrapper defined below
- `Audio.SamplingFrequency` = 48000, `Audio.Channels` = 2
- `CodecDelay` = 0 (all leading trims are internal to the elementary
  stream; block timestamps are the playable timeline)
- `DefaultDuration` = 20 ms in Matroska ticks
- `SeekPreRoll` = informational hint for foreign demuxers (SHALL be
  written; SHOULD be 500 ms in ticks)

The two-layer (@300/@600) and three-layer (@300@12000/@600@12000) profiles
are unchanged in content; the layers SHALL be packed into the single
composite elementary stream defined here. A Sena file SHALL NOT contain
`A_SENALF` or `A_OPUSHF` tracks.

#### Scenario: Single-track layout
- GIVEN a Sena file
- THEN it contains exactly one audio track with CodecID `A_MS/ACM` and a
  Sena WAVEFORMATEX CodecPrivate, and no other audio tracks.

#### Scenario: Video coexistence
- GIVEN a Matroska file with video track(s) and one Sena audio track
  (produced e.g. by mkvmerge from a `.sena` file)
- THEN the Sena track's CodecID, CodecPrivate, and block payloads survive
  the remux verbatim, and the file plays video plus Sena audio through a
  stock Matroska demuxer plus the Sena decoder.

### Requirement: WAVEFORMATEX wrapper
`CodecPrivate` SHALL be a little-endian WAVEFORMATEX structure (18 bytes,
including `cbSize`) immediately followed by the SENA-INIT blob:

| field | value |
|---|---|
| wFormatTag (u16) | `0x534E` (`WAVE_FORMAT_SENA`) |
| nChannels (u16) | 2 |
| nSamplesPerSec (u32) | 48000 |
| nAvgBytesPerSec (u32) | total ES block payload bytes / playable duration, rounded up |
| nBlockAlign (u16) | 1 |
| wBitsPerSample (u16) | 32 |
| cbSize (u16) | byte length of the following SENA-INIT blob |

This is the Matroska `A_MS/ACM` escape hatch used as specified: the track
identifies itself by an ACM format tag, and the DirectShow media subtype is
deterministically `FOURCCMap(0x534E)` =
`{0000534E-0000-0010-8000-00AA00389B71}`.

`nAvgBytesPerSec` SHALL be the true average of the whole Sena track (sum of
all composite block payload bytes, excluding EBML/SimpleBlock framing,
divided by the playable duration in seconds), so that container-agnostic
tools report the correct average bitrate.

#### Scenario: Deterministic media subtype
- GIVEN a Sena track parsed by a WAVEFORMATEX-aware demuxer
- THEN the reported audio subtype/tag is 0x534E, the extra data is exactly
  the SENA-INIT blob, and the reported average bitrate equals the track
  average.

### Requirement: SENA-INIT blob
The SENA-INIT blob SHALL be (all integers little-endian):

| field | type | meaning |
|---|---|---|
| magic | 4 bytes | ASCII `SEN2` |
| version | u16 | 2 |
| profile | u8 | 0 = @300, 1 = @600 |
| flags | u8 | bit0: three-track layout |
| lf_rate | u32 | LF stream rate: 16000 (@300) or 32000 (@600) |
| playable_samples | u64 | playable length in 48 kHz stereo frames |
| audio_sha256 | 32 bytes | see content-hash requirement |
| lf_asc_len | u16 | |
| lf_asc | bytes | xHE-AAC AudioSpecificConfig |
| opus_head_len | u16 | |
| opus_head | bytes | mid-layer OpusHead (contains pre-skip) |
| hf_head_len | u16 | three-track only; absent when flags bit0 = 0 |
| hf_head | bytes | three-track only: top-layer OpusHead (16 kHz stream; pre-skip in 48 kHz units per RFC 7845) |

The blob SHALL be rejected unless magic/version match, profile is 0 or 1,
lf_rate matches the profile, and the head lengths consume exactly the blob.

This blob replaces the v1 identification tags: `SENA_PROFILE`,
`SENA_VERSION`, `SENA_PLAYABLE_SAMPLES`, and `SENA_AUDIO_SHA256` SHALL NOT
be written as Matroska tags.

#### Scenario: Codec-level identification
- GIVEN a Matroska file
- WHEN a decoder inspects the audio track's CodecPrivate
- THEN it is Sena iff wFormatTag = 0x534E and the SENA-INIT blob validates,
  without scanning file bytes for marker strings.

### Requirement: Composite ES block format
Each SimpleBlock of the Sena track SHALL carry one composite block covering
one 20 ms slot of the playable timeline (block *i* covers
[i*20 ms, (i+1)*20 ms)). The block timestamp SHALL be i*20 ms. Lacing SHALL
NOT be used; every block is a keyframe-flagged SimpleBlock.

Block payload layout (all integers little-endian):

```
u8   flags = 0 (reserved; decoders SHALL reject nonzero)
u16  mid_len,  mid bytes          ; exactly one mid-layer Opus frame, always present
u8   lf_count (0..4)
if lf_count > 0: u24 au_index     ; absolute index of the first LF AU
per LF AU:       u24 lf_len, lf bytes
u8   hf_count (0..4)
per HF packet:   u16 hf_len, hf bytes
```

Placement is by playable END, clamped to the final block:

- The mid layer contributes exactly one 20 ms Opus frame per block: block
  *i* carries mid packet *i*.
- LF AUs (1024 samples at lf_rate): AU *k* covers playable
  [(k-1)*1024, k*1024) samples at lf_rate for k ≥ 1; AU 0 is the warmup AU
  covering [-1024, 1024) (its preroll half is trimmed). AU k lands in the
  block containing its last playable sample,
  `floor((k*unit48 - 1) / 960)` where `unit48 = 1024 * 48000 / lf_rate`,
  clamped to the final block. By-end placement keeps the AU covering any
  block boundary inside that block, so a cold start there has no LF hole.
- Top-layer (three-track) Opus packet *j* lands in block *j* exactly (valid
  while its pre-skip < 960, which the decoder SHALL enforce), clamped to
  the final block.
- `au_index` re-anchors the LF timeline when decoding starts mid-stream:
  together with the block timestamp it reconstructs the AU's grid start
  without any absolute reference (shift-invariant).
- All sections of a block belong to the same 20 ms slot; layer queues are
  consumed in arrival order.

#### Scenario: Deterministic layer placement
- GIVEN the encoded layer streams
- THEN every block's contents are fully determined by the placement rules
  above, and a decoder can reconstruct each layer's timeline from block
  order, au_index, and the INIT blob alone.

### Requirement: Playable length, duration, and end of stream
`playable_samples` in SENA-INIT is the sole authoritative length: the
decoded, mixed output SHALL be exactly that many 48 kHz stereo frames, and
a decoder SHALL fail rather than pad if the decoded output is short.

The Segment `Info Duration` SHALL equal `playable_samples / 48000` seconds;
no padding SHALL be added.

#### Scenario: Exact length
- GIVEN a Sena file
- THEN Info Duration matches playable_samples exactly, and block count is
  ceil((playable_samples + mid pre-skip) / 960) for the mid grid.

### Requirement: Clustering, cues, and streaming
Blocks SHALL be packed into Clusters of at most 1 second, in presentation
order, first cluster starting at 0. The muxer SHALL write a Cues element
with one CuePoint per cluster (CueTime = cluster timestamp,
CueTrackPositions: CueTrack = 1, CueClusterPosition = cluster byte offset).
Cues MAY follow the last cluster; a SeekHead is OPTIONAL.

Decoding SHALL be able to start from the header plus the first cluster
without reading the rest of the file.

#### Scenario: Seek points
- GIVEN a Sena file
- WHEN seeking to an arbitrary time with a cue-aware demuxer
- THEN a cue point exists at the nearest preceding cluster boundary
  (≤ 1 s away).

### Requirement: Audio content hash (domain v3)
`audio_sha256` SHALL be the SHA-256 of:

1. the ASCII domain separator `SENA encoded audio sha256 v3\0`;
2. `u32` little-endian length of the SENA-INIT blob, then the blob (with
   the audio_sha256 field itself zeroed);
3. `u64` little-endian composite block count;
4. for each block in decode order: `u32` little-endian byte length, then
   the block payload bytes.

Container framing, timestamps, tags, and cluster layout SHALL NOT affect
the hash.

#### Scenario: Content hash
- GIVEN a Sena file
- THEN audio_sha256 equals a recomputation over the file's INIT blob and
  block payloads, and changes whenever any layer's encoded bytes change.

### Requirement: No embedded tags in audio streams
Elementary streams SHALL be muxed without embedded metadata (OpusTags
comment header, m4a metadata boxes, etc.). Container-level tags SHALL NOT
be duplicated into the track.

#### Scenario: Tag stripping
- GIVEN an elementary stream containing tag blocks
- WHEN muxing into Sena
- THEN tag blocks are discarded.

### Requirement: Editable container tags
User-editable metadata tags SHALL be stored in a top-level `Tags` element
at the head of the Segment, immediately after the Tracks element and
before the first Cluster; album art SHALL use Matroska Attachments placed
likewise. Head placement is required because mainstream Matroska readers
(matroskadec/LAV at probe time) never scan to EOF.

A tag writer MAY update user tags/attachments in place by voiding old
elements, inserting the replacement at the head insertion point, and
patching the Segment size; it SHALL NOT modify, reorder, or corrupt
Clusters, Cues, or the track's CodecPrivate (which carries the immutable
Sena identification). Cluster byte positions shift as a block by exactly
the inserted length.

#### Scenario: Retag at the head
- GIVEN a Sena file and a user-metadata update
- WHEN the tag writer updates the file
- THEN the SENAINIT blob and cluster contents/order remain intact,
  clusters shift as a block by the inserted length, and the new metadata
  is visible at probe time.
