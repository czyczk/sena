# ffmpeg / LAV Filters plugin notes (libsena_dec via runtime loading)

Verdict first: ffmpeg support is a single **libavformat demuxer** named
`sena` that drives the shared `sena-dec` C ABI and emits the decoded 48 kHz
stereo float PCM stream directly (the libopenmpt/libgme pattern). LAV Filters
inherits it by rebuilding its embedded FFmpeg fork with the same patch. The
core library (`libsena_dec.so` / `sena_dec.dll` / `libsena_dec.dylib`) is
loaded at **runtime**, so the FFmpeg build needs no Rust toolchain and core
updates are drop-in replacements.

This mirrors the foobar2000 design rule: all trim/mix/resample logic stays in
the Rust core; the shim is a thin adapter.

---

## 1. Why a demuxer, not a libavcodec decoder

A Sena file's playable stream is the *sum of two codec tracks* after
resampling, gain restore, and leading/trailing trims. ffmpeg's decoder
abstraction is strictly one-stream-in/one-stream-out; there is no place for
the two-track mix except a filter, and a filter cannot own the container.
The established ffmpeg pattern for "container that decodes to one PCM stream"
is a demuxer that outputs PCM packets: `libavformat/libopenmpt.c`,
`libgme.c`, `libmodplug.c`, `avisynth.c`. `senadec.c` follows it:

- stream codecpar: `AV_CODEC_ID_PCM_F32LE`, 48 kHz, stereo (pass-through
  `pcm_f32le` decoder downstream);
- `read_packet` = `sena_dec_read_f32` (1024-frame packets);
- `read_seek` = `sena_dec_seek` (frame-exact, stream timebase 1/48000);
- `s->duration`/`st->duration` from `SENA_PLAYABLE_SAMPLES`; stream bitrate
  from `audio_span_bytes` (not file size — same rule as the fb2k plugin);
- metadata: immutable `SENA_*` tags + user tags (`sena_file_read_tags`);
- Matroska Attachments as attached_pic streams (`sena_file_art_read`).

## 2. Probe: how `.sena` wins over the matroska demuxer

A Sena file is valid EBML/Matroska, so `matroska_probe` returns
`AVPROBE_SCORE_MAX` for it on the first 2048-byte window — and on a tie the
earlier-registered demuxer (matroska) would win, even for `.sena`. The patch
therefore makes matroska **defer** (`return 0`) when the file is Sena:

- by extension: `av_match_ext(p->filename, "sena")`;
- by content: the mandatory `SENA_PROFILE` tag is visible in the probe buffer
  (the container spec requires the identification `Tags` element before the
  first Cluster; measured offset ~217 bytes in all e2e assets, well inside
  `PROBE_BUF_MIN` = 2048).

The shared helper `ff_sena_probe_match()` (EBML magic + tag scan) is used by
both the sena demuxer's own probe (returns `AVPROBE_SCORE_MAX`) and the
matroskadec patch (guarded by `#if CONFIG_SENA_DEMUXER`, so stock builds are
unaffected). ffmpeg's probe loop only bumps a *probed* format to score 1 on
extension match (format.c `av_probe_input_format3`), so deferral is clean.

Consequences verified by `tests/ffmpeg/run.sh`:

- `.sena`, Sena-renamed-`.mka`, and Sena-renamed-`.bin` all open as `sena`;
- a plain Opus-in-`.mka` still opens as `matroska,webm` (negative control).

## 3. Runtime loading (`sena_dec_dl.c`)

- One-time, thread-safe load (`AVOnce`), symbol table `SenaDecAPI`; a missing
  symbol cleanly disables the demuxer (error at open, never a crash).
- Search order: `$SENA_DEC_LIBRARY` (full path; the test-suite override) →
  directory of the module containing the demuxer (dladdr /
  GetModuleHandleEx; this is what makes LAV's "DLLs in one folder" layout
  work) → platform default search.
- The library is never unloaded: decoder handles may outlive the demuxer
  instance, and unloading a Rust cdylib with live handles is unsound.
- glibc < 2.34 needs `-ldl` at configure time (`--extra-ldflags=-ldl`);
  newer glibc, Windows and macOS need nothing.

## 4. Determinism findings while validating (important)

The task's deterministic answers (the e2e assets + archived reference
decodes) surfaced two decoder-core behaviors; both are now pinned by tests.

### 4.1 Streaming path seam fix (fixed, was a real defect)

`StreamingDecoder::produce_chunk` used to resample each ~100 ms chunk with a
*fresh* zero-phase `Resampler` over just the pending window — the window's
head/tail lacked the FIR guard context (`guard_in` = 602 core samples), so
every chunk boundary rang: periodic seams up to 0.12 peak, 12.5 dB worse RMS
than the whole-file path (-65.6 dB vs -78.0 dB against ref32). The fix keeps
`lf_guard` core samples of consumed raw as left context and only emits
outputs with complete right context (except at EOF, where the zero-padded
tail matches the whole-file decode by construction). Seek windows keep the
pre-target raw as context (as before), plus a zero prefix replacing the
warmup AU when the seek anchors at AU 0 — the whole-file decode treats
everything before the 1024-sample trim as nonexistent.

Result: **streaming output is bit-exact with the whole-file pipeline**
(`stream::tests::streaming_matches_whole_file_pipeline`, all e2e assets;
worst-case diff 3.6e-12 = f32 ulp from FFT block-size differences). This also
fixes the foobar2000 plugin's playback path (it drives the same ABI).

### 4.2 Seek warmup is format-inherent (confirmed format behavior, not a bug)

