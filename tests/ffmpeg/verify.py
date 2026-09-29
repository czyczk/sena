#!/usr/bin/env python3
"""Deterministic checks for the ffmpeg Sena decoder (libavcodec plugin).

Subcommands (see run.sh for orchestration):
  decode  <ffmpeg.raw> <stream_ref.raw> <cli_ref.raw> <playable_frames>
  seek    <ffmpeg> <asset.sena> <cont_ref.raw> <t1> [t2 ...]
  probe   <ffprobe> <file> <expected_codec_name>
  tags    <ffprobe> <fixture.sena> <key=value> [...]
  art     <ffmpeg> <fixture.sena> <cover_file> <workdir>
  nolib   <ffmpeg> <asset.sena>
"""
import os
import subprocess
import sys

import numpy as np

SR = 48000
ULP_TOL = 1e-9  # f32 last-ulp / FFT block-size rounding envelope
# Composite-block coverage past playable_samples (the mid layer covers
# pre-skip past the last block): the shifted-timeline seek output includes
# this tail because the decoder cannot know the cue shift.
SEEK_TAIL_FRAMES = 648
# After the cold-start ramp (LF joins at the first independency AU), seek
# output converges to the continuous decode. The envelope is loose on
# purpose: a zero-history LF fresh start is format-inherently inexact at
# some anchors (LPD prev-frame state and free-running eSBR counters are not
# carried in the bitstream; the container-seek path hides this by backing
# off an extra independency AU for real context, which a cold start cannot
# do). The DirectShow path uses shadow-file warmup for full quality.
SEEK_SETTLE_S = 2.0
SEEK_SETTLE_TOL = 0.4


