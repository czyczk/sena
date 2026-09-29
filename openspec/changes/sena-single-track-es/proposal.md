# Change: Single-track composite elementary stream (`A_MS/ACM` + WAVE_FORMAT_SENA)

## Why
The two/three-physical-track Matroska layout cannot coexist with video in
DirectShow-based playback, and the current probe/deferral machinery is
structurally unsafe:

- A Sena playable stream is the *mix* of its physical tracks, but LAV
  Splitter exposes each track as an independent output pin and DirectShow
  graph building connects exactly one audio pin to the render chain. A
  dual-input mixer filter would require changing player graph-building
  logic, which is not feasible.
- The ffmpeg `sena` demuxer plugin plus the `matroska_probe` deferral patch
  assume the whole file is Sena. Any video MKV named `*.sena` or carrying
  the `SENA_PROFILE` byte string is routed to a demuxer that emits only the
  mixed PCM stream — the video is silently dropped.
- The format has no codec-level identity: detection relies on a substring
  scan for `SENA_PROFILE` in the probe buffer.

Verified upstream behavior (FFmpeg matroskadec/riffdec, LAVFilters
LAVFDemuxer/LAVFStreamInfo/LAVFAudioHelper) shows a clean escape hatch: an
`A_MS/ACM` track whose WAVEFORMATEX carries an unregistered `wFormatTag`
survives end to end — ffmpeg keeps `codec_tag`, creates the stream, and
puts the bytes after the 18-byte WAVEFORMATEX into extradata; LAV keeps the
stream (codec_tag != 0), exposes an output pin whose first preferred media
type is `MEDIATYPE_Audio / FOURCCMap(wFormatTag) / FORMAT_WaveFormatEx`
with the full WAVEFORMATEX+cbExtra format block, and reports
`nAvgBytesPerSec` from our header. A stock LAV therefore hands a single
composite audio pin to a single custom decoder filter, with no patches to
players, LAV, or ffmpeg, and video tracks are unaffected.

## What Changes
- The Sena container becomes Matroska with **exactly one audio track**:
  `CodecID = A_MS/ACM`, `CodecPrivate = WAVEFORMATEX(wFormatTag =
  WAVE_FORMAT_SENA) + SENA-INIT blob`. The two-track and three-track
  *layer* content is unchanged; the layers are packed into one composite
  elementary stream.
- New composite ES block format: one SimpleBlock per 20 ms grid slot,
  carrying the mid Opus frame, optionally one or two LF xHE-AAC AUs (with
  the absolute AU index of the first), and optionally the top-band Opus
  frame(s). Block order remains the timing authority; layer timelines are
  derived from the AU index / block grid / per-layer pre-skip rules.
- `SENA_PROFILE` / `SENA_VERSION` / `SENA_PLAYABLE_SAMPLES` /
  `SENA_AUDIO_SHA256` tags move into the SENA-INIT blob (profile, flags,
  lf_rate, playable_samples, sha256, ASC, OpusHead×2). Container
  identification is codec-level; the tag-based probe and the
  `matroska_probe` deferral are deleted.
- CodecDelay on the track is 0: all leading trims are internal to the
  elementary stream (OpusHead pre-skip, 1024-sample LF warmup), block
  timestamps are already the playable timeline. The muxer now writes Cues
  (previously required but never emitted), plus DefaultDuration and an
  informational SeekPreRoll.
- Audio content hash domain v3: SHA-256 over the SENA-INIT blob and all
  composite block payloads in decode order.
- Decoder core gains an ES-level push/pull API (`sena_es_*`, ABI v3):
  decode from individual blocks, seek-warmup from a block source, and a
  bounded ramp fallback (LF silent until the first independency AU) when no
  random access is available. The container API (`sena_dec_*`) is preserved
  and reimplemented over the ES core, so the foobar2000 plugin and CLI are
  unaffected at the source level.
- The ffmpeg plugin changes from a *demuxer* to a *decoder*: a small
  libavcodec AVCodec wrapping `sena_es_*`, plus `AV_CODEC_ID_SENA` and a
  `ff_codec_wav_tags` entry for 0x534E. Stock ffmpeg demuxes the container;
  the patched ffmpeg/LAV decodes it. The old demuxer, probe helper, and
  deferral patch are removed.
- Bitrate reporting: combined average via WAVEFORMATEX `nAvgBytesPerSec`
  (flows to ffmpeg `bit_rate` and the DirectShow pin), live combined bitrate
  via the pin's byte counters, and per-layer rates via the ES bit
  accounting (block section sizes) and `SenaDecReadInfo.payload_bits`.

## Impact
- **Incompatible format change**: v1 two/three-track files are rejected by
  the new decoder; the encoder emits only v2. (Accepted: no compatibility
  requirement.)
- sena-mux writes a single track, Cues, SeekPreRoll; sena-enc assembles
  composite blocks; sena-dec is restructured around the ES core with the
  demuxer reduced to a thin single-track reader; ffi gains `sena_es_*`
  (ABI v3) while keeping `sena_dec_*`.
- plugins/ffmpeg is rewritten (avcodec decoder; demuxer/probe/deferral
  deleted). foobar2000 plugin recompiles unchanged against the new core.
- A future DirectShow `sena_audio.ax` transform filter (not in this change)
  consumes the ES API; stock LAV Splitter supplies the pin.
- Video MKV files carrying a Sena track play correctly in LAV-based players
  once the decoder filter exists, and remuxing with mkvmerge is safe
  (block payloads are opaque to remuxers).
- e2e assets must be regenerated (v1 files invalid); reference PCM decodes
  remain valid because layer coding is unchanged.
