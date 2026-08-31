# foobar2000 plugin research notes (foo_input_sena)

Verdict first: the Sena component is **`foo_input_sena`**, not a `foo_pd_*`
packet decoder. These notes are the implementation reference for the
foobar2000 shim called out by the decoder spec (`C-compatible plugin ABI`).

---

## 1. Why `foo_input`, not `foo_pd`

The foobar2000 SDK has two adjacent extension points, and they solve different
problems:

- `input_entry` / `input_decoder` / `input_info_writer` (`SDK/input.h`) owns a
  *file format end to end*: extension/content-type matching, container parsing,
  metadata read/write, and decoding to PCM.
- `packet_decoder` (`SDK/packet_decoder.h`) only decodes *one elementary codec
  stream* that an existing container input has already demuxed. It is selected
  by the owning input (`owner_MP4`, `owner_matroska`, `owner_ADTS`, ...) and
  receives a setup struct (`packet_decoder::matroska_setup`) plus raw packets.
  It cannot see the other Sena track, cannot write file tags, and cannot own
  `.sena`.

Sena is a two-track Matroska container (`A_OPUS` + private `A_SENALF`) whose
playable output is the *sum* of two decoded tracks after resampling, gain
restore, and leading/trailing trim. A `foo_pd` for `A_SENALF` would only make
the built-in Matroska input decode the LF track; it can never produce the mixed
stereo timeline. Therefore the component must implement `input_entry` and
register `foo_input_sena`.

Consequences:

- Do not depend on the built-in Matroska input for Sena playback, seeking, or
  tag writing, even for `.mka`.
- Use the shared `sena-dec` C ABI from the C++ shim; all trim/mix logic stays
  in Rust and is identical on Windows and macOS.

---

## 2. Reference repositories

All clones are under `~/src/public/foobar2000-research/` (outside this repo).
`SDK-2025-03-07` is the official SDK archive downloaded from
<https://www.foobar2000.org/downloads/SDK-2025-03-07.7z> (source is at
`~/src/public/foobar2000-research/SDK-2025-03-07`).

| Repo (commit) | What it teaches |
|---|---|
| official SDK 2025-03-07 (`foobar2000/foo_sample/input_raw.cpp`) | Canonical minimal `input_stubs` + `input_singletrack_factory_t` input; same `input_raw.cpp` is compiled by both the `.vcxproj` and the mac `.xcodeproj` |
| `nu774/foo_input_caf` `8cd0516` | Best small production example: leading trim, end-padding trim, `dynamic_bitrate_helper` for real-time VBR bitrate, CAF tag read/write |
| `vgmstream/vgmstream` `05dbda9` (`fb2k/foo_vgmstream.cpp`) | Large multi-format input, multi-subsong handling, file-type registration, stream abstraction over foobar `file` |
| `M3MEMonster/foo_input_adlib_opl` `280b060` | Multi-subsong `input_factory_t`, dynamic sample-rate info, tag-unsupported path |
| `vsu/foo-input-dts` `b327981` | Per-frame dynamic tech info (`samplerate`) after first block |
| `hozuki/foo_input_hca` `a03bf05` | Example of *not* reporting dynamic bitrate (returns `false`), i.e. the `.tak` failure mode to avoid |
| `pnck/foo_input_ncm` `4cec3a7` | Container-wrapper input that delegates the inner codec to another `input_entry`, plus tag-write/remove semantics split across wrapper and inner file |
| `stuerp/foo_midi` `5238041` | Modern full component layout/build scripts (Windows only) |
| `TheQwertiest/foo_spotify` `ef3c0dd` | Complex custom input with `get_dynamic_info`, non-physical paths, no file tags |
| `kartun83/foo_tun_midi` `44217b8` | macOS-only `input_stubs` component; fetches/builds SDK 2025-03-07; generated Xcode project; `arm64` build script chain |
| `JendaT/fb2k-components-mac-suite` `5bbd0b2` | Community macOS component suite; useful secondary source for Xcode project generation and SDK library linking (official SDK remains the authority) |
| `ocean-feng/foobar_input_sacd` `f67683c` | Fork of a complex `foo_input_*` codebase (SACD) |
| `keithhanlon/foobar2000__macOS_components` `2911836` | Minimal macOS component Makefile examples (old-style `-dynamiclib`; prefer the official Xcode project) |
| `templeblock/foobar2000-plugins` `d504485` | Historic Windows component collection (not used for modern patterns) |

