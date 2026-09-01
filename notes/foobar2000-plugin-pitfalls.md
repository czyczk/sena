# foobar2000 plugin development pitfalls (foo_input_sena experience log)

Reference for building the next Sena plugins (and future host shims). Each
entry: symptom -> root cause -> fix. Companion to
`notes/foobar2000-plugin.md` (architecture research) and
`notes/album-art.md` (pictures).

## SDK / API traps

### Attached pictures are NOT `input_info_writer` meta
- Symptom: "Attached picture editing is not supported for this file type";
  converter: "An error occurred while transferring attached pictures
  (unsupported file format)".
- Cause: foobar2000 handles embedded pictures through the separate
  `album_art_extractor` / `album_art_editor` service pair (registered via
  `service_factory_single_t`, entrypoints `is_our_path`/`open`/`get_guid`).
  The tag-writer `PICTURE` meta path is not used for pictures at all.
- Fix: implement both services; map foobar art GUIDs
  (`album_art_ids::cover_front` ...) to your container's picture location.
  See `notes/album-art.md`; for Matroska use Attachments, never string Tags
  (binary payloads break `CStr` reads at embedded NULs).

### ReplayGain lives in the info section, not in meta
- Symptom: RG apply silently wrote 0 entries / empty values / wasn't shown.
- Cause: `file_info::meta_enumerate()` does NOT include ReplayGain. RG is
  in `replaygain_info` (`get_replaygain()` / `set_replaygain()`); writers
  must enumerate it with `replaygain_info::for_each(...)` and readers must
  `set_replaygain()` after parsing meta (use
  `g_is_meta_replaygain`/`set_from_meta`, case-insensitive - foobar emits
  lowercase `replaygain_track_gain`, transferred tags are usually uppercase).

### `replaygain_info::for_each` reuses one transient text buffer
- Symptom: stored values were garbage (`\x15;\x15`), or last-value-wins.
- Cause: the callback's `const char* value` points at a single stack
  `t_text_buffer` reused for all four calls; it is invalid after the
  callback returns.
- Fix: copy key/value into stable storage (e.g. `pfc::string8`) INSIDE the
  callback before building your entry list.

### RG duplication on re-apply (meta + info both present)
- Symptom: metadata tab shows `-11.37 dB; -11.37 dB`.
- Cause: after a scan, the file_info carries the old RG as meta (read back
  from the file) AND the fresh scan in the info section; writing both
  duplicates the entry.
- Fix: dedupe on write; the info section (fresh scan) is authoritative -
  keep meta-form RG only for keys the info section lacks. Pure tag
  transfers (meta-only) keep working. One re-apply heals old files.

### `console::formatter()` has no `operator<<(const char*)`
- Symptom: clang (macOS slice) rejects the line; MSVC may accept it, so
  the break only shows on one platform.
- Fix: `console::print(pfc::string8(...))` built via a named
  `pfc::string8` lvalue (`pfc::string8 msg; msg << ...;`) - `pfc`
  `operator<<` requires an lvalue first argument.

### Misc API details
- `pfc::list_t` removal by index is `remove_by_idx(i)`, not `remove_item`
  (that takes the item).
- Service instantiation: `new service_impl_t<T>()` (no `fb2k::new_service`
  in this SDK vintage).
- `input_open_file_helper` reason matters: `input_open_info_read` for
  extractors, `input_open_info_write` for editors.
- foobar's converter decides what RG it transfers; with RG processing it
  may carry gain WITHOUT the original peak (peak is a source-PCM property).
  Only a fresh scan produces a peak - that is foobar semantics, not the
  plugin dropping it.

## Codec / audio traps

### Progress bar races to ~99% then stalls
- Cause: foobar's converter progress is input-consumption driven (pipe
  backpressure). A slurp-all-then-encode pipeline consumes the input
  instantly, so the bar maxes out before the real work starts.
