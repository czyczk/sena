#!/usr/bin/env bash
# Golden test: senaenc output vs reference decoders (scattered pipeline).
# usage: run.sh <input.wav> <300|600> <kbps>
set -euo pipefail
IN="${1:?input wav}"; PROFILE="${2:?profile}"; KBPS="${3:?kbps}"
ROOT="$(cd "$(dirname "$0")/../.." && pwd)"
WD="$(mktemp -d)"
LOG="$WD/enc.log"
TMPDIR="$WD" "$ROOT/target/release/senaenc" --profile "$PROFILE" --keep-workdir "$KBPS" "$IN" "$WD/out.sena" 2> "$LOG" && cat "$LOG" >&2
SENAWD="$(sed -n 's/^senaenc: workdir kept at //p' "$LOG" | tail -1)"
python3 "$ROOT/tests/golden/validate.py" "$IN" "$PROFILE" "$WD/out.sena" "$SENAWD"
rm -rf "$WD"
echo "golden: PASS"
