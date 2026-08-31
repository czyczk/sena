# Build environment and cross-compilation notes

## cargo check workaround on this machine

The `/snap/bin/cargo` shim fails with a rustup/DBus transient-scope error.
Interactive shells use the real toolchain binaries directly:

```bash
# writable cargo home inside the repo (the system ~/.cargo is read-only here)
export CARGO_HOME=/home/zenas/src/Rust_Projects/sena/.cargo-home
# copied writable toolchain (system ~/.rustup toolchain dir is read-only)
export PATH=/home/zenas/src/Rust_Projects/sena/.toolchains/stable/bin:$PATH
cargo check --workspace
```

`.toolchains/`, `.cargo-home/`, `.cache/` are git-ignored local state.

Target standard libraries were extracted manually from the 2026-07-16
toolchain manifest into `.toolchains/stable/lib/rustlib/` for:

- `x86_64-pc-windows-msvc`
- `arm64ec-pc-windows-msvc`
- `x86_64-apple-darwin`
- `aarch64-apple-darwin`

## Verified builds

- Linux aarch64: `cargo test --workspace` passes (decoder 10/10 tests).
- Windows x86_64: `cargo xwin build -p sena-dec --target x86_64-pc-windows-msvc --lib --release`
  produces `sena_dec.lib`. XWIN cache is at `.cache/cargo-xwin`.
- Windows ARM64EC: Rust staticlib built with cargo-xwin after setting
  sena-dec crate-type to `["rlib","staticlib"]` (staticlib creation does not
  need ARM64EC CRT import libs).
- macOS arm64/x86_64: Rust staticlibs built with the copied Linux-host
  toolchain plus manually extracted Apple target std libs.

## foobar2000 plugin build status (verified 2026-08-31)

All four P0 payloads build and pass structural checks:

- `dist/windows/x64/foo_input_sena.dll`: PE x64, exports
  `foobar2000_get_interface`.
- `dist/windows/arm64ec/foo_input_sena.dll`: PE `8664 machine (x64) (ARM64X)`
  (dumpbin), exports `foobar2000_get_interface`.
- `dist/mac/foo_input_sena.component/Contents/MacOS/foo_input_sena`:
  Mach-O universal `[arm64:x86_64]`, exports `_foobar2000_get_interface`
  for both slices (llvm-nm).
- `dist/foo_input_sena-0.1.0.fb2k-component` packages exactly
  `x64/`, `arm64ec/`, `mac/foo_input_sena.component`.

Windows recipe:
1. `cargo xwin build -p sena-dec --target <triple> --lib --release`.
2. `MSBuild foo_input_sena.vcxproj /p:Configuration=Release /p:Platform=x64|ARM64EC`
   with `FOOBAR_SDK` and `SENA_LIB_DIR` pointing at the SDK and prebuilt
   `libs/<Platform>` directory. SDK/pfc/component_client `.lib` files were
   produced by MSBuild from the SDK projects (v143; helpers lib is not
   needed because `dynamic_bitrate_helper.cpp` is compiled into the plugin).

macOS recipe (Linux cross-build):
1. Download `MacOSX11.3.sdk.tar.xz` (phracker/MacOSX-SDKs).
2. Build Rust staticlibs for `aarch64-apple-darwin` and
   `x86_64-apple-darwin`.
3. Compile the SDK/pfc/component_client/shared Xcode source lists with
   `clang++ -target <arch>-apple-macos11 -isysroot <sdk> -stdlib=libc++`.
4. Archive with `llvm-ar`; link the component with `ld64.lld` (Ubuntu
   `lld-14` package extracted locally) and `-bundle`.
5. Create the fat binary by hand (fat_arch entries are 20 bytes: cpu, sub,
   offset, size, align) and wrap it in the `.component` bundle.

Runtime foobar2000 host / SDK foo_input_validator runs were not executed on
this Linux host; the binaries are structurally verified as above.

## One-click scripts (added 2026-08-31)

- Unified: `just doctor` / `just all`; individual Python command surface in
  `plugins/foobar2000/scripts/build.py`.
- Host wrappers: `build-windows.ps1`, `build-macos.sh`, `build-linux.sh`.
- VS selection: vswhere-based, default prefers 2022 and falls back to 18
  (2026); `--vs 2022|2026|auto` overrides.