- Fix: stream the pipeline (consume = process chunk-by-chunk, write the
  codec inputs as data arrives). The bar then tracks real work; only the
  unavoidable codec tail remains.

### xHE-AAC random access
- The USAC stream is NOT frame-independent in general. Random access
  points are marked by `usacIndependencyFlag` = bit 7 of the first AU
  payload byte (~every 15 AUs from exhale). A fresh decoder at such an AU
  is bit-exact for some content, and merely converges for other content
  (SBR/stateful tools). Opus packets decode independently but a fresh
  decoder's output converges over ~7 packets (state), so mid-stream seeks
  are fast but not bit-exact - acceptable and standard.
- Seek bookkeeping: make the zero-phase resampler keep pre-target context
  (trim AFTER resampling), round each chunk's take to a representable
  rational-ratio count, and keep per-chunk bit accounting seek-relative.

### Length over-count / trailing noise (WAV streaming readers)
- Symptom: file reports/plays longer than the source with a noise tail.
- Cause: streaming WAV readers that decode until EOF count trailer bytes
  (LIST/metadata/padding after the data chunk) as samples.
- Fix: honor the declared data-chunk size when sane (0 / 0xFFFFFFFF =
  stream to EOF); ignore bytes after the declared boundary. Keep the CLI
  batch reader and the streaming reader consistent.

### Audio content hash (SENA_AUDIO_SHA256)
- Hash ONLY the canonical audio (normalized 48 kHz stereo interleaved
  f32 LE - the playable timeline); never container layout, timestamps,
  tags, attachments/FileUID, or codec payloads. Preserve it across tag /
  attachment rewrites (tests enforce byte equality when present).
- Cross-library known-vector tests: avoid transcendental functions
  (libm sin differs by 1 ULP across implementations -> different hashes).
  Use exactly-representable signals (linear ramps).

## Build / cross-compile traps

### `can't find crate for core` - missing rust target std libs
- Cause: `rustc --print target-libdir` prints a path even when the std
  libs are missing; checking the return code is not enough.
- Fix: check the directory EXISTS; print the exact
  `rustup target add i686-pc-windows-msvc x86_64-pc-windows-msvc
  arm64ec-pc-windows-msvc aarch64-apple-darwin x86_64-apple-darwin`
  command in the failure and in `doctor`. cargo-xwin supplies only the
  Windows CRT, not the rust std.

### Per-arch build failures must not abort the rest
- Wrap each arch (rust lib + SDK + MSBuild) and each mac slice in its own
  try/except; report `SKIPPED` with the error and keep going; package
  whatever succeeded. Only fail when nothing built.

### MSBuild C1041 "cannot open vc143.pdb"
- Cause: a stale/zombie cl.exe from an earlier aborted run holds the PDB
  (the obj dir under %TEMP% is reused). Also concurrent CL can contend.
- Fix: kill the zombie (`taskkill /PID <pid> /F`), pass
  `/p:MultiProcessorCompilation=false` to serialize CL.

### justfile naming
- Namespace recipes per artifact (`senadec-plugin-fb2k-*`,
  `senadec-bin-*`, `senaenc-*`); keep generic `doctor`/`check`/`test`.
  Per-arch recipes + one aggregate + `package`; legacy aliases were
  removed by request (do not reintroduce).

## CLI hygiene
- Both CLIs must refuse to overwrite existing outputs (`--force` to allow;
  interactive `[y/N]` prompt only when stdin is a terminal AND the audio
  input is not stdin, otherwise hard error).
- `doctor`-style subcommands: check required external tools without doing
  real work (fatal vs warning severity + impact text), locate tools next
  to the executable FIRST including `.exe`/PATHEXT variants (a bare name
  never resolves on Windows - the classic "exhale.exe is right there"
  bug).

## ABI growth
- Appending fields to a `#[repr(C)]` info struct is fine only when the
  plugin and the Rust lib ship together (they do, in one package). Keep
  probe and decode info paths filled consistently, and default-construct
  new fields in tests.
