# Sena decoder for FFmpeg (and LAV Filters)

Adds a `sena` audio codec to FFmpeg. A Sena track is a single composite
elementary stream in Matroska (`A_MS/ACM` with `wFormatTag = 0x534E`; the
WAVEFORMATEX extra bytes are the SENA-INIT blob), so the **stock** Matroska
demuxer owns the container — including video MKV files carrying a Sena
audio track — and this decoder produces the mixed, trimmed 48 kHz stereo
float PCM from it. The Rust `sena-dec` core is loaded at runtime
(`dlopen`/`LoadLibrary`): the FFmpeg build needs no Rust toolchain, and a
core update is a drop-in library replacement.

Layout:

```
libavcodec/senadec.c       the decoder (ES push API front-end)
libavcodec/sena_dec_dl.c   runtime loader for the sena-dec core
libavcodec/sena_dec_dl.h
libavcodec/sena_dec.h      the sena-dec C ABI (copy of the canonical header)
tools/apply.py             idempotent installer into an FFmpeg source tree
tools/rebuild.sh           apply + re-run configure + make
```

## What the patch changes in FFmpeg

`apply.py` makes six edits, all guarded/idempotent (`--check` verifies,
`--uninstall` reverts):

1. copies the files above into `libavcodec/`;
2. adds `AV_CODEC_ID_SENA` to `libavcodec/codec_id.h`;
3. adds the codec descriptor to `libavcodec/codec_desc.c`;
4. registers `ff_sena_decoder` in `libavcodec/allcodecs.c` (sorted) —
   configure picks it up automatically; re-run `./configure` afterwards;
5. adds the objects to `libavcodec/Makefile` (`CONFIG_SENA_DECODER`);
6. maps wFormatTag `0x534E` to `AV_CODEC_ID_SENA` in
   `libavformat/riff.c` (`ff_codec_wav_tags`), which makes the Matroska
   `A_MS/ACM` path and WAV resolve the codec with no demuxer patches.

No probe/deferral hacks: a video MKV with a Sena track keeps its video;
an audio-only `.sena` file is just a Matroska file with one Sena track.

## Build

```
python3 tools/apply.py /path/to/ffmpeg
cd /path/to/ffmpeg && ./configure <previous args> && make ffmpeg ffprobe
```

The decoder needs the Rust core at runtime:

```
cargo build --release -p sena-dec-capi     # -> target/release/libsena_dec.so
export SENA_DEC_LIBRARY=$PWD/target/release/libsena_dec.so
```

Library search order: `$SENA_DEC_LIBRARY` → directory of the module that
loads it (the `avcodec-*.dll` directory — also the LAV Filters layout) →
platform default (`libsena_dec.so` / `sena_dec.dll` / `libsena_dec.dylib`).

## DirectShow notes

With a patched LAV, `LAVSplitter` exposes the Sena track as an audio pin
with subtype `{0000534E-0000-0010-8000-00AA00389B71}`
(`FOURCCMap(0x534E)`) and a WAVEFORMATEX format block. `LAVAudio` does not
claim that subtype; a Sena DirectShow transform filter (planned, wrapping
the same ES core) connects behind it with zero player/LAV changes. Seeks
in the filter use the ES core's shadow-file warmup (full container-seek
quality); the ffmpeg plugin here shows the cold-start ramp fallback
(LF silent until the first independency AU, then converges).
