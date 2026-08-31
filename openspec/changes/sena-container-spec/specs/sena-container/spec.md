# Sena Container Specification

## ADDED Requirements

### Requirement: Container format is Matroska with two audio tracks
The Sena container SHALL be Matroska (EBML). File extensions `.sena` (primary)
and `.mka` (accepted) SHALL both be recognized.

The container SHALL contain exactly two audio tracks:

- Track 1 (high-frequency): codec ID `A_OPUS`, sample rate 48000 Hz,
  2 channels, CodecPrivate = standard OpusHead packet.
- Track 2 (low-frequency): private codec ID `A_SENALF`, sample rate as
  emitted by the encoder (16000 Hz or 32000 Hz, see encoder spec),
  2 channels, CodecPrivate = AudioSpecificConfig (ASC) bytes of the
  xHE-AAC stream.

#### Scenario: Two-track layout
- GIVEN a Sena file
- THEN it contains exactly one A_OPUS track at 48 kHz and one A_SENALF track
  at the encoder-emitted rate (16 kHz or 32 kHz), both with 2 channels.

### Requirement: Container-level identification
The container SHALL include a top-level tag `SENA_PROFILE` with value `300`
or `600`, and a tag `SENA_VERSION` with the profile specification version.
These identification tags SHALL be in the first top-level Matroska `Tags`
element, which SHALL be placed before the first Cluster so that probing and
streaming do not require reading the file tail.

#### Scenario: Identification
- GIVEN a file with extension `.sena` or `.mka`
- WHEN a decoder probes it
- THEN it is treated as Sena only if SENA_PROFILE is present and supported,
  without reading past the first Cluster.

### Requirement: Alignment metadata
Each track SHALL carry its codec delay as Matroska `CodecDelay` (and
`SeekPreRoll` where applicable), in nanoseconds, using the track's sample
rate:

- LF track: `1024 * 1e9 / actual_lf_rate` ns (one 1024-sample core frame at
  the actual stream rate).
- Opus track: `pre_skip * 1e9 / 48000` ns, where `pre_skip` comes from the
  `OpusHead` CodecPrivate packet.

These values are trim amounts, not relative leads. They are informational
cross-checks: the `OpusHead` pre-skip and the 1024-frame rule are the
authority for trimming.

#### Scenario: Alignment metadata present
- GIVEN a Sena file
- THEN both tracks expose CodecDelay matching the formulas above for the
  file's actual rates.

### Requirement: Playable length metadata
The container SHALL carry a top-level tag `SENA_PLAYABLE_SAMPLES` whose
value is the playable length of the file in 48 kHz stereo frames, i.e.
the number of samples of the original source timeline represented by the
file. It is the authoritative end-of-stream (trailing) information: the
decoded and mixed output SHALL have exactly this many frames after sena-dec
has trimmed the leading padding of both tracks. Both tracks share the same
value (they encode one common timeline).

#### Scenario: Playable length present
- GIVEN a Sena file
- THEN SENA_PLAYABLE_SAMPLES equals the source length in 48 kHz frames
  and is consistent with a decoder that trims warmup on both tracks.

### Requirement: Segment duration and block timing
The Segment `Info Duration` SHALL equal the playable length
(`SENA_PLAYABLE_SAMPLES / 48000` seconds); no extra padding SHALL be
added. Track block timestamps SHALL advance by the codecs' frame
durations: 20 ms for the Opus track, and one AU duration
(1024 samples at the track's actual stream rate: 64 ms at 16 kHz,
32 ms at 32 kHz) for the LF track.

#### Scenario: Exact segment duration
- GIVEN a Sena file
- THEN Info Duration matches SENA_PLAYABLE_SAMPLES exactly, and LF AU
  timestamps advance by 64 ms (16 kHz stream) or 32 ms (32 kHz stream).

### Requirement: Cluster interleaving for streaming and seeking
The muxer SHALL interleave both tracks' clusters in presentation-time order
with a cluster duration of at most 1 second, so that decoding can start from
the first cluster pair without reading the whole file.

#### Scenario: Streaming start
- GIVEN a Sena file
- WHEN a decoder receives the file header and the first cluster pair
- THEN it can begin decoding.

### Requirement: No embedded tags in audio streams
Elementary streams SHALL be muxed without any embedded metadata tags
(artist/title/encoder strings, OpusTags comment header, or equivalent
metadata boxes). Container-level tags are managed separately and SHALL NOT
be duplicated into the tracks.

#### Scenario: Tag stripping
- GIVEN an elementary stream containing tag blocks
- WHEN muxing into Sena
- THEN tag blocks are discarded and only audio packets are copied.

### Requirement: Editable container tags
Immutable Sena tags (`SENA_PROFILE`, `SENA_VERSION`,
`SENA_PLAYABLE_SAMPLES`) SHALL stay in the first top-level Matroska `Tags`
element before the first Cluster. User-editable metadata tags SHALL be
stored in separate top-level `Tags` element(s) placed at the end of the
Segment, after the last Cluster and any Cues.

A tag writer MAY update user tags in place by voiding the old user `Tags`
element(s) with EBML `Void` element(s) of the same total encoded length,
appending the replacement `Tags` element at the Segment tail, and patching
the Segment size plus any SeekHead references. It SHALL NOT move, rewrite,
or invalidate Clusters or Cues.

The foobar2000 tag-writer implementation details are researched in
`notes/foobar2000-plugin.md`.

#### Scenario: Retag without moving clusters
- GIVEN a Sena file and an update to user metadata
- WHEN a tag writer updates the file
- THEN immutable Sena tags remain readable from the file head, cluster byte
  positions remain unchanged, and the new metadata is readable from the
  Segment tail.

### Requirement: Cue support
The muxer SHALL write cue points at regular intervals (approximately 5 s)
for fast seeking.

#### Scenario: Seek points
- GIVEN a Sena file
- WHEN seeking to an arbitrary time
- THEN a cue point exists within approximately 5 s of the target.