After `sena_dec_seek`, the LF xHE-AAC decode continues from the nearest
independency-flag AU with a fresh rxaac decoder. Measured with
`examples/lf_preroll_probe.rs` (fresh decode from AU K vs continuous):

- the jump is *not* sample-exact: an LPD/FAC warmup of ~100-200 ms (up to
  ~0.2-0.5 max sample diff on loud LPD content) plus a persistent SBR
  noise-phase difference (~1e-4..1.7e-3);
- decoding extra non-independent AUs before the target ("preroll") does **not**
  reliably converge it (flat until a re-sync frame is crossed; then 1e-5);
- Opus seeks are exact.

**Resolution after cross-checking with rxaac-dec (accepted, 2026-09-21):**
this is inherent to the xHE-AAC format and to the AOSP C reference, not a
decoder defect. rxaac's three-way comparison (rxaac seek vs C-reference seek
vs C continuous decode) shows the C reference has the *same* permanent
residual (-25..-41 dB), and rxaac is faithful to it (-73..-80 dB). The
residual is fully decomposed: the FD core is bit-exact from the frame after
the independency frame; eSBR noise/sine phase counters are free-running (not
carried in the bitstream) -> permanent envelope-identical noise difference;
missing LPD previous-frame state -> the loud-content transient; pure ACELP
indep frames don't reset TCX arithmetic context. Even true AudioPreroll seeks
are only "clean restarts" per spec (-23..-53 dB vs continuous), not
bit-exact. A fresh decoder is already the all-zero state, so "reset on
indep" is meaningless; resetting during continuous decode would instead
deviate from the C reference every ~2 s.

For Sena streams specifically: scanning all e2e assets
(`examples/lf_au_scan.rs`) shows AudioPreroll payloads only in AU 0 (which is
exactly the trimmed warmup) — **exhale emits no mid-stream preroll AUs**, so
the indep-AU jump is already the best available random access, and AU 1 is
also flagged independent (preroll pattern at stream start).

Consequences: the ffmpeg seek test asserts bit-exactness against the **Rust
seek path** (the deterministic answer for a seek) plus lag 0 and a
steady-state envelope against the continuous decode — never equality between
the two decode paths. Player-visible behavior matches any standards-based
xHE-AAC random access.

## 5. LAV Filters integration

LAV Filters (`Nevcairiel/LAVFilters`) embeds a patched FFmpeg as shared
`avcodec/avformat-*.dll` next to `LAVSplitter.ax`/`LAVAudio.ax`.

- **Demuxer availability**: `demuxer/Demuxers/LAVFDemuxer.cpp` enumerates
  formats via `av_demuxer_iterate`, so the `sena` demuxer appears in LAV
  Splitter's formats list automatically once the patch is applied to LAV's
  ffmpeg tree (`plugins/ffmpeg/tools/apply.py <LAVFilters>/ffmpeg`).
- **Core DLL**: `just senadec-plugin-lav-dll` cross-builds `sena_dec.dll`
  (cargo-xwin, x86_64/i686-pc-windows-msvc) into `build/plugins/ffmpeg/`;
  place it next to `avformat-*.dll` (module-relative load) or on PATH.
- **File association**: DirectShow resolves `.sena` to LAV Splitter via
  registry `HKCR\Media Type\Extensions\.sena` (Source Filter = LAV CLSID) —
  the LAV installer writes these from the `InitFormats()` table in
  `LAVFilters.iss`; add `FR(SplitterFormats[N], 'sena', 'Sena Audio', True,
  ['sena','mka',''])` there, or associate in MPC-HC/PotPlayer format options.
  Content-based checkbytes (`0,4,,1A45DFA3`) are shared with Matroska on
  purpose: LAV's ffmpeg probe defers to the sena demuxer internally.
- **Decode path**: LAV Splitter pin exposes float PCM
  (`MEDIASUBTYPE_IEEE_FLOAT`); LAV Audio passes PCM through. MPC-HC and
  PotPlayer render it directly.

Not verifiable on Linux (DirectShow is Windows-only): the .iss edit, the
registry association, and end-to-end playback. Everything up to the
DirectShow boundary is validated here via the Linux ffmpeg build.

## 6. Windows/macOS notes for the core library

- Windows x64/x86: `cargo xwin build -p sena-dec-capi --target {x86_64,i686}-pc-windows-msvc --release --lib`
  produces `sena_dec.dll` (+ import lib); all 22 ABI symbols verified in the
  export table.
- The fb2k component links the *static* lib; the ffmpeg demuxer wants the
  *shared* lib. The shared packaging lives in a separate crate,
  `crates/sena-dec-capi` (`crate-type = ["cdylib"]`, lib name `sena_dec`, a
  facade re-exporting the `sena-dec` ABI), so `sena-dec` itself keeps
  `["rlib", "staticlib"]` and the fb2k macOS/Windows builds are unaffected.
- ARM64 Windows (LAV ARM64 exists): `aarch64-pc-windows-msvc` cdylib should
  work the same way via cargo-xwin; not yet wired into the just recipe.

## 7. Follow-ups (not in scope)

- Nothing on the seek side: cross-check with rxaac confirmed the residual is
  format-inherent and faithful to the AOSP C reference (see 4.2).
- ffmpeg muxer/encoder for `.sena` (would wrap sena-enc's exhale/opusenc
  drivers; out of scope for playback support).
- Album-art on non-seekable inputs (currently skipped; the audio path works).
- Upstreaming: the demuxer + loader are written to ffmpeg style; the
  matroskadec deferral is the only upstream-touching hunk.