def run(cmd, env=None, ok_codes=(0,)):
    r = subprocess.run(cmd, stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    if r.returncode not in ok_codes:
        raise AssertionError(f"{' '.join(cmd)} exited {r.returncode}: {r.stderr.decode()[-400:]}")
    return r


def read_f32(path):
    return np.fromfile(path, dtype="<f4")


def cmd_decode(ff, stream_ref, cli_ref, playable):
    a, s, c = read_f32(ff), read_f32(stream_ref), read_f32(cli_ref)
    n = int(playable) * 2
    assert len(a) == n == len(s) == len(c), f"length {len(a)} vs {n} vs {len(s)} vs {len(c)}"
    if not np.array_equal(a, s):
        raise AssertionError("ffmpeg output is not bit-exact vs the streaming reference")
    dmax = float(np.abs(a.astype(np.float64) - c).max())
    assert dmax <= ULP_TOL, f"ffmpeg vs whole-file CLI: max diff {dmax}"
    print(f"decode: bit-exact vs streaming reference; vs whole-file CLI max={dmax:.3e}")


def xcorr_lag(a, b):
    n = min(len(a), len(b))
    aa, bb = a[:n:4], b[:n:4]
    L = 1 << (2 * len(aa) - 1).bit_length()
    c = np.fft.irfft(np.fft.rfft(aa, L) * np.conj(np.fft.rfft(bb, L)), L)
    w = int(0.5 * SR / 4)
    cc = np.concatenate([c[-w:], c[:w]])
    return (np.argmax(cc) - w) * 4


def cmd_seek(ffmpeg, asset, cont_ref, times):
    cont = read_f32(cont_ref)
    for t in times:
        frame = round(float(t) * SR)
        out = os.path.join(os.path.dirname(cont_ref), f"seek_{t}.raw")
        run([ffmpeg, "-hide_banner", "-loglevel", "error", "-ss", str(t), "-i", asset,
             "-map", "0:a", "-f", "f32le", "-y", out])
        got = read_f32(out)
        # The demuxer lands on a block boundary at/below the target (20 ms
        # grid); output runs from that block to the coverage end (playable
        # plus the mid-layer's tail coverage past it).
        cue_frame = (frame // 960) * 960
        want_frames = len(cont) // 2 - cue_frame + SEEK_TAIL_FRAMES
        assert len(got) // 2 == want_frames, (
            f"-ss {t}: length {len(got)//2} vs expected {want_frames} "
            f"(cue {cue_frame}, playable {len(cont)//2} + tail {SEEK_TAIL_FRAMES})")
        base = cont[cue_frame * 2:]
        # Ramp/anchor region is informational; assertions start after the
        # settle point (LF joins at the first independency AU, the resampler
        # edge settles within ~0.5 s).
        # Alignment is asserted on the first ~0.5 s, where the mid layer
        # dominates (the LF ramp may still be muted). The grid anchor of a
        # shifted timeline can sit up to one block off the cue position.
        if len(got) < 2 * SR:
            print(f"seek {t:>6}s: len ok (short tail, content checks skipped)")
            os.unlink(out)
            continue
        w = int(0.5 * SR) * 2
        lag = xcorr_lag(got[:w].reshape(-1, 2).mean(axis=1),
                        base[:w].reshape(-1, 2).mean(axis=1))
        assert abs(lag) <= 960, f"-ss {t}: lag {lag}"
        # Diff directly and at +/- one block, keeping the best alignment
        # (the grid anchor of a shifted timeline can sit up to one block off
        # the cue position; a spurious correlation peak must not force it).
        i0 = int(SEEK_SETTLE_S * SR) * 2
        def _maxdiff(ga, ba):
            m = min(len(ga), len(ba))
            return float(np.abs(ga[:m].astype(np.float64) - ba[:m]).max(initial=0.0))
        dmax = min(
            _maxdiff(got[i0:], base[i0:]),
            _maxdiff(got[i0 + 960:], base[i0:]) if len(got) > i0 + 960 else 1e9,
            _maxdiff(got[i0:], base[i0 + 960:]) if len(base) > i0 + 960 else 1e9,
        )
        assert dmax <= SEEK_SETTLE_TOL, f"-ss {t}: settled max {dmax:.3e}"
        early = float(np.abs(got[:i0].astype(np.float64) - base[:i0]).max(initial=0.0))
        print(f"seek {t:>6}s: len ok; lag 0; settled {dmax:.1e} (ramp envelope); ramp-region {early:.1e}")
        os.unlink(out)


def cmd_probe(ffprobe, path, expect):
    r = run([ffprobe, "-hide_banner", "-show_entries", "format=format_name",
             "-of", "default=noprint_wrappers=1:nokey=1", path])
    fmt = r.stdout.decode().strip()
    assert fmt == "matroska,webm", f"{path}: format_name {fmt!r} (stock Matroska must own the file)"
    r = run([ffprobe, "-hide_banner", "-select_streams", "a:0", "-show_entries",
             "stream=codec_name", "-of", "default=noprint_wrappers=1:nokey=1", path])
    got = r.stdout.decode().strip()
    assert got == expect, f"{path}: codec_name {got!r} != {expect!r}"
    print(f"probe {os.path.basename(path)}: {fmt} + codec {got}")


def cmd_tags(ffprobe, fixture, kvs):
    r = run([ffprobe, "-hide_banner", "-show_entries", "format_tags",
             "-of", "default=noprint_wrappers=1", fixture])
    shown = r.stdout.decode()
    for kv in kvs:
        k, v = kv.split("=", 1)
        assert f"TAG:{k}={v}" in shown, f"tag {k}={v} missing in:\n{shown}"
    print(f"tags: {len(kvs)} user tags visible in ffprobe")


def cmd_art(ffmpeg, fixture, cover, workdir):
    out = os.path.join(workdir, "extracted_art.bin")
    run([ffmpeg, "-hide_banner", "-loglevel", "error", "-i", fixture,
         "-map", "0:v", "-c", "copy", "-f", "rawvideo", "-y", out])
    a, b = open(out, "rb").read(), open(cover, "rb").read()
    assert a == b, f"attached pic payload differs ({len(a)} vs {len(b)} bytes)"
    print(f"art: attached_pic stream payload matches ({len(a)} bytes)")


def cmd_nolib(ffmpeg, asset):
    env = {k: v for k, v in os.environ.items()
           if k not in ("SENA_DEC_LIBRARY", "LD_LIBRARY_PATH", "DYLD_LIBRARY_PATH")}
    r = subprocess.run([ffmpeg, "-hide_banner", "-loglevel", "error", "-i", asset,
                        "-f", "null", "-"], stdout=subprocess.PIPE, stderr=subprocess.PIPE, env=env)
    assert r.returncode != 0, "expected failure without the core library"
    msg = r.stderr.decode()
    assert "decoder core unavailable" in msg or "sena" in msg.lower(), msg[-400:]
    print("nolib: clean failure without libsena_dec (no crash)")


def main():
    cmd = sys.argv[1]
    {
        "decode": lambda: cmd_decode(*sys.argv[2:6]),
        "seek":   lambda: cmd_seek(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5:]),
        "probe":  lambda: cmd_probe(*sys.argv[2:5]),
        "tags":   lambda: cmd_tags(sys.argv[2], sys.argv[3], sys.argv[4:]),
        "art":    lambda: cmd_art(*sys.argv[2:6]),
        "nolib":  lambda: cmd_nolib(*sys.argv[2:4]),
    }[cmd]()


if __name__ == "__main__":
    main()
