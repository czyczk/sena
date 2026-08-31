# Tasks

- [ ] 1. Implement mka writer (two tracks, cluster interleaving <= 1s)
- [ ] 2. Implement elementary-stream tag stripping (OpusTags page, m4a metadata boxes)
- [ ] 3. Write CodecDelay/SeekPreRoll metadata per track
- [ ] 4. Container identification tag SENA_PROFILE and SENA_VERSION
- [ ] 5. Editable-tags layout: user tags at segment tail; void+append
      tag-update path (see notes/foobar2000-plugin.md)
- [ ] 6. Conformance tests: parse by senadec, seek, streaming start,
      tag rewrite without cluster movement
