# Change: xfi profile family (Fraunhofer-IIS xHE-AAC mid) + ES v3

## Why

The Opus mid layer is the weakest rail in the three-track layout for
attention-stability metrics (stereo IC error and envelope sigma in
3-12 kHz). A Fraunhofer-IIS xHE-AAC CBR 128k @ 44.1 kHz mid measures better
on those axes at a *lower* nominal rate (128k vs 160k), which lets the
three-track layout start at 256k total instead of 288k. ezcd-cli makes the
FhG IIS encoder scriptable; the decoder side is unaffected (any xHE-AAC
mid decodes through the same USAC path).

## What Changes

- New profile names `<lf split><mid codec>`: `300o`/`600o` (Opus mid,
  unchanged behavior; bare `300`/`600` still accepted) and
  `300xfi`/`600xfi` (Fraunhofer-IIS xHE-AAC mid via ezcd-cli). The xfi
  family is always three-track and accepts nominal totals 256-288k
  (continuous): LF exhale unchanged (deduct 24k/32k), mid fixed CBR 128k,
  top band gets the rest (96k at 256k, +1k per +1k, capped 128k at 288k).
- ES v3: SENA-INIT gains a mid-codec byte (v2 files read as Opus mid and
  stay decodable); composite blocks carry the xHE mid as a counted AU list
  (count + absolute au_index + per-AU lengths, same shape as the LF
  section) because 23.22 ms AUs do not align to the 20 ms block grid. Mid
  AUs are placed by playable end (same rule as LF AUs).
- The mid xHE stream runs at 44100 Hz; decode trims one 1024-sample warmup
  frame and resamples 44.1k -> 48k. The AU model matches the LF rail (AU0
  = 2048 = preroll + main; steady 1024/AU; independency flag in bit 7).
- senaenc requires `ezcd-cli`/`ezcd-cli-portable` (next to the exe or on
  PATH) for xfi profiles; WSL runs the Windows binary through interop with
  `wslpath` path translation. `senaenc doctor` reports it.
- foobar2000 `codec_profile` displays `xAAC-xAAC@<lf>+HF@12000` for xfi
  files (the mid encoder identity is not exposed on decode side).

## Out of scope

- No quality judgment changes to the Opus-mid family (unchanged).
- No backward compatibility for v2 *writing*: new encodes are always v3,
  old v2 assets remain decodable.

## Verification

- `cargo test --workspace` (incl. `xhe_mid_seek`, and
  `streaming_matches_whole_file_pipeline` on an xfi e2e asset).
- `tests/golden/run.sh <wav> 600xfi 256` — container bit-equality vs the
  workdir elementary streams + reference-decoder assembly alignment.
