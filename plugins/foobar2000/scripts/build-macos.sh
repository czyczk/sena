#!/usr/bin/env bash
# macOS view: build the universal .component and the installer package.
set -euo pipefail
cd "$(dirname "$0")/../../.."
python3 plugins/foobar2000/scripts/build.py mac
python3 plugins/foobar2000/scripts/build.py package
