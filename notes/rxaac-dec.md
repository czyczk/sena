# External dependency notes: rxaac-dec (xHE-AAC decoder)

Path: `../rxaac-dec`; consumed crate: `rxaac-dec-lib` (common: `rxaac-common`).

## Integration contract
- `UsacDecoder` is an **untrimmed PCM core**: `decode_au(au, &mut pcm)` appends
  decoded interleaved f32. senadec owns the leading 1024-core-sample trim and
  the final `SENA_PLAYABLE_SAMPLES` truncation.
- senadec validates `output_rate()`, `output_samples()`, and `num_channels()`
  against the container track; Sena LF streams observed as NoSbr, 1024 core
  frame, stereo, 16 kHz or 32 kHz.
- Verified on `assets/e2e`: leading lag is exactly -3072 (p300, 16 kHz) /
  -1536 (p600, 32 kHz) at 48 kHz, 0 frame errors. Output contains trailing
  padding beyond the playable length, which is expected to be handled by
  senadec truncation.
- Per-AU decode error policy is decided by senadec (currently: silence for
  the frame + warning, continue).

## Known issue (2026-08-31): one-byte-shifted AUs panic `spectrum.rs`

Root cause was on the sena-enc side, not in rxaac-dec: exhale 1.2.2 can
write a **3-byte** `mdat` preamble (`7a 32 00`) while the old encoder code
assumed 2 bytes. Every extracted AU therefore started one byte late.

Reproducing asset:
- Source: `assets/e2e/01__src.flac`, first 10 s (480000 frames @48k).
- Encode command that produced the bad stream (before the `stco` fix):
  `senaenc --profile 300 --opus-original 160 seg1.wav seg1.sena`
- Resulting LF track in `seg1.sena`: ASC
  `f95048221cc0585200200099004688b800`, 157 AUs, stream rate 16000 Hz.
- Expected first AU begins `d9de08221cc0585200200099004688b8...`;
  bad extraction began `00d9de08221cc05852...` (one extra leading 0x00).
- Correct first AU offset is `stco[0]` (1553); old code used `mdat + 2`
  (1552).

Observed rxaac-dec behavior on the bad AU stream:
```
warning: LF AU decode error (0): spectrum error
warning: LF AU decode error (0): spectrum error
thread 'main' panicked at
  ../rxaac-dec/crates/rxaac-dec-lib/src/spectrum.rs:179:13:
  index out of bounds: the len is 128 but the index is 128
```

Stack (sena-dec calling rxaac-dec-lib):
```
rxaac_dec_lib::spectrum::section_data       spectrum.rs:179
rxaac_dec_lib::spectrum::fd_channel_stream  spectrum.rs:374
<UsacDecoder>::core_coder_data              usac.rs:742
<UsacDecoder>::decode_frame_reader          usac.rs:416
<UsacDecoder>::decode_frame                 usac.rs:390
<UsacDecoder>::decode_au                    usac.rs:1127
```

rxaac-dec CLI (`rxaac-dec lf.m4a -o lf.wav`) on the same shifted AU stream
also reported `AU 2: spectrum error (au_size=203)` and
`AU 4: spectrum error (au_size=199)`, then panicked at the same
`spectrum.rs:179`.

Containment on the Sena side:
- sena-enc `find_aus` now trusts the MP4 `stco` chunk table instead of a
  hard-coded preamble length.
- sena-dec wraps `UsacDecoder::decode_au` in `catch_unwind`, truncates any
  partial frame appended before the panic, inserts silence for that AU, and
  continues.

For rxaac-dec upstream, the request is: don't panic on malformed/bit-shifted
USAC AU input; return `Err` from `section_data` / `decode_frame` instead.


## API observations
- No `reset()` method exposed; for seek / rewind, recreate `UsacDecoder`
  from the ASC.
- No simd feature yet. When rxaac-dec adds one, wire it into sena-dec's
  `simd` feature.
- The CLI `rxaac-dec` currently cannot parse the minimal reference M4A built
  by `assets/tools/decode_ref.py` (`truncated box: esds payload`, descriptor
  length form difference). This does not block sena-dec because senadec feeds
  raw AUs to the lib, not M4A files.

## Reproducibility
- The directory has no `.git` metadata, so it cannot be rev-pinned. Deliberately
  NOT pinned for now (pre-release, expected to keep updating).

## Do not modify
- All issues are recorded here or raised upstream; never edit `../rxaac-dec`
  from the sena side.
