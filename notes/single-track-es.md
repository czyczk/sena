# Format v2: single-track composite ES — the DirectShow/video redesign

See also: `notes/lf-timeline-model.md` (the definitive LF AU ↔ timeline
mapping and anchor arithmetic — read before touching LF placement/seek).

Status: implemented (this supersedes the demuxer design in
`ffmpeg-lav-plugin.md`, which is kept for history).

## Why the v1 multi-track MKA had to go

- A Sena stream is the *mix* of its physical tracks, but DirectShow graph
  building connects exactly one audio pin to the render chain. A dual-input
  mixer filter is never auto-connected — players would have had to change.
- The ffmpeg `sena` demuxer + `matroska_probe` deferral assumed the whole
  file was Sena: any video MKV named `.sena` or carrying the byte string
  `SENA_PROFILE` got routed to a demuxer that emits only mixed PCM — video
  silently dropped.

## The v2 design (what actually shipped)

- Container: Matroska with **exactly one** audio track,
  `CodecID = A_MS/ACM`, `CodecPrivate = WAVEFORMATEX(wFormatTag=0x534E) +
  SENA-INIT` blob. CodecDelay = 0 (trims are ES-internal), DefaultDuration
  20 ms, informational SeekPreRoll, real Cues (1 s clusters).
- ES blocks: one composite block per 20 ms slot — counted sections:
  1 mid Opus frame always, 0..n LF AUs with absolute `au_index`, 0..n
  top-band Opus frames. Layers are placed by playable END (LF AU k ends in
  block `floor((k*unit48-1)/960)`; top packet j in block j), clamped to the
  final block at EOS, so a mid-stream block boundary never has an LF hole.
- SENA-INIT carries profile/flags/lf_rate/playable_samples/sha256(v3)/ASC/
  OpusHead×2 — identification is codec-level; no SENA_* tags, no probe
  strings, no deferral patch.

## Verified upstream behavior (the enabler)

- ffmpeg `matroskadec.c` `A_MS/ACM` → `ff_get_wav_header`: unknown
  wFormatTag keeps `codec_tag`, sets extradata = bytes past the 18-byte
  WAVEFORMATEX, `bit_rate = nAvgBytesPerSec*8`, stream kept (codec NONE
  only logs). Stock ffprobe shows `Audio: none (NS[0][0] / 0x534E), 48000
  Hz, stereo, <correct> kb/s`.
- LAV: `LAVFDemuxer.cpp:2643` keeps streams with `codec_tag != 0`;
  `LAVFAudioHelper::initAudioType` defaults the pin subtype to
  `FOURCCMap(codec_tag)` = `{0000534E-0000-0010-8000-00AA00389B71}`, first
  preferred type, WAVEFORMATEX with our cbExtra, `nAvgBytesPerSec`
  back-filled. LAV Audio never claims it; a custom transform filter
  (planned) connects by merit with zero patches anywhere.
- ffmpeg `SeekPreRoll` only propagates to `codecpar->seek_preroll`; the
  demuxer does NOT deliver preroll packets and LAV ignores
  `AV_PKT_FLAG_DISCARD` — so seek warmup is the decoder's job (below).

## sena-dec ES core (ABI v3)

- `EsDecoder` (crates/sena-dec/src/es.rs): block-fed state machine
  (container block store OR host push queue), lanes decode concurrently.
  Container API (`sena_dec_*`) is preserved and reimplemented over it;
  the fb2k plugin and senadec CLI work unchanged (both rebuilt clean:
  windows x64/arm64ec, mac arm64/x86_64).
- Push API (`sena_es_*`): `open(init_blob)`, `push(ts, block)`,
  `pull(f32)`, `finish`, `reset`, `set_output_start`, `warmup_seek(path,
  target)`, `get_info/read_info/stats`, `close`.
- Seek modes:
  - container seek: block-jump + per-lane skip counters; LF jumps to prev
    USAC independency AU (documented format-inherent inexactness class),
    Opus by index, top band with 8-packet context. Bit-exact vs v1.
  - warmup seek (DS filter path): shadow file reader decodes the warmup
    window; re-delivered blocks deduped by timestamp. Bit-exact vs
    container seek.
  - cold-start ramp (ffmpeg/ffplay): LF silent until first independency
    AU. Re-anchoring reconstructs LF grid start from `(block ts, au_index)`
    alone (by-end placement makes it shift-invariant — works even with
    CLI-shifted pts). Converges to ~2e-5 in benign anchors; pathological
    anchors (zero-history LPD/eSBR) can diverge more — inherent to
    zero-history starts; warmup is the quality path.
  - join fix of record: the ramp join must compare the AU start against
    the CURRENT production point (`tf_seek_base + produced`), not the
    stream start — otherwise a late join lands the whole LF lane ~1 chunk
    late permanently.

## ffmpeg plugin (libavcodec/senadec.c)

- `FFCodec` over the ES push API. `FF_CODEC_DECODE_CB` return value = bytes
  consumed (NOT 0 — the first version returned 0 and ffmpeg re-sent the
  same packet forever). Drain is a packet with `pkt->data == NULL`.
- pts anchoring: first packet after init/flush is the grid origin; deltas
  from it stay exact under the CLI `-ss` tsfixup (which shifts pts to
  arbitrary/non-grid/negative values — u64 underflow killed the first
  version).
- Frame pts from the core's absolute position (`sena_es_get_read_info`) +
  `frame->time_base` must be set (audio_ts_process needs it).
- Tags/Attachments must be at the HEAD (right after Tracks): matroskadec
  at probe time never scans to EOF, so tail-positioned tags/art are
  invisible to ffprobe/LAV/player UIs. The tag/art rewriter now inserts at
  `tracks_end` (mid-file Insert ⇒ the io path falls back to the buffered
  whole-file rewrite; also fixed a rewind bug in that fallback).
- Pre-existing bug found by this: `FILE_NAME_ID` constant was 0x45E0
  instead of 0x466E (Matroska FileName) — matroskadec rejected the
  attachment as incomplete.

## Verification matrix (all green)

- cargo test --workspace (13+30+21+22+2 tests)
- v1→v2 remux fixtures: decode bit-exact vs v1 reference PCM (7/7),
  container seeks bit-exact (multiple targets, 2t+3t)
- ffmpeg (patched build): full decode bit-exact (7/7); `-ss` at 1.0/5.5/
  9.97/15.003/15.1/19.99: exact length, lag 0, settled ≤ 2e-4 after the
  ramp; stock ffprobe identifies `matroska,webm` + `sena` codec, correct
  bitrate/duration; tags/art visible; clean failure without libsena_dec.
- fb2k plugin: windows x64/arm64ec + mac arm64/x86_64 build clean.
