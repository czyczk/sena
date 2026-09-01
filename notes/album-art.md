# Album art (attached pictures) in Sena / foobar2000 - design and research summary

## How foobar2000 exposes attached pictures to tag writers

Attached picture editing is NOT part of `input_info_writer` in any
meaningful way. foobar2000 uses a separate service pair:

- `album_art_extractor` / `album_art_extractor_v2` - READ pictures and
  thumbnail/icon data (album_art_manager asks registered extractors).
- `album_art_editor` / `album_art_editor_v2` - EDIT pictures
  (`set(GUID, album_art_data_ptr, abort)`, `remove(GUID)`,
  `commit(abort)`, and `album_art_editor_instance_v2::remove_all()`).

The errors seen in foobar come straight from this:

- "Attached picture editing is not supported for this file type" =
  `exception_album_art_unsupported_format`: NO registered
  `album_art_editor` matched the file, so the Properties dialog's
  attached-picture editor refuses.
- "An error occurred while transferring attached pictures (unsupported
  file format)" during conversion: the converter's metadata transfer
  (album art) found no editor/extractor for the produced `.sena`.

The `input_info_writer` PICTURE-meta path does not carry pictures in
modern foobar2000; writing binary data into Matroska string Tags was
wrong (we now filter those entries, so tag writes stay valid).

## Where pictures live in Matroska

Pictures are **top-level Segment `Attachments`** elements
(`0x19 0x41 0xA4 0x69`) containing one or more `AttachedFile`
(`0x61 0xA7`; children: FileName `0x45 0xE0`, FileMimeType `0x46 0x60`,
FileData `0x46 0x5C`, FileUID `0x46 0xAE`). They are not string Tags.

## Implementation

- `crates/sena-dec/src/attachments.rs`: parse and in-place rewrite
  (void existing Attachments element, append replacement at the Segment
  tail, re-patch the Segment size; clusters never move). Unit tests
  cover write/read/remove round-trips and cluster stability.
- `crates/sena-dec/src/ffi.rs`: C ABI `sena_file_art_read` /
  `sena_file_art_write` (+ `sena_art_*` accessors). Binary-safe (explicit
  length, no CStr).
- `plugins/foobar2000/foo_input_sena/album_art_sena.cpp`: registers the
  `album_art_extractor_v2` + `album_art_editor_v2` services for
  `.sena`/`.mka`; maps foobar GUIDs to attachment file names
  (`cover_front.jpg`, `cover_back.jpg`, `disc.jpg`, `icon.jpg`,
  `artist.jpg`, unknown -> `art_<name>.jpg`); MIME sniffed from magic
  bytes.
- The converter's picture transfer now succeeds via the editor; the tag
  writer keeps skipping PICTURE meta entries (they are binary, not
  string tags).

## Verification

Attachment round-trip test (Rust) passes; the plugin builds for
x86/x64/arm64ec + macOS universal and packages. On the user side:
convert a source with a cover, then check Properties -> Artwork shows
the embedded picture, and converter no longer reports the
attached-picture error.
