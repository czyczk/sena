# One-click build/verify for sena + foo_input_sena.
# Common overrides: FOOBAR_SDK, FOOBAR_EXE, MACOSX_SDK, VS preference via `just windows vs=2022`.

set shell := ["bash", "-uc"]

repo := justfile_directory()
script := repo / "plugins" / "foobar2000" / "scripts" / "build.py"
senaenc_script := repo / "scripts" / "build-senaenc.py"
senaenc_out := env_var_or_default("SENAENC_OUT", "build/senaenc")
senaenc_vs := env_var_or_default("SENAENC_VS", "auto")
senaenc_linker := env_var_or_default("SENAENC_LINKER", "auto")
FOOBAR_SDK := env_var_or_default("FOOBAR_SDK", "~/src/public/foobar2000-research/SDK-2025-03-07")
FOOBAR_EXE := env_var_or_default("FOOBAR_EXE", "C:\\Program Files\\foobar2000\\foobar2000.exe")

doctor:
    @python3 "{{script}}" doctor

check:
    cargo check --workspace

test:
    cargo test --workspace

rust-libs arch="":
    @python3 "{{script}}" rust-libs {{ if arch == "" { "" } else { "--arch " + arch } }}

windows vs="auto":
    @python3 "{{script}}" windows --vs {{vs}}

mac:
    @python3 "{{script}}" mac

package:
    @python3 "{{script}}" package

all vs="auto":
    @python3 "{{script}}" all --vs {{vs}}

install-foobar:
    @python3 "{{script}}" install

# ---------------------------------------------------------------------------
# Standalone senaenc release builds (no opusenc/exhale needed at build time).
# Default: Windows x86-64 + Windows arm64 + macOS Universal into
# build/senaenc (repo-local; /build/ is git-ignored).
#
# Usage:
#   just senaenc                        # the three requested targets
#   just senaenc "linux-x64"            # one or more supported aliases/triples
#   SENAENC_OUT=/tmp/x just senaenc     # output elsewhere via env (also: --set senaenc_out ...)
#   SENAENC_VS=2022 just senaenc        # VS auto/2022/2026
#   SENAENC_LINKER=xwin just senaenc    # auto/xwin/vs/cargo
senaenc targets="windows-x64 windows-arm64 macos-universal":
    @python3 "{{senaenc_script}}" --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}" {{targets}}

senaenc-win-x64:
    @python3 "{{senaenc_script}}" --target windows-x64 --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senaenc-win-arm64:
    @python3 "{{senaenc_script}}" --target windows-arm64 --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senaenc-macos-universal:
    @python3 "{{senaenc_script}}" --target macos-universal --out "{{senaenc_out}}"

senaenc-list-targets:
    @python3 "{{senaenc_script}}" --help
