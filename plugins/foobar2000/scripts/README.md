# foo_input_sena one-click build scripts

Unified entry points (from repo root):

| Host | Command | Produces |
|---|---|---|
| Linux / WSL | `just doctor` then `just senadec-plugin-fb2k-all` | Windows x64 + ARM64EC DLLs, macOS universal component, `.fb2k-component` package |
| Windows PowerShell | `.\plugins\foobar2000\scripts\build-windows.ps1` | Windows x86 + x64 + ARM64EC DLLs and package |
| macOS | `./plugins/foobar2000/scripts/build-macos.sh` | universal `.component` and package |

Individual Python commands:

```text
python3 plugins/foobar2000/scripts/build.py doctor
python3 plugins/foobar2000/scripts/build.py check [--arch x64]   # preflight only, exit 1 on problems
python3 plugins/foobar2000/scripts/build.py windows --vs 2022   # or --vs 2026 / auto
python3 plugins/foobar2000/scripts/build.py mac
python3 plugins/foobar2000/scripts/build.py package [--scope windows-x64]
python3 plugins/foobar2000/scripts/build.py install             # copy DLL into foobar2000 profile
```

Build ergonomics:

- Every build command preflights its scope BEFORE compiling; a piece whose
  toolchain is incomplete is skipped and the rest still builds. The macOS
  build compiles the Rust staticlib before any C++, so a Rust-side failure
  never wastes a C++ compile.
- Runs end with a summary naming the gaps and the catch-up recipes (build
  the missing piece, then `package` again - packaging only zips what is in
  `dist/`, it never rebuilds).
- `package --scope all|windows|windows-x86|windows-x64|windows-arm64ec|mac`
  selects what goes into the `.fb2k-component`; scoped packages are named
  accordingly (e.g. `foo_input_sena-0.1.0-windows-x64.fb2k-component`),
  while `all` keeps the canonical `foo_input_sena-0.1.0.fb2k-component`.
- The matching just recipes: `senadec-plugin-fb2k-check [arch]`,
  `senadec-plugin-fb2k-package [scope]`, and the combined
  `senadec-plugin-fb2k-{windows-x86,windows-x64,windows-arm64ec,mac}-package`.

Environment overrides: `FOOBAR_SDK`, `FOOBAR_EXE`, `FOOBAR_APPDATA`,
`MACOSX_SDK`, `LD64_LLD`.

Minimum toolchain enforced by `doctor`:

- Rust/cargo >= 1.88 with the requested `rust-std` targets.
- Windows: Visual Studio 2022 or 18 (2026) with VC tools (auto-detected by
  vswhere; 2022 preferred when present, 2026 accepted/fallback).
- Linux/WSL Windows cross-build: `cargo-xwin`.
- Linux -> macOS cross-build: clang++ and `ld64.lld`; the script provisions
  `lld-14`/`libllvm14` locally under `.cache/tools` when `apt-get download`
  and `dpkg-deb` are available, and downloads MacOSX11.3.sdk to
  `.cache/MacOSX11.3.sdk` on demand.
- macOS: Xcode command line tools (`clang++`, `xcrun`).

WSL notes: MSBuild intermediates stay on the Windows temp drive (MSVC
lowercases UNC paths, WSL is case-sensitive), and the final artifacts are
copied back to `plugins/foobar2000/foo_input_sena/dist/`.
