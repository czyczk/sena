#!/usr/bin/env python3
"""Golden validation: parse the .sena container, verify packet bit-equality with
the workdir elementary streams, decode both tracks with the reference decoders,
and check alignment constants + quality parity.

usage: validate.py <input.wav> <profile 300|600> <out.sena> <workdir>
"""
import glob
import struct
import subprocess
import sys

import numpy as np
import soundfile as sf
import soxr

SR = 48000
PAD = 0.631
DELAY = {300: 3072, 600: 1536}


def read_vint(f, p):
    mask = 0x80
    size = 0
    while mask:
        if f[p] & mask:
            break
        mask >>= 1
    size = 8 - (mask.bit_length() - 1) if mask else 0
    v = 0
    for i in range(size):
        v = (v << 8) | f[p + i]
    v &= (1 << (7 * size)) - 1
    return v, size


def read_elem(f, p):
    mask = 0x80
    id_len = 1
    while mask and not (f[p] & mask):
        mask >>= 1
        id_len += 1
    if mask == 0:
        id_len = 4
    eid = f[p:p + id_len]
    sz, szlen = read_vint(f, p + id_len)
    return eid, p + id_len + szlen, sz


def parse_mka(path):
    f = open(path, "rb").read()
    p = 0
    seg = None
    while p < len(f):
        eid, q, sz = read_elem(f, p)
        if eid == bytes([0x18, 0x53, 0x80, 0x67]):
            seg = (q, q + sz)
            break
        p = q + sz
    assert seg, "no segment"
    tracks, tags, clusters, frames = {}, {}, 0, {}
    p = seg[0]
    while p < seg[1]:
        eid, q, sz = read_elem(f, p)
        body = f[q:q + sz]
        if eid == bytes([0x16, 0x54, 0xAE, 0x6B]):
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0xAE]):
                    te = body[q2:q2 + s2]
                    tr = {}
                    pp2 = 0
                    while pp2 < len(te):
                        e3, q3, s3 = read_elem(te, pp2)
                        if e3 == bytes([0xD7]):
                            tr["num"] = int.from_bytes(te[q3:q3 + s3], "big")
                        if e3 == bytes([0x86]):
                            tr["codec"] = te[q3:q3 + s3].decode()
                        if e3 == bytes([0x56, 0xAA]):
                            tr["delay_ns"] = int.from_bytes(te[q3:q3 + s3], "big")
                        pp2 = q3 + s3
                    tracks[tr["num"]] = tr
                pp = q2 + s2
        if eid == bytes([0x12, 0x54, 0xC3, 0x67]):
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0x73, 0x73]):
                    tg = body[q2:q2 + s2]
                    pp2 = 0
                    while pp2 < len(tg):
                        e3, q3, s3 = read_elem(tg, pp2)
                        if e3 == bytes([0x67, 0xC8]):
                            st = tg[q3:q3 + s3]
                            k = v = None
                            pp3 = 0
                            while pp3 < len(st):
                                e4, q4, s4 = read_elem(st, pp3)
                                if e4 == bytes([0x45, 0xA3]):
                                    k = st[q4:q4 + s4].decode()
                                if e4 == bytes([0x44, 0x87]):
                                    v = st[q4:q4 + s4].decode()
                                pp3 = q4 + s4
                            if k:
                                tags[k] = v
                        pp2 = q3 + s3
                pp = q2 + s2
        if eid == bytes([0x1F, 0x43, 0xB6, 0x75]):
            clusters += 1
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0xA3]):
                    sb = body[q2:q2 + s2]
                    tn, tnlen = read_vint(sb, 0)
                    frames.setdefault(tn, []).append(sb[tnlen + 3:])
                pp = q2 + s2
        p = q + sz
    return tracks, tags, clusters, frames


def m4a_aus(path):
    f = open(path, "rb").read()

    def boxes(s_, e_):
        out = []
        pp = s_
        while pp + 8 <= e_:
            szz = struct.unpack(">I", f[pp:pp + 4])[0]
            tg = f[pp + 4:pp + 8]
            if szz < 8 or pp + szz > e_:
                break
            out.append((tg, pp + 8, pp + szz))
            pp += szz
        return out

    sizes, mstart = None, None
    for tg, s_, e_ in boxes(0, len(f)):
        if tg == b"mdat":
            mstart = s_
        if tg == b"moov":
            for t2, s2, e2 in boxes(s_, e_):
                if t2 != b"trak":
                    continue
                for t3, s3, e3 in boxes(s2, e2):
                    if t3 != b"mdia":
                        continue
                    for t4, s4, e4 in boxes(s3, e3):
                        if t4 != b"minf":
                            continue
                        for t5, s5, e5 in boxes(s4, e4):
                            if t5 != b"stbl":
                                continue
                            for t6, s6, e6 in boxes(s5, e5):
                                if t6 == b"stsz":
                                    n = int.from_bytes(f[s6 + 8:s6 + 12], "big")
                                    sizes = [
                                        int.from_bytes(f[s6 + 12 + 4 * i:s6 + 16 + 4 * i], "big")
                                        for i in range(n)
                                    ]
    off = mstart
    aus = []
    for sz_ in sizes:
        aus.append(f[off:off + sz_])
        off += sz_
    return aus


