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
  `set_replaygain()` after parsing the stored tags (use
  `g_is_meta_replaygain`/`set_from_meta`, case-insensitive - foobar emits
  lowercase `replaygain_track_gain`, transferred tags are usually uppercase).
- READ side: NEVER `meta_add()` the REPLAYGAIN_* keys - other formats'
  readers expose RG exclusively through `set_replaygain()`, and the
  Properties Metadata tab shows raw meta entries; adding them as meta
  makes RG appear in BOTH tabs (wrong). The stored string tags stay the
  same; only the exposure differs.

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
- Hash the encoded elementary streams carried in the file, not the input
  PCM or decoded output: OpusHead + Opus packets for A_OPUS, ASC + raw AUs
  for A_SENALF, length-prefixed in track order (`encoded_audio_sha256`).
- Never include container layout, timestamps, tags, attachments/FileUID,
  or Ogg/M4A container bytes in the hash. Preserve the tag byte-for-byte
  across tag / attachment rewrites (tests enforce this).
- Different codec parameters MUST change the tag: if two .sena files made
  with different parameters still have equal SENA_AUDIO_SHA256, the hash is
  hashing the wrong thing (this was the original PCM-hash bug).
- Known-vector test uses synthetic byte streams; no transcendental
  functions are involved.

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

## File association / "Open With"

- Windows: the component declares its extensions via
  `DECLARE_FILE_TYPE("Sena files", "*.SENA;*.MKA")`. foobar2000 itself
  handles shell registration: Preferences -> Shell Integration lists the
  component's file types and applying them pops the UAC prompt the user
  saw. Nothing else is needed from the component; if the OS still asks
  for a program, apply the association once in foobar's Shell
  Integration page.
- macOS: `Open With` / Recommended Applications come from
  CFBundleDocumentTypes. Add them (plus a UTExportedTypeDeclarations UTI
  for the private extension) to the component bundle's Info.plist;
  LaunchServices picks them up from bundles nested in the host app. The
  app itself may also need its association applied once from foobar's
  settings.

## ABI growth
- Appending fields to a `#[repr(C)]` info struct is fine only when the
  plugin and the Rust lib ship together (they do, in one package). Keep
  probe and decode info paths filled consistently, and default-construct
  new fields in tests.

## 32-bit hosts (foobar2000 x86): every FFI path must be memory-bounded
- Symptom: playback fine, single-file RG scan fine, but a batch scan of 6+
  tracks (one decode thread per track) probabilistically kills the whole
  player with NOTHING in the console.
- Cause: 32-bit process = 2 GiB address space. The FFI used to
  `read_all()` the whole file and `Demuxed::parse()` copied every frame
  again (>= 2x file size per decoder instance; Rust OOM is a silent
  `abort()`, and the `Vec` growth transient needs old+new buffers). N
  concurrent scan threads multiply it. Same slurp existed in probe
  (`get_info`), album-art read, and the tag/art rewrite paths.
- Fix: `demux::index_container` scans the file through positioned reads
  with a sliding window - head elements parsed, audio frames enter as a
  compact index (`FrameRef`: offset/len/first byte, ~32 B per frame), no
  payload copies. `StreamingDecoder` fetches frame payloads lazily via
  `FrameStore::Lazy` (256 KiB read-ahead window, frames are consumed in
  file order). Tag/attachment rewrites are computed as a handful of
  positioned writes (`WriteOp`: void in place + append at Segment tail +
  size patch) instead of a rewritten whole-file copy. Non-seekable inputs
  fall back to the old in-memory path (pipe input cannot do random
  access); seekability is probed with a no-op `seek(0, SEEK_CUR)`.
- Sanity caps against hostile files: 16 MiB per frame/master payload,
  64 MiB per Attachments element, 2M indexed frames max.
- Also: every `extern "C"` entry point that touches decode state MUST be
  wrapped in `catch_unwind` - a panic escaping an `extern "C"` fn aborts
  the process with the message lost on stderr (invisible in a GUI host).
- Downstream note: no changes were needed in `ropus` / `rxaac-dec`; their
  decoders are per-instance and allocate only bounded per-frame buffers.
  Keep it that way (no global mutable state, no whole-file buffers).

### Leaks: the host never calls any "decode end" cleanup
- Symptom: memory climbs DURING playback of one track, climbs again per
  track switch; RG scan memory is never released afterwards. On 32-bit
  this is a guaranteed crash after enough tracks.
- Cause 1 (C++): foobar2000 destroys the input instance when done - there
  is no "stop/close" callback. `input_sena` held `SenaDec*` and only freed
  it on the next `open_decoder()`; every played/scanned track leaked the
  whole Rust decoder. Fix: `~input_sena()` calls `close_decoder()`. Any
  native handle member in an input class MUST be released from the dtor
  (cf. vgmstream's `~input_vgmstream` doing `libvgmstream_free`).
  `input_stubs` is non-polymorphic and the SDK wrapper stores the instance
  by value, so a plain dtor suffices (no `override`).
- Cause 2 (Rust): `StreamingDecoder::bits_prefix` appended one f64 per
  produced frame and only reset on seek - ~90 MB for a 4 min track, i.e.
  growth even within one file. Fix: rebase the prefix onto the read cursor
  after every `read_f32` (entries at/below the cursor are unreachable; a
  seek resets the prefix anyway). Test:
  `stream::tests::bits_prefix_stays_bounded_over_full_decode`.
- Diagnosis tip: "grows within one track" = per-frame/chunk accounting
  structure; "grows per track, never released" = missing dtor/close. The
  two are independent and both were present.

### macOS: instant crash at playback start, zero console output
- Symptom: any .sena plays -> the process dies at the first decode; no
  console lines at all. Properties/tag reads are fine. Windows (same
  build) totally fine.
- Crash report: `EXC_BAD_ACCESS (SIGBUS)`, `KERN_PROTECTION_FAILURE` at a
  Stack Guard page, on "Fb2k Playback Decoding Thread" - a **stack
  overflow**. macOS gives fb2k's decoding thread a 544 KiB stack (the
  report's VM region list shows it), Windows threads get 1 MiB.
- Root cause was DOWNSTREAM, in rxaac-dec (not sena): `UsacDecoder` had
  `mps_dec: Option<MpsDec>` with MpsDec = ~76 KiB of inline matrices, so
  the decoder struct was ~98 KiB and nested by-value construction
  (`UsacDecoder::new` -> `ChannelState::new` -> `FdChannelState::new` ->
  big-array `Default`) needed >640 KiB of stack. The MPS state is even
  `None` for our files - it overflows just from being an inline field.
- Diagnosis method worth reusing: `std::thread::Builder::new()
  .stack_size(N)` + binary search on N reproduces host thread budgets
  locally (an overflow aborts the process, so probe one size per process).
  Regression guard: `stream::tests::decode_fits_small_host_stack` (448 KiB).
- Fix (rxaac-dec, 2 lines): `mps_dec: Option<Box<MpsDec>>` + `Box::new` at
  the lazy init. UsacDecoder shrinks 98 KiB -> ~20 KiB; whole decode
  init+read fits in ~256 KiB of stack (>2x margin under 544 KiB).
- Lesson: constructors that return big structs by value are a stack
  hazard on small host threads (audio plugins, mobile). Box fields >~8 KiB
  at the struct level; don't rely on the optimizer to elide the copies.
  The pre-session "working" mac build was only marginally under the limit
  (old rxaac already needed ~640 KiB) - it worked by toolchain luck.
