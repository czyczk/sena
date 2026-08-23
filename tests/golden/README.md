# Golden tests (reference pipeline parity)

The Python pipeline in the reference repository is the source of truth for
DSP constants and behavior. These tests validate senaenc against it.

## Prerequisites

- python3 with numpy/scipy/soundfile/soxr
- `xhedec` and `opusdec` on PATH
- senaenc built (cargo build --release), with `exhale` and `opusenc`
  symlinked into `target/release/` (or beside the binary)

## Running

```bash
tests/golden/run.sh <input.wav> <profile: 300|600> <kbps>
```

The script:
1. runs senaenc with --keep-workdir,
2. parses the .sena container (tracks, tags, delays, cluster count) and
   verifies packet-level bit-equality with the source elementary streams,
3. decodes both tracks with the reference decoders and measures
   low-frequency envelope sigma and correlation against the input,
4. checks the measured alignment lags against the per-profile constants
   (3072 / 1536 samples at 48 kHz) within a 4-sample tolerance.
