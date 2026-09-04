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
- Per-AU decode error policy is decided by senadec: silence for the frame +
  warning, then recreate `UsacDecoder` from the ASC (there is no reset) and
  continue with the next AU. A panic is no longer treated as a recoverable
  per-AU error: it is caught only as a last resort and fails the decode with
  a clean `DecodeError`.

## Known issue (2026-08-31, FIXED 2026-09-03): one-byte-shifted AUs panic `spectrum.rs`

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
- sena-dec handles `decode_au`'s `Err` path itself: truncate any partial
  preroll append, insert a silent frame using the cached valid frame
  geometry, recreate the decoder from the ASC, and continue. A `catch_unwind`
  remains only as a thin fail-fast defense (see the fuzz methodology below).

For rxaac-dec upstream, the request was: don't panic on malformed/bit-shifted
USAC AU input; return `Err` from `section_data` / `decode_frame` instead.

### Was the upstream fix optimal?
Yes, for this failure mode. A valid short-window `group_dis` always ends
with the cumulative terminator 8 (the C code loops `for (group = 0;
group < window_grps;) { group = *groups++; ... }`). Entries after the
terminator are stale state; no scalefactor bits are coded for them, so there
are no "extra factors" to decode or salvage - reading them would only consume
bits that belong to the spectral data and corrupt the bitstream position.
The only safe action for a malformed grouping is to reject the AU with an
error (which the fixed code does via `TooManyScalefactors`); sena then treats
that frame as silence. For valid streams the fix is bit-exact with C.


## API observations
- No `reset()` method exposed; for seek / rewind, recreate `UsacDecoder`
  from the ASC.
- `rxaac-dec-lib` now has a `simd` feature (hand-written NEON kernels,
  aarch64 only). Sena wires it: `sena-dec/simd` now includes
  `rxaac-dec-lib/simd` in addition to `opus-decoder/simd`.
- The CLI `rxaac-dec` currently cannot parse the minimal reference M4A built
  by `assets/tools/decode_ref.py` (`truncated box: esds payload`, descriptor
  length form difference). This does not block sena-dec because senadec feeds
  raw AUs to the lib, not M4A files.

## Reproducibility
- `../rxaac-dec` now has real git history. Verified rxaac revision:
  `0270873ffb08deaaecd26f14dcafd24c4b215da5`
  (`Fix malformed-AU panics: validate channel layout, guard OOB paths`).
- The Cargo path dependency is still deliberately NOT pinned; when the
  directory moves, record the verified revision here.

## Acceptance (2026-09-03)
- Upstream has fixed the `spectrum.rs` panic: `section_data` now bounds-checks
  the factor buffer and returns `SpectrumError::TooManyScalefactors`; the
  error propagates through `fd_channel_stream` / `decode_frame` / `decode_au`.
- Verified from the Sena side without touching `../rxaac-dec`:
  - `cargo test --workspace` in rxaac-dec (target dir redirected to /tmp):
    all pass, including their regression test
    `usac::tests::malformed_au_never_panics` (leading 0x00/0xff, truncation,
    zeros, garbage, bit-flip variants, fresh and stateful decoders).
  - Added `crates/sena-dec/tests/rxaac_upstream.rs`: rebuilds the exact
    one-byte-shifted LF AU stream from `assets/e2e/01__p300__lfa__hf144.sena`
    and calls rxaac-dec-lib directly for the first 16 shifted AUs, both fresh
    and after valid AUs; no panic.
  - Also ran the full rxaac-dec workspace with `--features simd` on this
    aarch64 host (so the NEON kernels were exercised): all tests pass.
- This case is closed. Sena now handles the `Err` path directly; the
  remaining `catch_unwind` is only a thin fail-fast safety net (see the
  2026-09-04 fixed issue and fuzz methodology below).

## Fixed (2026-09-04): malformed AudioPreroll config panic + three related OOB classes
The issue reported on 2026-09-03 is fixed upstream in commit
`0270873ffb08deaaecd26f14dcafd24c4b215da5`:

- `UsacDecoder::new` now rejects a config whose element channel layout
  exceeds `num_out_channels` (covers the AudioPreroll re-init path).
- `core_coder_data` bounds-checks `chan_offset + nr_ch` against the channel
  vector.
- The same commit also guards three other fuzz-found OOB classes: FAC
  transition geometry, FAC windowing on non-transition frames, and SBR
  noise-floor index clamping.

Independent re-verification from the Sena side:
- The 2026-09-03 minimal repro (first LF AU of
  `assets/e2e/01__p300__lfa__hf144.sena`, byte 3 `0x22` -> `0x21`) now
  returns `Err(invalid frame: element channel layout exceeds
  num_out_channels)`; no panic.
- Re-ran the deterministic Sena-corpus hunt: **133,904 decode_au calls,
  0 panics, 0 unique panic signatures**.
- rxaac's own fast regression tests pass, and its ignored thorough fuzz
  `malformed_aus_fuzz_never_panics` passes.
- Added a permanent Sena-side ignored fuzz harness
  `crates/sena-dec/tests/rxaac_fuzz.rs` (39,910 deterministic decode calls
  against real Sena LF AUs; debug run ~112 s, 0 panics).

Sena-side status after the fix:
- Normal `Err` is handled per-AU (silence + decoder rebuild + continue).
- `catch_unwind` is now only a thin last resort: a future panic becomes
  `DecodeError::Codec("rxaac decoder panicked ...")` instead of aborting the
  host. It no longer tries to continue from a panicking decoder.

## Fuzz methodology (malformed AU / no-panic contract)
Keep this recipe for future rxaac updates:

1. **Corpus**: use the first 8-12 LF AUs of a real Sena asset
   (`assets/e2e/01__p300__lfa__hf144.sena`), with the valid ASC parsed from
   the same file. This covers the real AudioPreroll + CPE path.
2. **Mutation classes** (deterministic only, no entropy):
   - every-byte substitution with a fixed value set;
   - single-bit flips over the parser-head bytes;
   - truncations, leading `0x00/0x01/0xff`, and one-byte boundary-shifted
     AU streams (the historical exhale failure mode);
   - fixed-seed LCG random multi-byte mutations concentrated in the first
     80 bytes, mixing xor / assign / +/-1.
3. **Decoder states**: decode each variant with a fresh decoder; run a
   subset also after decoding 3 valid AUs (stateful), because preroll and
   overlap state matter.
4. **Harness**: `catch_unwind` is allowed in the *test harness only*, to
   collect panic locations instead of stopping at the first one. Group by
   `file:line + panic message`; report unique signatures, not raw counts.
5. **Minimise**: when a signature is found, delta-debug the mutation set.
   The 2026-09-03 channel-layout panic reduced from a 6-byte random variant
   to a single byte change (`0x22` -> `0x21` at offset 3).
6. **Run**:
   - upstream: `cargo test -p rxaac-dec-lib malformed_aus_fuzz_never_panics -- --ignored`
     (from the rxaac workspace; use a writable `--target-dir` if needed);
   - Sena side:
     `cargo test -p sena-dec --test rxaac_fuzz -- --ignored --nocapture`.
7. **Contract**: every malformed AU must return `Err`; a panic is an
   upstream bug to record in this file, with minimal repro, and sena's thin
   fail-fast catch is the safety net.

## Do not modify
- All issues are recorded here or raised upstream; never edit `../rxaac-dec`
  from the sena side.
