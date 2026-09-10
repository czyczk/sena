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
senaenc [--profile 300|600] [--opus-original|--opus-senav] [--bypass-recommendations] <bitrate_kbps> <in.wav|-> <out.sena>
```

- `<bitrate_kbps>`: total Sena bitrate. Hard minimum: 64k for @600, 56k for
  @300 (the xHE-AAC allotment plus a 32k Opus floor); lower requests are
  rejected. Below 128k a plain Opus encode is recommended instead and
  senaenc refuses (exit 5) unless `--bypass-recommendations` is given.
- The xHE-AAC track is allotted its measured average spend (24k for @300,
  32k for @600) and the remainder goes to Opus as its nominal VBR bitrate.
  Opus' own VBR float (~+16k) is accepted, so e.g. 160k at @600 yields
  ~176k actual (32k xHE-AAC + 128k Opus + float).
- Input `-` reads a WAV stream from stdin (foobar2000 converter style).
- Input WAV may be 8/16/24-bit PCM or 32-bit IEEE float, any sample rate
  (24k, 32k, 44.1k, 48k, 88.2k, 96k, ...). senaenc normalizes it to 48 kHz
  with a zero-phase rational resampler before the crossover.
- The input is consumed incrementally: the decode-resample-split-downsample
  pipeline runs chunk by chunk while the input arrives (so a feeding host
  such as the foobar2000 converter sees its progress bar advance with the
  real work), then the two codec inputs (lf.wav / hf.wav) are encoded.
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

The container carries an `Audio SHA256` content hash: SHA-256 of the
encoded audio elementary streams that the file carries (the Opus stream:
OpusHead + Opus packets; the xHE-AAC stream: ASC + raw AUs), length-prefixed
in Sena track order. It is deterministic for a given encoded stream set and
does not hash the input PCM, the decoded output, timestamps, tags or other
container layout. It is shown in foobar2000's Properties (Details, next to
Codec / Codec profile) and printed by `senadec --info`.

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
just doctor                          # build-environment check
just test                            # workspace tests
just senadec-plugin-fb2k-all         # windows x86/x64/arm64ec + macOS + zipped .fb2k-component
just senaenc                         # encoder CLI release binaries (windows x64/arm64 + Linux x64/arm64 + macOS Universal)
just senadec-bin                     # decoder CLI release binaries (same targets)
```

Plugin recipe names are namespaced (`senadec-plugin-fb2k-*`). Every build
runs a per-scope preflight before compiling anything; a piece whose
toolchain is incomplete is skipped while the rest continues, and each run
ends with a summary that names the gaps plus the exact catch-up recipes
(build the missing piece, then re-package). Useful subsets:

```bash
just senadec-plugin-fb2k-check x64            # preflight only: can x64 build?
just senadec-plugin-fb2k-windows-x64          # build just the x64 DLL
just senadec-plugin-fb2k-package windows-x64  # zip dist/ into a scope-named
                                              # foo_input_sena-0.1.0-windows-x64.fb2k-component
just senadec-plugin-fb2k-windows-x64-package  # build x64 + scoped package
just senadec-plugin-fb2k-package              # re-package everything in dist/ (no rebuild)
```

The full-platform package lands in `plugins/foobar2000/foo_input_sena/dist/
foo_input_sena-0.1.0.fb2k-component`; scoped packages carry the scope in
the file name.

Standalone `senaenc` release builds (no `opusenc`/`exhale` needed at build
time; they are runtime dependencies only):

```bash
just senaenc                                        # Windows x64/arm64 + Linux x64/arm64 + macOS Universal -> build/senaenc
just senaenc "linux-x64"                            # any supported alias/full Rust triple
SENAENC_OUT=/tmp/x SENAENC_VS=2026 just senaenc     # env overrides
just --set senaenc_out /tmp/x --set senaenc_vs 2022 senaenc
just senaenc-win-x64
just senaenc-win-arm64
just senaenc-linux-x64
just senaenc-linux-arm64
just senaenc-macos-universal
just senaenc-list-targets
```

Multi-target runs check every target's `rust-std` up front, skip failed
targets instead of aborting, and end with a summary naming the failures
plus the `just senaenc "<targets>"` command that retries only them.

The default output directory is `build/senaenc` (repo-local; `/build/` is
git-ignored). Pass `--out` / `SENAENC_OUT` / `--set senaenc_out ...` to write
elsewhere; relative paths resolve against the repo root.

Supported targets: `windows-x86`, `windows-x64`, `windows-arm64`,
`windows-arm64ec`, `macos-x64`, `macos-arm64`, `macos-universal`,
`linux-x64`, `linux-arm64`, or the equivalent full Rust target triples.
Unsupported targets fail with an explicit error. Linux triples matching the
host arch build natively; cross ones (e.g. `linux-x64` on an aarch64 host)
are linked through `cargo zigbuild` when zig is present under
`.cache/tools/zig`, or an installed `<arch>-linux-gnu-gcc`. Builds use the
rustup toolchain from PATH when it works, and a missing target rust-std is
reported as `rustup target add <triple>`. When no usable cargo is on PATH
the build falls back to the repo-local copied toolchain
`.toolchains/stable` (force a mode with `SENA_TOOLCHAIN=rustup|repo`); see
`notes/build-and-cross.md`. Windows linker modes are
`auto` (real MSVC link.exe via VS when available, otherwise cargo-xwin),
`xwin`, `vs`, and `cargo`; `--vs auto|2022|2026` selects the Visual Studio
instance when VS linking is used.

See `plugins/foobar2000/scripts/README.md` for per-host one-click details.
