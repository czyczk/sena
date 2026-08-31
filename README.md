# Sena

Dual-track lossy audio codec: low band xHE-AAC + high band Opus in a
Matroska container (`.sena` / `.mka`).

Workspace crates:

- `crates/sena-core` — shared constants (profiles, pad, delays, bitrate rule).
- `crates/sena-dsp` — zero-phase rational resampler and FIR crossover.
- `crates/sena-mux` — Matroska/EBML writer for Sena.
- `crates/sena-enc` — encoder pipeline (WAV in, subprocess drivers, packet extraction).
- `crates/sena-dec` — decoder core, C ABI, tag editor.
- `bins/senaenc`, `bins/senadec` — CLI tools.
- `plugins/foobar2000/foo_input_sena` — foobar2000 input component.

## senaenc command line

```text
senaenc [--profile 300|600] [--opus-original|--opus-senav] <bitrate_kbps> <in.wav|-> <out.sena>
```

- `<bitrate_kbps>`: total Sena bitrate; >= 160. At 160k the xHE-AAC track is
  deducted (16k for @300, 24k for @600) and the remainder goes to Opus.
- Input `-` reads a WAV stream from stdin (foobar2000 converter style).
- Input WAV may be 8/16/24-bit PCM or 32-bit IEEE float, any sample rate
  (24k, 32k, 44.1k, 48k, 88.2k, 96k, ...). senaenc normalizes it to 48 kHz
  with a zero-phase rational resampler before the crossover.
- Required binaries next to `senaenc.exe` or on PATH:
  - `exhale[.exe]` >= 1.2.2
  - `opusenc[.exe]` >= 1.6.1 (used for <= 192 kbit/s by default)
  - `opusenc-senav[.exe]` (used for > 192 kbit/s by default; version output
    contains `Opus SenaV`)

`exhale[.exe]` / `opusenc[.exe]` must be named with the platform extension
(`exhale.exe` on Windows); the lookup probes the directory of
`senaenc.exe` first (including `.exe`/`PATHEXT` variants), then `PATH`.

Examples:

```bash
senaenc --profile 300 160 in.wav out.sena
senaenc --profile 600 --opus-senav 192 - out.sena < in.wav
senaenc doctor   # check the required/optional tools without encoding
```

`senaenc doctor` reports each tool's location and version without
encoding: `exhale` and `opusenc` are required (missing, not runnable, or
older than the baseline is fatal, exit code 4), while `opusenc-senav` is
optional (a missing or non-SenaV build only warns and disables senav mode
and automatic selection above 192 kbit/s).

## senadec command line

```text
senadec [--format wav-f32|wav-s24|wav-s16|raw-f32|raw-s24|raw-s16]
        [--dither none] [--dump-tracks PREFIX] [--info] in.sena [-o out.wav]
```

Default output is `wav-f32`; `-o -` or no `-o` writes stdout. s16 uses
deterministic TPDF dither by default; `--dither none` disables it.

## foobar2000 converter presets

In foobar2000 Preferences -> Tools -> Converter -> Output presets, add an
encoder entry:

| Field | Value |
|---|---|
| Encoder file | full path to `senaenc.exe` |
| Extension | `sena` |
| Parameters | `--profile 300 --opus-original 160 - %d` |
| Format is | lossy |
| Highest BPS mode supported | 32 |

For the 600 Hz profile:

```text
--profile 600 --opus-original 160 - %d
```

Notes:

- Foobar pipes a WAV stream on stdin (`-`) and provides the destination
  filename as `%d`; this is why the parameters end with `- %d`.
- No `--ignorelength` is needed: senaenc handles both streaming WAV data
  chunks and normal files.
- `Highest BPS mode supported: 32` is correct: the encoder accepts 32-bit
  float WAV input and keeps the signal path float32 internally.
- Place `exhale.exe` and `opusenc.exe` next to `senaenc.exe` (or on PATH).
  For >192 kbit/s presets also place `opusenc-senav.exe` and use
  `--opus-senav`.
- Sena encodes at 48 kHz. Let the converter pass the source sample rate;
  senaenc normalizes any rate to 48 kHz itself.

## Build

```bash
just doctor
just all        # builds sena-dec libs + Windows x86/x64/arm64ec + macOS component + package
just test
```

Standalone `senaenc` release builds (no `opusenc`/`exhale` needed at build
time; they are runtime dependencies only):

```bash
just senaenc                                        # Windows x64 + arm64 + macOS Universal -> build/senaenc
just senaenc "linux-x64"                            # any supported alias/full Rust triple
SENAENC_OUT=/tmp/x SENAENC_VS=2026 just senaenc     # env overrides
just --set senaenc_out /tmp/x --set senaenc_vs 2022 senaenc
just senaenc-win-x64
just senaenc-win-arm64
just senaenc-macos-universal
just senaenc-list-targets
```

The default output directory is `build/senaenc` (repo-local; `/build/` is
git-ignored). Pass `--out` / `SENAENC_OUT` / `--set senaenc_out ...` to write
elsewhere; relative paths resolve against the repo root.

Supported targets: `windows-x86`, `windows-x64`, `windows-arm64`,
`windows-arm64ec`, `macos-x64`, `macos-arm64`, `macos-universal`,
`linux-x64`, `linux-arm64`, or the equivalent full Rust target triples.
Unsupported targets fail with an explicit error. Windows linker modes are
`auto` (real MSVC link.exe via VS when available, otherwise cargo-xwin),
`xwin`, `vs`, and `cargo`; `--vs auto|2022|2026` selects the Visual Studio
instance when VS linking is used.

See `plugins/foobar2000/scripts/README.md` for per-host one-click details.