Key SDK files:

- `foobar2000/SDK/input.h`:
  - `input_decoder::get_dynamic_info` lines ~95-110.
  - `input_info_writer` / `input_info_writer_v2::remove_tags` lines ~175-203.
  - `input_entry::is_our_path` / `is_our_content_type` lines ~205-215.
- `foobar2000/SDK/input_impl.h`: `input_singletrack_impl` (single-track
  class with `retag()`), `input_stubs`, `input_singletrack_factory_t` (line
  511).
- `foobar2000/SDK/input.cpp`: `g_open_from_list` tries each matching input in
  priority order and skips `exception_io_no_handler_for_path` /
  `exception_io_unsupported_format` failures. This is what makes claiming
  `.mka` safe: reject non-Sena files with `exception_io_unsupported_format`
  and the next input gets a chance.
- `foobar2000/helpers/dynamic_bitrate_helper.{h,cpp}`: the official
  VBR-bitrate helper.
- `foobar2000/foo_input_validator/readme.txt`: decoder / tag-writer / fuzzer
  validator every new input should pass.

---

## 3. Requirement 1: tag writing

### 3.1 Implement `input_info_writer`, do not rely on built-in `.mka` tags

- `input_open_file_helper()` opens read/write when `p_reason ==
  input_open_info_write`; implement `retag(const file_info&, abort_callback&)`
  and `remove_tags(abort_callback&)` (names used by `input_singletrack_impl`).
- Built-in Matroska support does not help us: it does not know `.sena`, and
  for `.mka` the built-in input may be tried before or after ours depending on
  the user's decoder-priority table. Sena `.mka` also contains `A_SENALF`,
  which the built-in input may not handle. The only deterministic behavior is
  to write tags ourselves for both extensions.
- Do not throw `exception_tagging_unsupported` from `open()`. Use the tag-writer
  validator after implementation.

### 3.2 Matroska tag editing algorithm (verified against the Matroska ordering
guidelines)

From <https://raw.githubusercontent.com/ietf-wg-cellar/matroska-specification/19bbeea1f203917278e6b792f0526c5e18735742/order_guidelines.md>:

> The `Tags` Element is the one that is most subject to changes after the
> file was originally created. ... When editing the `Tags` Element(s), the
> original `Tags` Element at the beginning can be voided and a new one
> written right at the end of the `Segment` Element. The file size will only
> marginally change.

Recommended Sena layout (needs to be reflected in the container spec):

1. Immutable tags `SENA_PROFILE`, `SENA_VERSION`, `SENA_PLAYABLE_SAMPLES`
   stay in the **first** top-level `Tags` element **before the first
   Cluster** so probing/streaming does not need the file tail.
2. User-editable metadata goes in **separate** top-level `Tags` element(s)
   **at the end of the Segment**, after the last Cluster and Cues.
3. A retag operation:
   - overwrites each old user `Tags` element with an EBML `Void` element of
     exactly the same total encoded length;
   - appends the new user `Tags` element at the Segment tail;
   - patches the Segment size (our muxer writes a known-size Segment) and any
     `SeekHead` references to user `Tags`;
   - never moves Clusters or Cues.
4. Multi-value `file_info::meta_*` entries map to repeated Matroska
   `SimpleTag` elements (UTF-8 name/value). ReplayGain and other `file_info`
   info fields need an explicit mapping table before P0 (keep it in the shared
   Rust tag writer, not the C++ shim).

Current `crates/sena-mux/src/mka.rs` writes all tags (today only the immutable
Sena tags) in one early `Tags` element, which is compliant with the proposed
layout. Do **not** change that early element when user tags are added; append
new elements at the tail.

### 3.3 ABI consequence

