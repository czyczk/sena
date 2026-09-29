#!/usr/bin/env bash
# End-to-end validation of the ffmpeg Sena decoder (libavcodec plugin over
# the dlopen'd Rust core) against the deterministic reference decodes (the
# Rust streaming/container decoder and whole-file CLI paths).
#
# usage: tests/ffmpeg/run.sh [ffmpeg-src-tree]
#
# Requires: ffmpeg/ffprobe built with the Sena patch (plugins/ffmpeg/tools/
# apply.py), python3 + numpy, and the e2e assets. Builds the needed Rust
# release artifacts itself.
set -euo pipefail

ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
FF_SRC="$(cd "${1:-$HOME/src/public/ffmpeg}" && pwd)"
FFMPEG="$FF_SRC/ffmpeg"
FFPROBE="$FF_SRC/ffprobe"
WORK="$(mktemp -d /tmp/sena-ffmpeg-test.XXXXXX)"
trap 'rm -rf "$WORK"' EXIT

cd "$ROOT"
# Build the runtime-loaded core separately: a combined invocation with
# `--example` filters every package's targets down to examples, silently
# skipping the cdylib and leaving a stale libsena_dec.so in place.
cargo build --release -p sena-dec-capi -p sena-dec -p senadec 2>&1 | tail -1
cargo build --release -p sena-dec --example decode_dump --example make_fixture 2>&1 | tail -1
export SENA_DEC_LIBRARY="$ROOT/target/release/libsena_dec.so"

[ -x "$FFMPEG" ] || { echo "no ffmpeg binary at $FFMPEG" >&2; exit 2; }
"$FFMPEG" -hide_banner -decoders 2>/dev/null | grep -Eq '^ *A\.* +sena ' \
    || { echo "ffmpeg at $FF_SRC has no sena decoder (run apply.py + reconfigure)" >&2; exit 2; }
echo "ffmpeg: $("$FFMPEG" -version | head -1)"
CORE_VERSION="$(strings "$SENA_DEC_LIBRARY" | grep -m1 'sena-dec' || true)"
echo "core:   ${CORE_VERSION:-unknown}"

PLAYABLE=960000
for f in assets/e2e/*.sena; do
    b="$(basename "$f" .sena)"
    "$ROOT/target/release/examples/decode_dump" "$f" 1024 > "$WORK/$b.stream.raw" 2>/dev/null
    "$ROOT/target/release/senadec" --format raw-f32 "$f" -o "$WORK/$b.cli.raw" >/dev/null 2>&1
    "$FFMPEG" -hide_banner -loglevel error -i "$f" -map 0:a -f f32le -y "$WORK/$b.ffmpeg.raw"
    python3 tests/ffmpeg/verify.py decode "$WORK/$b.ffmpeg.raw" "$WORK/$b.stream.raw" "$WORK/$b.cli.raw" "$PLAYABLE"
done

# Seeking: the demuxer lands on a cue point and the decoder cold-starts
# there (LF ramps in from the first independency AU); pts-driven trimming is
# exact, and the output converges to the continuous decode well inside 1 s.
DUMP="$ROOT/target/release/examples/decode_dump"
for asset in assets/e2e/01__p300__lfa__hf144.sena assets/e2e/09__p600__lfd__hf136.sena; do
    b="$(basename "$asset" .sena)"
    python3 tests/ffmpeg/verify.py seek "$FFMPEG" "$asset" "$WORK/$b.stream.raw" 1.0 5.5 9.97 15.003 19.99
done

# Container identification: a stock Matroska probe owns the file (no
# deferral); the track resolves to the sena codec via wFormatTag 0x534E.
cp assets/e2e/01__p300__lfa__hf144.sena "$WORK/renamed.mka"
cp assets/e2e/01__p300__lfa__hf144.sena "$WORK/noext.bin"
for f in assets/e2e/01__p300__lfa__hf144.sena "$WORK/renamed.mka" "$WORK/noext.bin"; do
    python3 tests/ffmpeg/verify.py probe "$FFPROBE" "$f" sena
done
"$FFMPEG" -hide_banner -loglevel error -i assets/e2e/01__src.flac -c:a libopus -b:a 96k -y "$WORK/plain.mka"
python3 tests/ffmpeg/verify.py probe "$FFPROBE" "$WORK/plain.mka" opus

# User tags + cover art visibility (stock matroskadec surfaces both).
"$FFMPEG" -hide_banner -loglevel error -f lavfi -i color=red:s=32x32 -frames:v 1 -y "$WORK/cover.png"
"$ROOT/target/release/examples/make_fixture" assets/e2e/01__p300__lfa__hf144.sena "$WORK/fixture.sena" \
    "TITLE=Sena Test" "ARTIST=Decoder Suite" "REPLAYGAIN_TRACK_GAIN=-6.53 dB" \
    --art cover.png image/png "$WORK/cover.png" 2>/dev/null
python3 tests/ffmpeg/verify.py tags "$FFPROBE" "$WORK/fixture.sena" \
    "TITLE=Sena Test" "ARTIST=Decoder Suite" "REPLAYGAIN_TRACK_GAIN=-6.53 dB"
python3 tests/ffmpeg/verify.py art "$FFMPEG" "$WORK/fixture.sena" "$WORK/cover.png" "$WORK"

# Missing core library: clean, actionable failure (no crash, no silence).
python3 tests/ffmpeg/verify.py nolib "$FFMPEG" assets/e2e/01__p300__lfa__hf144.sena

echo "ALL FFMPEG DECODER CHECKS PASSED"
