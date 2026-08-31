# foo_input_sena

foobar2000 input component for Sena files.

- Windows payloads:
  - `dist/windows/x64/foo_input_sena.dll` (x64 and ARM64EC fallback)
  - `dist/windows/arm64ec/foo_input_sena.dll` (ARM64X; dumpbin reports
    `8664 machine (x64) (ARM64X)`)
- macOS payload: `dist/mac/foo_input_sena.component` (universal
  `arm64` + `x86_64` Mach-O bundle).
- Packaged component: `dist/foo_input_sena-0.1.0.fb2k-component`.

The C++ shim is thin: all container parsing, codec decode, trim/mix, dynamic
bitrate accounting and Matroska tag rewriting live in the shared `sena-dec`
Rust C ABI (`sena_dec.h`).

## Windows build

```bat
cargo xwin build -p sena-dec --target i686-pc-windows-msvc --lib --release
cargo xwin build -p sena-dec --target x86_64-pc-windows-msvc --lib --release
cargo xwin build -p sena-dec --target arm64ec-pc-windows-msvc --lib --release
msbuild foo_input_sena.vcxproj /p:Configuration=Release /p:Platform=Win32
msbuild foo_input_sena.vcxproj /p:Configuration=Release /p:Platform=x64
msbuild foo_input_sena.vcxproj /p:Configuration=Release /p:Platform=ARM64EC
```

`FOOBAR_SDK` and `SENA_LIB_DIR` point at the foobar2000 SDK and the
per-platform `libs/<Platform>` directory (see `notes/build-and-cross.md`).

## macOS build

See `notes/build-and-cross.md`: Rust staticlibs for both Apple targets,
SDK/pfc/component_client/shared compiled with `clang++ -target ...`, linked
with `ld64.lld -bundle`, then fat-wrapped into the `.component` bundle.

## Notes

- Runtime foobar2000 host validation and SDK `foo_input_validator` runs are
  documented as the remaining host-side check in
  `notes/foobar2000-plugin.md`.
- Design/research notes: `notes/foobar2000-plugin.md`.
