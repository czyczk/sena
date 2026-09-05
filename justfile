# One-click build/verify for Sena (encoder, decoder and the foobar2000 plugin).
#
# Recipe groups:
#   general          doctor / check / test
#   plugin (fb2k)    senadec-plugin-fb2k-*     (foo_input_sena; the Sena decoder)
#   senaenc          senaenc-*                 (encoder CLI release builds)
#   senadec          senadec-bin-*             (decoder CLI release builds)
#
# Common overrides: FOOBAR_SDK, FOOBAR_EXE, MACOSX_SDK, VS preference via
# `just senadec-plugin-fb2k-windows vs=2022`.

set shell := ["bash", "-uc"]

repo := justfile_directory()
script := repo / "plugins" / "foobar2000" / "scripts" / "build.py"
release_bin_script := repo / "scripts" / "build-release-bin.py"
plugin_out := env_var_or_default("SENA_PLUGIN_OUT", "plugins/foobar2000/foo_input_sena/dist")
senaenc_out := env_var_or_default("SENAENC_OUT", "build/senaenc")
senadec_out := env_var_or_default("SENADEC_OUT", "build/senadec")
senaenc_vs := env_var_or_default("SENAENC_VS", "auto")
senaenc_linker := env_var_or_default("SENAENC_LINKER", "auto")
# Default release targets shared by the senaenc and senadec-bin recipes.
release_targets := "windows-x64 windows-arm64 linux-x64 linux-arm64 macos-universal"
FOOBAR_SDK := env_var_or_default("FOOBAR_SDK", "~/src/public/foobar2000-research/SDK-2025-03-07")
FOOBAR_EXE := env_var_or_default("FOOBAR_EXE", "C:\\Program Files\\foobar2000\\foobar2000.exe")

# ---------------------------------------------------------------------------
# General
# ---------------------------------------------------------------------------

doctor:
    @python3 "{{script}}" doctor

check:
    cargo check --workspace

test:
    cargo test --workspace

# ---------------------------------------------------------------------------
# foobar2000 plugin: foo_input_sena (the Sena decoder plugin)
#
#   just senadec-plugin-fb2k-windows           # Windows x64 + arm64ec DLLs
#   just senadec-plugin-fb2k-windows-x86       # one specific arch
#   just senadec-plugin-fb2k-mac               # macOS universal component
#   just senadec-plugin-fb2k-all               # Windows + macOS + package
#
#   just senadec-plugin-fb2k-package           # zip everything in dist/ into
#                                              # one .fb2k-component
#   just senadec-plugin-fb2k-package windows-x64
#                                              # scoped package (named
#                                              # ...-windows-x64.fb2k-component)
#   just senadec-plugin-fb2k-windows-x64-package
#                                              # build one arch + scoped package
#   just senadec-plugin-fb2k-check             # preflight only: can it build?
#   just senadec-plugin-fb2k-check x64         # ... for one arch
#
# Every build runs a per-scope preflight BEFORE compiling (a piece whose
# toolchain is incomplete is skipped, the rest continues; the mac build
# compiles the Rust staticlib before any C++). Runs end with a summary that
# names the gaps and the exact catch-up recipes: build the missing piece,
# then `just senadec-plugin-fb2k-package` re-packages everything in dist/
# without rebuilding. The packaged file lands in
# plugins/foobar2000/foo_input_sena/dist/
# ---------------------------------------------------------------------------

senadec-plugin-fb2k-doctor:
    @python3 "{{script}}" doctor

senadec-plugin-fb2k-check arch="":
    @python3 "{{script}}" check {{ if arch == "" { "" } else { "--arch " + arch } }}

senadec-plugin-fb2k-rust-libs arch="":
    @python3 "{{script}}" rust-libs {{ if arch == "" { "" } else { "--arch " + arch } }}

senadec-plugin-fb2k-windows vs="auto":
    @python3 "{{script}}" windows --arch x64 --arch arm64ec --vs {{vs}}

senadec-plugin-fb2k-windows-x86 vs="auto":
    @python3 "{{script}}" windows --arch x86 --vs {{vs}}

senadec-plugin-fb2k-windows-x64 vs="auto":
    @python3 "{{script}}" windows --arch x64 --vs {{vs}}