The decoder C ABI currently has no tag-write function. Before the plugin is
coded, decide and spec either:

- a shared `sena_file_write_tags` / `sena_file_remove_tags` ABI in the Rust
  core (recommended, because EBML void/append/segment-size patching must not be
  duplicated per host), or
- a small C++ EBML tag editor in the foobar shim (discouraged).

The foobar-side API surface is fixed regardless: `retag()` +
`remove_tags()`.

---

## 4. Requirement 2: leading/trailing padding

Keep the trims **inside `sena-dec`**, not in the foobar shim:

- `sena_dec_read_f32()` returns only playable frames (already truncated by
  `SENA_PLAYABLE_SAMPLES`). The shim never sees warmup or trailing padding.
- `get_info()` reports `length = SenaDecInfo::playable_frames / 48000.0`;
  `channels = 2`, `samplerate = 48000`.
- `decode_run()` is a thin loop: fill a float buffer, call
  `sena_dec_read_f32()`, `return false` on 0 frames, otherwise
  `audio_chunk::set_data_32(buf, got, 2, 48000)` (with
  `audio_chunk::channel_config_stereo`).
- `decode_seek(seconds)`: `audio_math::time_to_samples(seconds, 48000)`,
  clamp to `[0, playable_frames)`, then `sena_dec_seek(dec, frame)`.
- `decode_can_seek()` is true only when the `SenaDecIo` has a non-NULL
  `seek` callback and the underlying file is seekable.

Compare `foo_input_caf::decode_run()`: it does per-chunk `m_start_skip` and
`trim` math in the plugin. That is exactly the duplication we avoid by putting
the same logic in the Rust core. The CAF code is still the best reference for
what the plugin would have to do if it owned trim.

Gapless playback: two consecutive Sena files are independent decoder handles;
the C ABI has no global mutable state, so no cross-file state can leak.

---

## 5. Requirement 3: real-time dynamic bitrate (fix the `.tak` failure mode)

### 5.1 foobar mechanism

- After each `run()`, foobar may call
  `input_decoder::get_dynamic_info(file_info&, double&)`.
- `helpers/dynamic_bitrate_helper` accumulates `on_frame(duration_seconds,
  payload_bits)` per decoded block and, on the configured update interval
  (default ~9 updates/sec), computes
  `kbps = (accumulated_bits / accumulated_time + 500) / 1000` and writes
  `file_info::info_set_bitrate_vbr(val)`.
- The title-format field `%bitrate%` displays this dynamic value for the
  currently playing VBR track. See the title-formatting reference:
  <https://wiki.hydrogenaudio.org/index.php?title=Foobar2000:Title_Formatting_Reference>.
- `foo_input_caf` is the cleanest open-source example
  (`input_caf.cpp::update_dynamic_vbr_info` + `decode_get_dynamic_info`).
  `foo_input_hca` is the anti-example: `decode_get_dynamic_info()` always
  returns `false`, so the UI can only show the static average from
  `get_info()`. A TAK-style "average only" display means the input either
  returns `false` or never calls the helper.

### 5.2 What the shim needs from the core

Do **not** compute bitrate from the `SenaDecIo::read` callback byte counts:
`read_f32` may buffer/read ahead, so bytes read in one call do not correspond
to the frames returned in that call (first block would spike, later blocks
would read zero).

The core must report **payload bits attributable to the playable frames
returned by each `read_f32`**:

- `payload_bits` = sum of Opus packet payload bits + xHE-AAC AU payload bits
  whose presentation interval maps to the returned playable frame range,
  after clipping the leading/trailing padding intervals. Packets spanning
  block boundaries are attributed proportionally to overlapping frames.
- Warmup packets and trailing padding do not inflate the displayed bitrate.
- After `sena_dec_seek()`, accounting starts from the seek target; no
  cumulative cross-seek contamination.

Recommended ABI shape to add to the decoder spec before coding:

