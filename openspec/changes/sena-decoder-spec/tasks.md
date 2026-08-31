# Tasks

- [x] 1. sena-dec library: EBML demuxer + rxaac/ropus core adapters +
      leading trim + upsample + gain restore + mix + playable-length
      truncate + per-block payload-bit accounting + in-place Matroska
      tag writer
- [x] 2. CLI bin senadec: --format wav/raw, stdout pipe, --dither none,
      --dump-tracks, --info
- [x] 3. Conformance lock vectors vs reference cores (assets/e2e + small-AU,
      warmup-prone in-tree test)
- [x] 4. Plugin host shims over the C ABI (foobar2000 Windows
      x64 + arm64ec and macOS x86_64 + arm64 P0, rest per priority);
      follow notes/foobar2000-plugin.md; include input_info_writer tag
      editing and get_dynamic_info real-time bitrate
- [x] 5. Playable-length truncation + gapless sequence golden
- [x] 6. Output-format conformance: WAV headers, s24 packing, s16 TPDF
      reproducibility, raw pipe consumer checks
- [x] 7. Reference-asset delta analysis: every non-bit-exact delta vs
      assets/e2e references attributed to acceptable causes
