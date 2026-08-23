# Sena Container Specification

## ADDED Requirements

### Requirement: Container format is Matroska with two audio tracks
The Sena container SHALL be Matroska (EBML). File extensions `.sena` (primary)
and `.mka` (accepted) SHALL both be recognized.

The container SHALL contain exactly two audio tracks:

- Track 1 (high-frequency): codec ID `A_OPUS`, sample rate 48000 Hz,
  CodecPrivate = standard OpusHead packet.
- Track 2 (low-frequency): private codec ID `A_SENALF`, sample rate 16000 Hz,
  CodecPrivate = AudioSpecificConfig (ASC) bytes of the xHE-AAC stream.

#### Scenario: Two-track layout
- GIVEN a Sena file
- THEN it contains exactly one A_OPUS track at 48 kHz and one A_SENALF track
  at 16 kHz.

### Requirement: Container-level identification
The container SHALL include a top-level tag `SENA_PROFILE` with value `300`
or `600`, and a tag `SENA_VERSION` with the profile specification version.

#### Scenario: Identification
- GIVEN a file with extension `.sena` or `.mka`
- WHEN a decoder probes it
- THEN it is treated as Sena only if SENA_PROFILE is present and supported.

### Requirement: Alignment metadata
Each track SHALL carry its codec delay as Matroska `CodecDelay` (and
`SeekPreRoll` where applicable), in nanoseconds, using the track's sample
rate. The values SHALL match the per-profile constants defined in the
encoder specification; they are informational cross-checks because the
decoder trims warmup/pre-skip internally.

#### Scenario: Alignment metadata present
- GIVEN a Sena file
- THEN both tracks expose CodecDelay consistent with their profile.

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

### Requirement: Cue support
The muxer SHOULD write cue points at regular intervals (approximately 5 s)
for fast seeking.

#### Scenario: Seek points
- GIVEN a Sena file
- WHEN seeking to an arbitrary time
- THEN a cue point exists within approximately 5 s of the target.
