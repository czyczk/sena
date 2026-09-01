# Seeking, tags and attached pictures (foobar2000 integration notes)

Measured and fixed 2026-09-01.

## Seek performance (was several seconds, now ~50 ms)

Root cause: the old linear seek decoded every AU/packet between the start
and the target with no random access. xHE-AAC (exhale) streams are not
frame-independent in general, but exhale marks USAC random-access points
every ~15 AUs via the `usacIndependencyFlag` (bit 7 of the first AU
payload byte). A fresh decoder started exactly at such an AU is bit-exact
against sequential decoding for the tested p300 asset; for some content
(e.g. the p600 asset with its SBR/stateful tools) the fresh-start output
converges to the sequential output instead of being bit-exact (opus
shows the same class of convergence: ~0.2 peak over the first ~7
packets, decaying to <0.01). This is the standard behavior of random
access into stateful codecs (every player does the same).

Implementation (`crates/sena-dec/src/stream.rs`):
- `seek()` jumps: LF = nearest independency AU at/just before the target
  (plus one extra AU of context) with a fresh `UsacDecoder`; Opus = first
  packet covering the target with a fresh decoder.
- On seeks the pre-target LF raw is kept as zero-phase resampler context
  (trim after resampling instead of before) - no filter transient at the
  seek point.
- Offsets/trims are tracked per (re)start; the bit-accounting prefix is
  seek-relative; the rational-ratio take is rounded to a representable
  output count (up to 2 frames deferred to the next chunk).
- Measurements on the 20 s assets: seek + first second of playback is
  ~40-70 ms (was ~0.6 s for 80% and grew linearly with position; a
  4-minute track would have taken ~7-9 s).

## Attached pictures (meta "PICTURE") and the converter error

foobar's converter transfers metadata after encoding; attached pictures
arrive as `file_info` meta entries with key `PICTURE` carrying a binary
payload. Matroska Tags are string-only (UTF-8); embedding binary data
there is wrong and the previous code passed it through (CStr truncation
at embedded NULs) - the converter then reported
"error transferring attached pictures".

Fix: pictures are skipped at both layers
- `plugins/foobar2000/foo_input_sena/input_sena.cpp` (retag: skip key
  "PICTURE", log a console note);
- `crates/sena-dec/src/tags.rs` `sanitize_entries` (skip PICTURE keys and
  values with NUL bytes or invalid UTF-8) - a guard for any caller.
The write then succeeds without pictures; proper support would need
Matroska Attachments (out of scope; note kept in the console log).

## ReplayGain write / read-back

The Rust-level flow is verified sound: writing REPLAYGAIN_* entries,
re-parsing, re-probing, decoding and the header-only tag scan all work
and the immutable Sena tags survive (tests in `tags.rs`). If a
foobar2000 write previously corrupted the picture/tag area, the same
binary-payload path (now filtered) was the suspect; please re-test RG
scan/save with a rebuilt plugin. If it still fails, capture the exact
error text and the file size before/after the write (a .sena should only
grow by the appended Tags element).
