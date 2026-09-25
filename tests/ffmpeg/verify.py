#!/usr/bin/env python3
"""Deterministic checks for the ffmpeg Sena demuxer.

Subcommands (see run.sh for orchestration):
  decode  <ffmpeg.raw> <stream_ref.raw> <cli_ref.raw> <playable_frames>
  seek    <ffmpeg> <asset.sena> <stream_ref.raw> <t1> [t2 ...]
  probe   <ffprobe> <file> <expected_format_name>
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


def cmd_seek(ffmpeg, dumpbin, asset, stream_ref, times):
    cont = read_f32(stream_ref)
    for t in times:
        frame = round(float(t) * SR)
        out = os.path.join(os.path.dirname(stream_ref), f"seek_{t}.raw")
        ref = out + ".ref"
        run([ffmpeg, "-hide_banner", "-loglevel", "error", "-ss", str(t), "-i", asset,
             "-map", "0:a", "-f", "f32le", "-y", out])
        with open(ref, "wb") as fh:
            subprocess.run([dumpbin, asset, "1024", "--seek", str(frame)], stdout=fh, check=True)
        got, want = read_f32(out), read_f32(ref)
        assert len(got) == len(want), f"-ss {t}: length {len(got)} vs {len(want)}"
        if not np.array_equal(got, want):
            raise AssertionError(f"-ss {t}: not bit-exact vs the Rust streaming seek path")
        os.unlink(out)
        os.unlink(ref)
        # Warmup envelope vs the continuous decode: the LF xHE-AAC random
        # access is codec-inherent (different SBR noise state + a short LPD
        # warmup; measured in examples/lf_preroll_probe.rs), so only alignment
        # and the steady-state noise floor are asserted against it.
        base = cont[frame * 2:]
        lagmsg = "lag n/a (short tail)"
        if len(base) >= 4 * SR:  # xcorr needs a meaningful window
            lag = xcorr_lag(got.reshape(-1, 2).mean(axis=1), base.reshape(-1, 2).mean(axis=1))
            assert lag == 0, f"-ss {t}: lag {lag}"
            lagmsg = "lag 0"
        d = np.abs(got.reshape(-1, 2).astype(np.float64) - base.reshape(-1, 2)).max(axis=1)
        first, rest = d[: SR], d[SR:]
        assert float(rest.max(initial=0.0)) <= 5e-3, f"-ss {t}: steady max {rest.max():.3e}"
        print(f"seek {t:>6}s: bit-exact vs Rust seek path; {lagmsg}; vs continuous "
              f"warmup {first.max(initial=0.0):.1e}, steady {rest.max(initial=0.0):.1e}")


def cmd_probe(ffprobe, path, expect):
    r = run([ffprobe, "-hide_banner", "-show_entries", "format=format_name",
             "-of", "default=noprint_wrappers=1:nokey=1", path])
    got = r.stdout.decode().strip()
    assert got == expect, f"{path}: format_name {got!r} != {expect!r}"
    print(f"probe {os.path.basename(path)}: {got}")


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
        "seek":   lambda: cmd_seek(sys.argv[2], sys.argv[3], sys.argv[4], sys.argv[5], sys.argv[6:]),
        "probe":  lambda: cmd_probe(*sys.argv[2:5]),
        "tags":   lambda: cmd_tags(sys.argv[2], sys.argv[3], sys.argv[4:]),
        "art":    lambda: cmd_art(*sys.argv[2:6]),
        "nolib":  lambda: cmd_nolib(*sys.argv[2:4]),
    }[cmd]()


if __name__ == "__main__":
    main()