```c
typedef struct {
    uint64_t start_frame;    /* first playable frame of the last returned block */
    uint64_t frames;         /* frames returned by that read_f32 */
    uint64_t payload_bits;   /* payload bits attributed to [start_frame, start_frame+frames) */
} SenaDecReadInfo;

int sena_dec_get_read_info(SenaDec *dec, SenaDecReadInfo *info);
```

Then the shim does, after every successful `read_f32`:

```cpp
m_bitrate_helper.on_frame(info.frames / 48000.0, info.payload_bits);

bool decode_get_dynamic_info(file_info & out, double & ts_delta) {
    return m_bitrate_helper.on_update(out, ts_delta);
}
```

`get_info()` still sets the static average bitrate
(`file_size * 8 / length_seconds / 1000`) for playlist display.

---

## 6. Requirement 4: Windows + macOS x86_64 + arm64

### 6.1 Official packaging model

From the SDK development overview
(<https://wiki.hydrogenaud.io/index.php?title=Foobar2000:Development:Overview>):

- A component is a Windows DLL or a macOS bundle with extension `.component`.
- One `.fb2k-component` zip may contain architecture subdirectories:
  - root: legacy x86 payload (for a v2-only component, verify before release
    whether omitting root is accepted; otherwise ship the x64 DLL as root);
  - `x64\foo_input_sena.dll` — used by x64 and by ARM64EC foobar as fallback;
  - `arm64ec\foo_input_sena.dll` — preferred by ARM64EC foobar;
  - `mac\foo_input_sena.component` — macOS bundle.
- foobar2000 for ARM is an **ARM64EC** binary and can transparently load x64
  components; ARM64EC components are preferred for performance.
  (<https://wiki.hydrogenaud.io/index.php?title=Foobar2000:Foobar2000_for_ARM>)

### 6.2 Windows

| Piece | x86_64 | arm64 |
|---|---|---|
| foobar host ABI | x64 DLL | ARM64EC DLL (not native ARM64) |
| C++ shim | MSVC x64 (`foo_sample` uses v142 for x64) | MSVC ARM64EC: `/arm64EC`, link `/MACHINE:ARM64EC`, SDK provides `shared-ARM64EC.lib` |
| Rust core | `x86_64-pc-windows-msvc` staticlib | **`arm64ec-pc-windows-msvc`** staticlib |
| SDK project | `Release|x64` | `Release|ARM64EC` |

Evidence:

- SDK `foo_sample/foo_sample.vcxproj` contains `Win32`, `x64`, `ARM64`,
  `ARM64EC` configurations, and links `../shared/shared-$(Platform).lib`;
  `shared-ARM64EC.lib` ships in the SDK.
- Rust's `arm64ec-pc-windows-msvc` target is **tier 2**, distributed through
  `rustup`, and requires LLVM 18+, VS2022 with the ARM64/ARM64EC build tools,
  and the Windows 11 SDK:
  <https://doc.rust-lang.org/rustc/platform-support/arm64ec-pc-windows-msvc.html>.
  Note the doc explicitly says ARM64EC processes interact with the OS as
  x86_64 and **cannot load native AArch64 DLLs** — so do not accidentally
  build `aarch64-pc-windows-msvc` for the plugin.
- Plan A: link the Rust staticlib into the C++ DLL. Plan B if linker/CRT
  friction appears: build a Rust `cdylib` (`sena_dec_core.dll`) and ship it
  next to the component. Resolve this with a spike before the real build
  matrix, not during release week.

### 6.3 macOS

- Current requirement: macOS 11 (Big Sur)+, Intel or Apple Silicon
  (<https://www.foobar2000.org/mac>). SDK 2025-03-07 includes Xcode 12+
  projects; SDK 2024-08-07 raised the minimum to Big Sur.
- The official `foo_sample.xcodeproj` compiles the same
  `input_raw.cpp`/`main.cpp` on macOS and produces a `.component` bundle;
  `MACOSX_DEPLOYMENT_TARGET = 11.0`, `WRAPPER_EXTENSION = component`,
  `SDKROOT = macosx`. Debug has `ONLY_ACTIVE_ARCH = YES`; Release has no
  such override, so the standard Xcode release build is universal
  (`ARCHS_STANDARD` = `arm64 x86_64`). For CI, set
  `ARCHS="arm64 x86_64"` and `ONLY_ACTIVE_ARCH=NO` explicitly.
- Link the five SDK static libraries: `libfoobar2000_SDK.a`,
  `libfoobar2000_SDK_helpers.a`, `libfoobar2000_component_client.a`,
  `libshared.a`, `libpfc-Mac.a`, plus `Cocoa.framework`.
  Build them per-arch and `lipo` them if `xcodebuild` does not emit
  universal archives.
- Every macOS component must define `FOOBAR2000_MAC_CLASS_SUFFIX` in its own
  `foobar2000-mac-class-suffix.h` (SDK `helpers-mac/foobar2000-mac-helpers.h`;
  sample: `foo_sample/foobar2000-mac-class-suffix.h`).
- Rust core: build both `x86_64-apple-darwin` and `aarch64-apple-darwin`
  staticlibs, `lipo -create` into one universal `.a`, link it into the bundle.
  A pure-Rust sena-dec core should have minimal extra linker requirements;
  verify frameworks during the spike.
- `foo_tun_midi` is the best single-repo reference for the macOS flow
  (`Scripts/bootstrap_sdk.sh`, `Scripts/generate_xcode_project.rb`,
  `Scripts/build.sh`), but it is intentionally arm64-only; do not copy that
  limitation. `fb2k-components-mac-suite` is useful community material, not
  normative.

### 6.4 Component skeleton

```cpp
class input_sena : public input_stubs {
public:
    void open(service_ptr_t<file> hint, const char * path,
              t_input_open_reason reason, abort_callback & ab) {
        m_file = hint;
        input_open_file_helper(m_file, path, reason, ab);
        if (reason == input_open_info_write) {
            // Open/keep file r/w for the shared tag ABI.
            return;
        }
        // sena_dec_open() with an IO adapter over m_file + ab.
        // On non-Sena .mka: throw exception_io_unsupported_format so the
        // built-in Matroska input gets its chance (SDK g_open_from_list).
    }

    void get_info(file_info & info, abort_callback & ab) {
        info.set_length(m_info.playable_frames / 48000.0);
        info.info_set_int("samplerate", 48000);
        info.info_set_int("channels", 2);
        info.info_set("encoding", "lossy");
        info.info_set_bitrate(/* static average */);
        // User metadata read through the shared tag ABI.
    }

    bool decode_run(audio_chunk & chunk, abort_callback & ab) {
        uint64_t got = 0;
        if (sena_dec_read_f32(m_dec, m_buf.data(), m_buf.size(), &got) != 0)
            throw exception_io_data();   // map error/abort precisely
        if (got == 0) return false;
        chunk.set_data_32(m_buf.data(), got, 2, 48000);
        SenaDecReadInfo ri{};
        sena_dec_get_read_info(m_dec, &ri);
        m_bitrate.on_frame(ri.frames / 48000.0, (t_size)ri.payload_bits);
        return true;
    }

    bool decode_get_dynamic_info(file_info & out, double & ts_delta) {
        return m_bitrate.on_update(out, ts_delta);
    }

    void retag(const file_info & info, abort_callback & ab) { /* shared tag ABI */ }
    void remove_tags(abort_callback & ab) { /* shared tag ABI */ }

    static bool g_is_our_path(const char *, const char * ext) {
        return stricmp_utf8(ext, "sena") == 0 || stricmp_utf8(ext, "mka") == 0;
    }
    // g_is_our_content_type: keep false or define an audio/sena MIME; do not
    // blindly claim audio/x-matroska because built-in input also claims it.
};

static input_singletrack_factory_t<input_sena> g_input_sena;
DECLARE_FILE_TYPE("Sena files", "*.SENA;*.MKA");
```

Component registration files are platform-shared in the official sample:
`DECLARE_COMPONENT_VERSION`, `VALIDATE_COMPONENT_FILENAME`, and the input
class compile unchanged on both platforms; only UI/resource files differ.

---

## 7. Before coding: checklist / open decisions

- [x] Decoder spec now includes `SenaDecReadInfo` / `sena_dec_get_read_info`
      for dynamic bitrate.
- [x] Decoder spec now includes `sena_file_write_tags` /
      `sena_file_remove_tags` (shared tag ABI).
- [ ] Define the ReplayGain/info-field mapping table for the tag ABI before
      P0 coding. (Basic `file_info::meta_*` fields are wired; ReplayGain
      info fields remain a follow-up.)
- [x] Container spec now requires immutable Sena tags early and user tags at
      the Segment tail, with the void+append edit algorithm.
- [x] Build a linking spike for one Windows target and one macOS arch
      (Rust staticlib vs cdylib; CRT/framework dependencies) before the full
      matrix. Resolved: Rust staticlib only; Windows links via MSVC
      link.exe/MSBuild, macOS via clang + ld64.lld + MacOSX11.3.sdk.
- [ ] Run SDK `foo_input_validator` decoder + tag writer + fuzzer on both
      platforms. (Requires an actual foobar2000/macOS host; binaries are
      built and structurally verified.)
- [ ] Test `.mka` priority behavior both with our input above and below the
      built-in Matroska input; `.sena` must always work.
- [ ] Verify `%bitrate%` moves on a file with known bitrate variation and
      remains sane across seek, pause/resume, and `input_flag_no_seeking`.
- [x] Package exactly: `x64/`, `arm64ec/`, `mac/`; built
      `dist/foo_input_sena-0.1.0.fb2k-component` and verified archive layout.

## 8. Primary references

- foobar2000 SDK download & changelog: <https://www.foobar2000.org/SDK>,
  <https://www.foobar2000.org/changelog-sdk>
- Development overview / packaging: <https://wiki.hydrogenaud.io/index.php?title=Foobar2000:Development:Overview>
- foobar2000 for ARM (ARM64EC): <https://wiki.hydrogenaud.io/index.php?title=Foobar2000:Foobar2000_for_ARM>
- Title formatting `%bitrate%`: <https://wiki.hydrogenaudio.org/index.php?title=Foobar2000:Title_Formatting_Reference>
- Matroska ordering guidelines: <https://raw.githubusercontent.com/ietf-wg-cellar/matroska-specification/19bbeea1f203917278e6b792f0526c5e18735742/order_guidelines.md>
- Rust ARM64EC target: <https://doc.rust-lang.org/rustc/platform-support/arm64ec-pc-windows-msvc.html>
- foobar2000 macOS requirements: <https://www.foobar2000.org/mac>
- `foo_input_caf`: <https://github.com/nu774/foo_input_caf>
- `vgmstream` foobar plugin: <https://github.com/vgmstream/vgmstream/tree/master/fb2k>
- `foo_tun_midi` (macOS input component): <https://github.com/kartun83/foo_tun_midi>

## 9. Tag/info latency diagnosis (2026-08-31)

Symptom: adding a .sena to the playlist (info read) and writing tags felt
slower than .ogg/.m4a/.mp3.

Root cause found:
1. `input_sena::open()` called `sena_dec_open()` for `input_open_info_read`
   as well as decode. `sena_dec_open()` reads the whole file AND fully decodes
   both codec tracks (release measurement on `01__p300`: ~0.47 s for a 20 s
   clip, debug build > 11 s), while other formats only parse headers.
2. `sena_file_write_tags()` used to read the whole file and write the whole
   file back even though only the Segment-size field and tail Tags changed.

Fixes:
- New C ABI `sena_dec_probe_info()`: parses/validates the container only
  (profile/version/playable length/track layout/ASC/OpusHead), no codec
  decode. `senadec --info` also uses this path now (measured ~0.00 s).
- `input_sena::open()` only opens the decoder for `input_open_decode`;
  `get_info()` caches the probe result.
- Tag write now computes the byte ranges that actually changed (Segment size
  patch, voided old user Tags, appended tail Tags) and writes only those
  ranges with seek+write, instead of rewriting the entire file.

Remaining: actual foobar2000 host-side A/B timing after reinstalling the
rebuilt x64 DLL.
