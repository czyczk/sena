#!/usr/bin/env bash
# Linux / WSL view: build every target that this host can produce.
set -euo pipefail
cd "$(dirname "$0")/../../.."
python3 plugins/foobar2000/scripts/build.py all --vs "${VS:-auto}"
