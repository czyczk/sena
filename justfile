# One-click build/verify for sena + foo_input_sena.
# Common overrides: FOOBAR_SDK, FOOBAR_EXE, MACOSX_SDK, VS preference via `just windows vs=2022`.

set shell := ["bash", "-uc"]

repo := justfile_directory()
script := repo / "plugins" / "foobar2000" / "scripts" / "build.py"
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
