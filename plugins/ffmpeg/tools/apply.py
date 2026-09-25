#!/usr/bin/env python3
"""Install the Sena demuxer into an FFmpeg source tree (idempotent).

usage: apply.py <ffmpeg-src> [--check | --uninstall]

What it does:
  1. copies the Sena plugin sources into <ffmpeg-src>/libavformat/
     (senadec.c, sena_dec_dl.c, sena_dec_dl.h, sena_dec.h, sena_probe.h)
  2. registers ff_sena_demuxer in libavformat/allformats.c (sorted position)
  3. adds the objects to libavformat/Makefile (CONFIG_SENA_DEMUXER)
  4. patches matroskadec's probe to defer Sena files to the sena demuxer
     (a .sena extension or a visible SENA_PROFILE tag => score 0 there)

configure picks the new demuxer up automatically (it scans allformats.c);
re-run ./configure with the previous arguments after applying, then make.
With --uninstall every change is reverted and the copied files removed.
"""
import argparse
import pathlib
import re
import shutil
import sys

PLUGIN_DIR = pathlib.Path(__file__).resolve().parent.parent
SRC_DIR = PLUGIN_DIR / "libavformat"
COPIED_FILES = ["senadec.c", "sena_dec_dl.c", "sena_dec_dl.h", "sena_dec.h", "sena_probe.h"]

ALLFORMATS_LINE = "extern const FFInputFormat  ff_sena_demuxer;"
MAKEFILE_LINE = "OBJS-$(CONFIG_SENA_DEMUXER)              += senadec.o sena_dec_dl.o"

MKA_INCLUDE_ANCHOR = '#include "matroska.h"'
MKA_INCLUDE_LINE = '#include "sena_probe.h"'

# Exact upstream snippet in matroska_probe() that claims a file as
# Matroska/WebM. FFmpeg 6.x-8.x all carry it verbatim; if a future FFmpeg
# reformats it, applying fails loudly instead of patching the wrong spot.
MKA_PROBE_OLD = """            if (!memcmp(p->buf + n, matroska_doctypes[i], probelen))
                return AVPROBE_SCORE_MAX;"""
MKA_PROBE_NEW = """            if (!memcmp(p->buf + n, matroska_doctypes[i], probelen)) {
#if CONFIG_SENA_DEMUXER
                /* A Sena file is Matroska carrying the mandatory SENA_PROFILE
                 * tag; leave it to the sena demuxer, and never claim a .sena
                 * file as plain Matroska. */
                if (av_match_ext(p->filename, "sena") ||
                    ff_sena_probe_match(p->buf, p->buf_size))
                    return 0;
#endif
                return AVPROBE_SCORE_MAX;
            }"""


def fail(msg):
    print(f"apply: error: {msg}", file=sys.stderr)
    sys.exit(1)


def insert_sorted(lines, new_line, key):
    """Insert new_line before the first entry whose sort key is larger."""
    for i, line in enumerate(lines):
        if key(line) > key(new_line):
            lines.insert(i, new_line + "\n")
            return
    lines.append(new_line + "\n")


def patch_allformats(path, uninstall):
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    present = any(ALLFORMATS_LINE in l for l in lines)
    if uninstall:
        if present:
            lines = [l for l in lines if ALLFORMATS_LINE not in l]
            path.write_text("".join(lines))
            return "removed ff_sena_demuxer registration"
        return "not registered (skipped)"
    if present:
        return "already registered (skipped)"
    demuxer_re = re.compile(r"^extern const FFInputFormat  ff_(\w+)_demuxer;\s*$")
    for i, line in enumerate(lines):
        m = demuxer_re.match(line)
        if m and m.group(1) > "sena":
            lines.insert(i, ALLFORMATS_LINE + "\n")
            path.write_text("".join(lines))
            return f"registered before ff_{m.group(1)}_demuxer"
    fail(f"no demuxer registration point found in {path}")