def ogg_audio(path):
    f = open(path, "rb").read()
    pkts, pos, cur = [], 0, b""
    while pos + 27 <= len(f):
        assert f[pos:pos + 4] == b"OggS"
        nsegs = f[pos + 26]
        segtab = f[pos + 27:pos + 27 + nsegs]
        body = pos + 27 + nsegs
        for l in segtab:
            cur += f[body:body + l]
            body += l
            if l < 255:
                pkts.append(cur)
                cur = b""
        if f[pos + 5] & 4:
            break
        pos = body
    return pkts[2:]


def xcorr_lag(a, b, fs):
    n = min(len(a), len(b))
    aa, bb = a[:n:4], b[:n:4]
    L = 1 << (2 * len(aa) - 1).bit_length()
    c = np.fft.irfft(np.fft.rfft(aa, L) * np.conj(np.fft.rfft(bb, L)), L)
    w = int(0.5 * fs / 4)
    cc = np.concatenate([c[-w:], c[:w]])
    return (np.argmax(cc) - w) * 4


def bsig(ref, dec, lo, hi):
    def bp(x):
        X = np.fft.rfft(x.mean(axis=1))
        f = np.fft.rfftfreq(len(x), 1 / SR)
        X[(f < lo) | (f >= hi)] = 0
        return np.fft.irfft(X, len(x))

    def env_db(x):
        win = SR // 50
        c = np.cumsum(x * x)
        c = np.pad(c, (win, 0))[:-win]
        e = np.sqrt((c[win:] - c[:-win]) / win)[::SR // 100]
        return 20 * np.log10(e + 1e-12)

    n = min(len(ref), len(dec))
    er, ed = env_db(bp(ref[:n])), env_db(bp(dec[:n]))
    m = min(len(er), len(ed))
    mask = er[:m] > er[:m].max() - 40
    return float((ed[:m] - er[:m])[mask].std())


def main():
    inp, profile_s, sena_path, wd = sys.argv[1:5]
    profile = int(profile_s)
    tracks, tags, clusters, frames = parse_mka(sena_path)
    assert tracks[1]["codec"] == "A_OPUS", tracks
    assert tracks[2]["codec"] == "A_SENALF", tracks
    assert tags["SENA_PROFILE"] == profile_s, tags
    assert clusters > 10, clusters
    exp_delay_ns = DELAY[profile] * 1_000_000_000 // SR
    assert abs(int(tracks[2]["delay_ns"]) - exp_delay_ns) <= 3_000_000, tracks

    lf_pkts = frames[2]
    lf_aus = m4a_aus(f"{wd}/lf.m4a")
    assert len(lf_pkts) == len(lf_aus) and all(
        a == b for a, b in zip(lf_aus, lf_pkts)
    ), "LF packets differ"
    hf_pkts = frames[1]
    hf_audio = ogg_audio(f"{wd}/hf.opus")
    assert len(hf_pkts) == len(hf_audio) and all(
        a == b for a, b in zip(hf_audio, hf_pkts)
    ), "HF packets differ or tags not stripped"
    print(f"container OK: {len(lf_pkts)} LF AUs, {len(hf_pkts)} HF packets, {clusters} clusters")

    subprocess.run(["xhedec", f"{wd}/lf.m4a", f"{wd}/lf_dec.wav"], check=True)
    subprocess.run(["opusdec", "--quiet", "--force-wav", f"{wd}/hf.opus", f"{wd}/hf_dec.wav"], check=True)
    ref, _ = sf.read(inp, dtype="float64", always_2d=True)
    lf, fs = sf.read(f"{wd}/lf_dec.wav", dtype="float64", always_2d=True)
    if fs != SR:
        lf = soxr.resample(lf, fs, SR, quality="HQ")
    hp, _ = sf.read(f"{wd}/hf_dec.wav", dtype="float64", always_2d=True)
    lf /= PAD
    hp /= PAD
    lag_l = xcorr_lag(ref.mean(axis=1), lf.mean(axis=1), SR)
    lag_h = xcorr_lag(ref.mean(axis=1), hp.mean(axis=1), SR)
    assert abs(-lag_l - DELAY[profile]) <= 4, f"xhe lag {lag_l} vs {-DELAY[profile]}"
    assert abs(lag_h) <= 4, f"opus lag {lag_h}"

    def appl(x, lag):
        return x[lag:] if lag >= 0 else x[-lag:]

    lfs, hps = appl(lf, lag_l), appl(hp, lag_h)
    n = min(len(lfs), len(hps), len(ref))
    sena = lfs[:n] + hps[:n]
    refn = ref[:n]
    sig = bsig(refn, sena, 24, 144)
    corr = float(np.corrcoef(refn.mean(axis=1)[::8], sena.mean(axis=1)[::8])[0, 1])
    print(f"lags: xhe={lag_l} opus={lag_h}  LFσ={sig:.3f}  corr={corr:.4f}")
    assert sig < 0.10, sig
    assert corr > 0.99, corr


if __name__ == "__main__":
    main()
