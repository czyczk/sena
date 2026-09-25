#!/usr/bin/env bash
# Apply the Sena demuxer patch to an FFmpeg source tree and rebuild
# ffmpeg/ffprobe, reusing the tree's existing configure arguments.
#
# usage: rebuild.sh <ffmpeg-src> [--jobs N]
set -euo pipefail

HERE="$(cd "$(dirname "$0")" && pwd)"
SRC="$(cd "$1" && pwd)"
JOBS="$(nproc 2>/dev/null || sysctl -n hw.ncpu 2>/dev/null || echo 8)"

python3 "$HERE/apply.py" "$SRC"
cd "$SRC"
if [ -f config.h ]; then
    ARGS="$(sed -n 's/^#define FFMPEG_CONFIGURATION "\(.*\)"$/\1/p' config.h | head -1)"
    [ -n "$ARGS" ] || { echo "rebuild: no FFMPEG_CONFIGURATION in config.h" >&2; exit 1; }
    echo "configure: $ARGS"
    # The recorded configuration is a flat, single-quoted-arg-free string
    # written by configure itself; word-splitting is the intended replay.
    ./configure $ARGS
else
    echo "rebuild: $SRC is not configured yet; run ./configure first" >&2
    exit 1
fi
make -j"$JOBS" ffmpeg ffprobe
echo "rebuild: OK - $SRC/ffmpeg"