def patch_makefile(path, uninstall):
    text = path.read_text()
    lines = text.splitlines(keepends=True)
    present = any("CONFIG_SENA_DEMUXER" in l for l in lines)
    if uninstall:
        if present:
            lines = [l for l in lines if "CONFIG_SENA_DEMUXER" not in l]
            path.write_text("".join(lines))
            return "removed CONFIG_SENA_DEMUXER objects"
        return "not in Makefile (skipped)"
    if present:
        return "already in Makefile (skipped)"
    obj_re = re.compile(r"^OBJS-\$\(CONFIG_(\w+)_DEMUXER\)")
    for i, line in enumerate(lines):
        m = obj_re.match(line)
        if m and m.group(1) > "SENA":
            lines.insert(i, MAKEFILE_LINE + "\n")
            path.write_text("".join(lines))
            return f"added objects before CONFIG_{m.group(1)}_DEMUXER"
    fail(f"no demuxer object list found in {path}")


def patch_matroskadec(path, uninstall):
    text = path.read_text()
    if uninstall:
        changed = text.replace(MKA_PROBE_NEW, MKA_PROBE_OLD)
        changed = changed.replace(MKA_INCLUDE_LINE + "\n", "")
        if changed == text:
            return "matroskadec.c untouched (skipped)"
        path.write_text(changed)
        return "reverted matroskadec.c"
    if MKA_PROBE_NEW in text:
        return "matroskadec.c already patched (skipped)"
    if MKA_PROBE_OLD not in text:
        fail(f"matroska_probe snippet not found in {path}; this FFmpeg version "
             f"changed the probe - adjust MKA_PROBE_OLD in {__file__}")
    if MKA_INCLUDE_ANCHOR not in text:
        fail(f"{MKA_INCLUDE_ANCHOR} not found in {path}")
    text = text.replace(MKA_PROBE_OLD, MKA_PROBE_NEW, 1)
    text = text.replace(MKA_INCLUDE_ANCHOR, MKA_INCLUDE_ANCHOR + "\n" + MKA_INCLUDE_LINE, 1)
    path.write_text(text)
    return "patched matroskadec.c (defer Sena files)"


def check(tree):
    ok = True
    for name in COPIED_FILES:
        if not (tree / "libavformat" / name).exists():
            print(f"missing: libavformat/{name}")
            ok = False
    if ALLFORMATS_LINE not in (tree / "libavformat" / "allformats.c").read_text():
        print("missing: allformats.c registration")
        ok = False
    if "CONFIG_SENA_DEMUXER" not in (tree / "libavformat" / "Makefile").read_text():
        print("missing: Makefile objects")
        ok = False
    if MKA_PROBE_NEW not in (tree / "libavformat" / "matroskadec.c").read_text():
        print("missing: matroskadec.c deferral patch")
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
    if not (tree / "libavformat" / "allformats.c").exists():
        fail(f"{tree} does not look like an FFmpeg source tree")

    if args.check:
        sys.exit(0 if check(tree) else 1)

    avformat = tree / "libavformat"
    if args.uninstall:
        for name in COPIED_FILES:
            target = avformat / name
            if target.exists():
                target.unlink()
                print(f"removed libavformat/{name}")
    else:
        for name in COPIED_FILES:
            src = SRC_DIR / name
            if not src.exists():
                fail(f"missing plugin source {src}")
            target = avformat / name
            if target.exists() and target.read_bytes() == src.read_bytes():
                print(f"libavformat/{name} up to date")
                continue
            shutil.copyfile(src, avformat / name)
            print(f"installed libavformat/{name}")

    print("allformats.c:", patch_allformats(avformat / "allformats.c", args.uninstall))
    print("Makefile:    ", patch_makefile(avformat / "Makefile", args.uninstall))
    print("matroskadec.c:", patch_matroskadec(avformat / "matroskadec.c", args.uninstall))
    if not args.uninstall:
        print("done. Re-run ./configure in the FFmpeg tree, then make.")


if __name__ == "__main__":
    main()