- Verified from this WSL host:
  - `build.py windows --arch x64 --vs 2022` -> dist x64 DLL.
  - `build.py windows --arch arm64ec --vs 2022` -> dist ARM64EC DLL
    (ARM64X header verified by dumpbin in the earlier session).
  - `build.py mac` -> universal arm64+x86_64 component.
  - `build.py package` -> `.fb2k-component`.
  - `build.py install` -> copies the x64 DLL into
    `%APPDATA%\foobar2000-v2\user-components-x64\foo_input_sena`.
- Installed foobar2000 v2.25.9 is detected at
  `C:\Program Files\foobar2000\foobar2000.exe`. Headless component-load
  validation is still limited (no UI automation here); module enumeration
  showed the host runs, but user-components are not yet confirmed loaded.

## 2026-08-31 additions

- Windows x86 (Win32) target added to `build.py` / justfile / vcxproj.
  Built and verified with VS18 (2026): `14C machine (x86)`, exports
  `foobar2000_get_interface`.
- VS18 (2026) builds now pass `/m:1 /p:UseMultiToolTask=false
  /p:PrecompiledHeader=NotUsing`; without it, MSVC on this WSL host fails
  with C3859/C1076 (PCH virtual memory) for the foobar SDK projects.
- `.fb2k-component` layout now contains: root `foo_input_sena.dll` (x86),
  `x64/`, `arm64ec/`, and `mac/foo_input_sena.component`.

## Streaming decoder + fast probe (2026-08-31, evening)

- `crates/sena-dec/src/stream.rs` now provides the FFI decoder used by
  foobar2000. `sena_dec_open` parses/validates the container
  (`StreamingDecoder::open` -> `pipeline::probe`, no codec decode) and audio
  is produced incrementally in `sena_dec_read_f32`.
- LF and HF codec frames are queued independently (`VecDeque`) and only the
  consumed prefix is drained after each chunk, so a short chunk on one track
  never discards samples from the other. Bit accounting infos are trimmed by
  the same consumed frame counts.
- `sena_dsp::Resampler` gained `output_frames` / `input_frames_for_output`
  so streaming chunks map exactly between LF input frames and 48 kHz output
  frames.
- Seek is still linear (rebuild + discard buffered frames), but buffered
  frames at/after the target survive for the next read; the FFI test
  `abi_open_read_seek_and_dynamic_info` covers the final-100-frames case.
- Verified on `assets/e2e/01__p300__lfa__hf144.sena`: streaming produces all
  960000 frames and the same per-block payload-bit totals as the offline
  decoder (3510704 bits total, first block 14492 bits).
- `sena_dec_probe_info` is the info fast path; foobar `get_info` calls it
  and never instantiates the decoder for info scans.
- `sena_file_read_tags` is now a true range-scan fast path as well:
  `tags::scan_user_tags` reads only top-level EBML element headers and the
  small user-`Tags` payloads through seek+read, skipping every cluster
  payload. Unit test asserts the scan serves <10% of the file bytes and
  finds freshly rewritten tail tags; the FFI tag-rewrite test also calls
  `sena_file_read_tags` after rewriting.
- Cluster-level seek plan (not yet implemented): the muxer writes ~1000 ms
  clusters. Record each Cluster's first frame index + timestamp in `Demuxed`,
  then on backward/late seeks restart the codec decoders at cluster `N-1`
  (one cluster of warm-up), set `first=false`, align the LF/HF raw queues to
  the later of their two global output offsets (using the existing
  info-trim helpers), and discard through the target. Bit accounting is
  already global, so chunk rates stay correct.
- `cargo test --workspace` passes on Linux x86_64 after these changes
  (sena-dec 11/11).
- Rebuilt `just all --vs 2022` after every Rust/C++ change (all three
  Windows DLLs + universal macOS component + package). Package layout
  verified: root x86 DLL, `x64/`, `arm64ec/`,
  `mac/foo_input_sena.component`; all DLLs export
  `foobar2000_get_interface`, ARM64EC DLL keeps the `(ARM64X)` header.
- Installed the final x64 DLL to
  `%APPDATA%\foobar2000-v2\user-components-x64\foo_input_sena`. Runtime
  play test through foobar was not completed (WSL Windows interop stalls on
  a headless UI launch attempt; installed DLL copy is byte-identical to the
  packaged x64 payload).
