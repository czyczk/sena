#!/usr/bin/env python3
"""Install the Sena decoder into an FFmpeg source tree (idempotent).

usage: apply.py <ffmpeg-src> [--check | --uninstall]

What it does:
  1. copies the Sena plugin sources into <ffmpeg-src>/libavcodec/
     (senadec.c, sena_dec_dl.c, sena_dec_dl.h, sena_dec.h)
  2. adds AV_CODEC_ID_SENA to libavcodec/codec_id.h (after APPLE_APAC)
  3. adds the codec descriptor to libavcodec/codec_desc.c
  4. registers ff_sena_decoder in libavcodec/allcodecs.c (sorted position)
  5. adds the objects to libavcodec/Makefile (CONFIG_SENA_DECODER)
  6. maps wFormatTag 0x534E to AV_CODEC_ID_SENA in libavformat/riff.c
     (ff_codec_wav_tags), which makes the Matroska `A_MS/ACM` path and WAV
     resolve the codec automatically - no demuxer patches needed

configure picks the new decoder up automatically (it scans allcodecs.c and
codec_id.h); re-run ./configure with the previous arguments after applying,
then make. With --uninstall every change is reverted and the copied files
removed.
"""
import argparse
import pathlib
import re
import shutil
import sys

PLUGIN_DIR = pathlib.Path(__file__).resolve().parent.parent
SRC_DIR = PLUGIN_DIR / "libavcodec"
COPIED_FILES = ["senadec.c", "sena_dec_dl.c", "sena_dec_dl.h", "sena_dec.h"]

CODEC_ID_ANCHOR = "    AV_CODEC_ID_APPLE_APAC,\n"
CODEC_ID_LINE = "    AV_CODEC_ID_SENA,\n"

CODEC_DESC_ANCHOR = """    {
        .id        = AV_CODEC_ID_APPLE_APAC,
        .type      = AVMEDIA_TYPE_AUDIO,
        .name      = "apple_apac",
        .long_name = NULL_IF_CONFIG_SMALL("Apple Positional Audio Codec"),
        .props     = AV_CODEC_PROP_LOSSY,
    },
"""
CODEC_DESC_LINE = """    {
        .id        = AV_CODEC_ID_SENA,
        .type      = AVMEDIA_TYPE_AUDIO,
        .name      = "sena",
        .long_name = NULL_IF_CONFIG_SMALL("Sena multilayer audio"),
        .props     = AV_CODEC_PROP_LOSSY,
    },
"""

ALLCODECS_LINE = "extern const FFCodec ff_sena_decoder;"
MAKEFILE_LINE = "OBJS-$(CONFIG_SENA_DECODER)              += senadec.o sena_dec_dl.o"

RIFF_ANCHOR = "    { AV_CODEC_ID_FLAC,            0xF1AC },\n"
RIFF_LINE = "    { AV_CODEC_ID_SENA,            0x534E },\n"


def fail(msg):
    print(f"apply: error: {msg}", file=sys.stderr)
    sys.exit(1)


def insert_after(text, anchor, line):
    if line in text:
        return text, False
    if anchor not in text:
        return None, False
    return text.replace(anchor, anchor + line, 1), True


def patch_file(path, anchor, line, uninstall, sort_regex=None):
    """Insert `line` after `anchor` (or sorted by sort_regex), idempotent."""
    text = path.read_text()
    if uninstall:
        if line.strip() in text:
            path.write_text(text.replace(line, "", 1))
            return "reverted"
        return "not present (skipped)"
    if line.strip() in text:
        return "already applied (skipped)"
    if anchor is not None:
        new, _ = insert_after(text, anchor, line)
        if new is None:
            fail(f"anchor not found in {path}:\n{anchor}")
        path.write_text(new)
        return "patched (after anchor)"
    if sort_regex is not None:
        key = re.compile(sort_regex)
        target = "SENA_DECODER"
        lines = text.splitlines(keepends=True)
        for i, l in enumerate(lines):
            m = key.match(l)
            if m and m.group(1) > target:
                lines.insert(i, line + "\n")
                path.write_text("".join(lines))
                return f"patched (before {m.group(1)})"
        fail(f"no insertion point found in {path}")
    fail("internal: no anchor and no sort regex")


def check(tree):
    ok = True
    for name in COPIED_FILES:
        if not (tree / "libavcodec" / name).exists():
            print(f"missing: libavcodec/{name}")
            ok = False
    for path, line, label in [
        (tree / "libavcodec" / "codec_id.h", CODEC_ID_LINE, "codec_id.h"),
        (tree / "libavcodec" / "codec_desc.c", 'AV_CODEC_ID_SENA', "codec_desc.c"),
        (tree / "libavcodec" / "allcodecs.c", ALLCODECS_LINE, "allcodecs.c"),
        (tree / "libavcodec" / "Makefile", "CONFIG_SENA_DECODER", "Makefile"),
        (tree / "libavformat" / "riff.c", "0x534E", "riff.c wav tag"),
    ]:
        if line not in path.read_text():
            print(f"missing: {label}")
            ok = False
    print("check: all patches present" if ok else "check: incomplete - run apply")
    return ok


def main():
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("ffmpeg_src", type=pathlib.Path, help="FFmpeg source tree")
    g = ap.add_mutually_exclusive_group()
    g.add_argument("--check", action="store_true", help="verify the patch state only")
    g.add_argument("--uninstall", action="store_true", help="revert everything")
    args = ap.parse_args()

    tree = args.ffmpeg_src.resolve()
    if not (tree / "libavcodec" / "allcodecs.c").exists():
        fail(f"{tree} does not look like an FFmpeg source tree")

    avcodec = tree / "libavcodec"
    avformat = tree / "libavformat"
    if args.uninstall:
        for name in COPIED_FILES:
            target = avcodec / name
            if target.exists():
                target.unlink()
                print(f"removed libavcodec/{name}")
    else:
        for name in COPIED_FILES:
            src = SRC_DIR / name
            if not src.exists():
                fail(f"missing plugin source {src}")
            target = avcodec / name
            if target.exists() and target.read_bytes() == src.read_bytes():
                print(f"libavcodec/{name} up to date")
                continue
            shutil.copyfile(src, avcodec / name)
            print(f"installed libavcodec/{name}")

    print("codec_id.h:  ", patch_file(avcodec / "codec_id.h", CODEC_ID_ANCHOR, CODEC_ID_LINE, args.uninstall))
    print("codec_desc.c:", patch_file(avcodec / "codec_desc.c", CODEC_DESC_ANCHOR, CODEC_DESC_LINE, args.uninstall))
    print("allcodecs.c: ", patch_file(avcodec / "allcodecs.c", None, ALLCODECS_LINE, args.uninstall,
                                      r"^extern const FFCodec\s+ff_(\w+)_decoder;\s*$"))
    print("Makefile:    ", patch_file(avcodec / "Makefile", None, MAKEFILE_LINE, args.uninstall,
                                      r"^OBJS-\$\(CONFIG_(\w+)_DECODER\)"))
    print("riff.c:      ", patch_file(avformat / "riff.c", RIFF_ANCHOR, RIFF_LINE, args.uninstall))
    if not args.uninstall:
        print("done. Re-run ./configure in the FFmpeg tree, then make.")


if __name__ == "__main__":
    main()