senadec-plugin-fb2k-windows-arm64ec vs="auto":
    @python3 "{{script}}" windows --arch arm64ec --vs {{vs}}

senadec-plugin-fb2k-mac:
    @python3 "{{script}}" mac

senadec-plugin-fb2k-package scope="all":
    @python3 "{{script}}" package --scope {{scope}}

senadec-plugin-fb2k-windows-x86-package vs="auto":
    @python3 "{{script}}" windows --arch x86 --vs {{vs}} && python3 "{{script}}" package --scope windows-x86

senadec-plugin-fb2k-windows-x64-package vs="auto":
    @python3 "{{script}}" windows --arch x64 --vs {{vs}} && python3 "{{script}}" package --scope windows-x64

senadec-plugin-fb2k-windows-arm64ec-package vs="auto":
    @python3 "{{script}}" windows --arch arm64ec --vs {{vs}} && python3 "{{script}}" package --scope windows-arm64ec

senadec-plugin-fb2k-mac-package:
    @python3 "{{script}}" mac && python3 "{{script}}" package --scope mac

senadec-plugin-fb2k-all vs="auto":
    @python3 "{{script}}" all --vs {{vs}}

senadec-plugin-fb2k-install:
    @python3 "{{script}}" install

# ---------------------------------------------------------------------------
# senaenc release binaries (encoder CLI; no opusenc/exhale needed at build
# time - runtime deps only). Default: Windows x64 + arm64, Linux x64 + arm64
# and macOS Universal into build/senaenc (repo-local, git-ignored).
#
#   just senaenc                        # the five default targets
#   just senaenc "linux-x64"            # any supported alias/full triple
#   SENAENC_OUT=/tmp/x just senaenc     # output elsewhere (--set senaenc_out)
#   SENAENC_VS=2022 just senaenc        # VS auto/2022/2026
#   SENAENC_LINKER=xwin just senaenc    # auto/xwin/vs/cargo
# ---------------------------------------------------------------------------

senaenc targets=release_targets:
    @python3 "{{release_bin_script}}" --pkg senaenc --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}" {{targets}}

senaenc-win-x64:
    @python3 "{{release_bin_script}}" --pkg senaenc --target windows-x64 --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senaenc-win-arm64:
    @python3 "{{release_bin_script}}" --pkg senaenc --target windows-arm64 --out "{{senaenc_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senaenc-linux-x64:
    @python3 "{{release_bin_script}}" --pkg senaenc --target linux-x64 --out "{{senaenc_out}}"

senaenc-linux-arm64:
    @python3 "{{release_bin_script}}" --pkg senaenc --target linux-arm64 --out "{{senaenc_out}}"

senaenc-macos-universal:
    @python3 "{{release_bin_script}}" --pkg senaenc --target macos-universal --out "{{senaenc_out}}"

senaenc-list-targets:
    @python3 "{{release_bin_script}}" --help

# ---------------------------------------------------------------------------
# senadec release binaries (decoder CLI). Same targets/linker modes as
# senaenc; outputs go to build/senadec by default.
# ---------------------------------------------------------------------------

senadec-bin targets=release_targets:
    @python3 "{{release_bin_script}}" --pkg senadec --out "{{senadec_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}" {{targets}}

senadec-bin-win-x64:
    @python3 "{{release_bin_script}}" --pkg senadec --target windows-x64 --out "{{senadec_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senadec-bin-win-arm64:
    @python3 "{{release_bin_script}}" --pkg senadec --target windows-arm64 --out "{{senadec_out}}" --vs "{{senaenc_vs}}" --linker "{{senaenc_linker}}"

senadec-bin-linux-x64:
    @python3 "{{release_bin_script}}" --pkg senadec --target linux-x64 --out "{{senadec_out}}"

senadec-bin-linux-arm64:
    @python3 "{{release_bin_script}}" --pkg senadec --target linux-arm64 --out "{{senadec_out}}"

senadec-bin-macos-universal:
    @python3 "{{release_bin_script}}" --pkg senadec --target macos-universal --out "{{senadec_out}}"

senadec-bin-list-targets:
    @python3 "{{release_bin_script}}" --pkg senadec --help
