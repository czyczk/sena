# Tasks

- [x] 1. sena-core: ES framing module — WAVE_FORMAT_SENA / CodecID /
      SEN2 magic constants, SENA-INIT encode/decode, WAVEFORMATEX wrapper
      build/parse, composite block encode/decode (flags, au_index, LF2/HF2),
      AU/block placement formulas, SHA-256 v3 helper, unit tests
- [x] 2. sena-mux: single-track writer — WAVEFORMATEX CodecPrivate,
      CodecDelay 0, DefaultDuration, SeekPreRoll, 1 s clusters, Cues
      element, no SENA_* tags
- [x] 3. sena-enc: INIT blob construction (incl. sha256 v3), composite
      block assembly on the 20 ms grid, avg bytes/sec from assembled sizes;
      drop multi-track muxing
- [x] 4. sena-dec: ES core (`es.rs`) — block parse, per-layer queues,
      layer worker decode, mix, pull, reset, payload-bits accounting;
      bit-exact vs whole-file reference
- [x] 5. sena-dec: demux rewrite — single-track EBML reader, INIT
      extraction, lazy block store, block-source iterator for warmup;
      reject v1 files; keep tags/attachments
- [x] 6. sena-dec: pipeline + streaming reimplemented over the ES core;
      seek warmup (indep-AU rollback, OPUSHF backoff, DSP guards) and
      cold-start ramp
- [x] 7. sena-dec ffi: ABI v3 — keep `sena_dec_*` semantics, add
      `sena_es_*`; update header copies
- [x] 8. CLIs: senaenc/senadec build and round-trip new format;
      `--dump-tracks` via ES core
- [x] 9. foobar2000 plugin: rebuild against new staticlib; verify open,
      decode, dynamic bitrate, tags, album art on v2 files
- [x] 10. plugins/ffmpeg rewrite: delete demuxer/probe/deferral; add
      libavcodec `sena` decoder (sena_es_* wrapper), AV_CODEC_ID_SENA,
      riff tag 0x534E, codec descriptor, Makefile/registration patches in
      apply.py
- [x] 11. tests: regenerate e2e assets (reference PCM unchanged), update
      demux/pipeline/stream/ffi unit tests, golden suite, rewrite
      tests/ffmpeg gates (decode bit-exact, stock ffprobe identification,
      mkvmerge video+audio remux decode)
- [ ] 12. docs: notes updates (format v2, DS filter plan, deployment),
      justfile recipes retargeted
