# Sena demuxer for FFmpeg (and LAV Filters)

Adds a `sena` input format to FFmpeg: `.sena`/`.mka` files containing the
Sena dual-track layout (xHE-AAC LF + Opus HF in Matroska) are decoded to the
mixed, trimmed 48 kHz stereo PCM stream by the Rust `sena-dec` core, which is
loaded at runtime (`dlopen`/`LoadLibrary`) — the FFmpeg build itself needs no
Rust toolchain, and a core update is a drop-in library replacement.

Layout:

```
libavformat/senadec.c      the demuxer (libopenmpt-style: decoded PCM out)
libavformat/sena_dec_dl.c  runtime loader for the sena-dec core
libavformat/sena_dec_dl.h
libavformat/sena_dec.h     the sena-dec C ABI (copy of the canonical header)
libavformat/sena_probe.h   shared probe helper (also used by matroskadec)
tools/apply.py             idempotent installer into an FFmpeg source tree
tools/rebuild.sh           apply + re-run configure + make
```

## What the patch changes in FFmpeg

`apply.py` makes three edits, all guarded/idempotent (`--check` verifies,
`--uninstall` reverts):

1. copies the files above into `libavformat/`;
2. registers `ff_sena_demuxer` (`allformats.c` + `Makefile`); configure picks
   it up automatically — re-run `./configure` afterwards;
3. patches `matroska_probe()` to return 0 for Sena files (a `.sena`
   extension, or a visible `SENA_PROFILE` tag in the probed head), so the
   sena demuxer always wins its own files. Plain Matroska/WebM files are
   unaffected (tests/ffmpeg/run.sh checks both directions).

The demuxer exposes one audio stream (`pcm_f32le`, 48 kHz, stereo) with exact
duration and the span-based average bitrate, plus container tags
(`SENA_PROFILE`, user metadata) and Matroska cover art as attached_pic
streams. Seeking is frame-exact (`sena_dec_seek`).

## Build (Linux/macOS)

```bash
cargo build --release -p sena-dec-capi      # target/release/libsena_dec.so
plugins/ffmpeg/tools/rebuild.sh ~/src/public/ffmpeg   # apply + configure + make
```

The patched `ffmpeg`/`ffprobe` then decode any `.sena` file when the core
library is findable. Core library search order:

1. `$SENA_DEC_LIBRARY` (full path override — used by the test suite),
2. next to the module containing the demuxer (the libavformat library /
   executable directory),
3. the platform default search (`LD_LIBRARY_PATH`, ldconfig, ...).

To install: `sudo make install` in the FFmpeg tree, then copy
`target/release/libsena_dec.so` to `/usr/local/lib` (+ `ldconfig`) or next to
the ffmpeg binary.

Note: on glibc < 2.34 `dlopen` needs `-ldl`; configure with
`--extra-ldflags=-ldl` there (glibc >= 2.34 has it in libc; Windows/macOS
need nothing).

## LAV Filters (MPC-HC / PotPlayer)

LAV Filters embeds a patched FFmpeg and loads its demuxers dynamically
(`av_demuxer_iterate` in `demuxer/Demuxers/LAVFDemuxer.cpp`), so the same
patch applies to the FFmpeg fork inside LAVFilters:

1. `plugins/ffmpeg/tools/apply.py <LAVFilters>/ffmpeg` (its ffmpeg tree), then
   rebuild LAV's FFmpeg and the LAV Filters as usual (Windows/MSVC).
2. Build the core DLL: `just senadec-plugin-lav-dll windows-x64`
   (cross-compiles `sena_dec.dll` from `crates/sena-dec-capi` with
   cargo-xwin) and place it next to LAV's `avformat-*.dll` — the loader looks
   in the module's directory.
3. The `sena` demuxer appears automatically in LAV Splitter's formats list.
   For file association, register `.sena` like LAV's installer does for
   other formats (`LAVFilters.iss` `InitFormats()`):
   `FR(SplitterFormats[N], 'sena', 'Sena Audio', True, ['sena', 'mka', ''])`
   — or associate `.sena` with the LAV Source filter CLSID via the player's
   format options / registry `HKCR\Media Type\Extensions\.sena`.

LAV Audio receives float PCM from the splitter pin (`AV_CODEC_ID_PCM_F32LE`),
which MPC-HC and PotPlayer render normally.

The full design notes, including the DirectShow details and the seek-warmup
analysis, are in `notes/ffmpeg-lav-plugin.md`.

## Test

```bash
just senadec-plugin-ffmpeg-test        # or: bash tests/ffmpeg/run.sh [ffmpeg-src]
```

Deterministic gates (`tests/ffmpeg/verify.py`):

- decode: ffmpeg output **bit-exact** with the Rust streaming reference
  (`decode_dump`), and within f32-ulp (1e-9) of the whole-file `senadec` CLI;
  exact playable length;
- seek: bit-exact vs the Rust seek path, xcorr lag 0, codec-warmup envelope;
- probe: `.sena`/`.mka`/extensionless all resolve to `sena`, while a plain
  Opus-in-`.mka` still resolves to `matroska`;
- tags + cover art visible through ffprobe/ffmpeg;
- without the core library: clean, actionable error (no crash).
