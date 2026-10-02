#!/usr/bin/env python3
"""Golden validation (v2 single-track composite ES): parse the .sena
container, verify packet bit-equality with the workdir elementary streams,
decode the rails with the reference decoders (xhedec / opusdec / scipy
hilbert for the shifted top band), and check alignment constants + quality
parity.

usage: validate.py <input.wav> <profile 300|600> <out.sena> <workdir>
"""
import struct
import subprocess
import sys

import numpy as np
import soundfile as sf
import soxr
from scipy.signal import hilbert

SR = 48000
PAD = 0.631
HF_SPLIT_HZ = 12000.0
BLOCK_FRAMES = 960


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
    while mask:
        if f[p] & mask:
            break
        mask >>= 1
        id_len += 1
    if mask == 0:
        id_len = 4
    eid = f[p:p + id_len]
    sz, szlen = read_vint(f, p + id_len)
    return eid, p + id_len + szlen, sz


# ---------- v2 container: one A_MS/ACM track, CodecPrivate = WAVEFORMATEX +
# SENA-INIT, SimpleBlocks = composite 20 ms ES blocks ----------
def parse_sena(path):
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
    tracks, clusters, frames = {}, 0, {}
    p = seg[0]
    while p < seg[1]:
        eid, q, sz = read_elem(f, p)
        body = f[q:q + sz]
        if eid == bytes([0x16, 0x54, 0xAE, 0x6B]):  # Tracks
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
                        if e3 == bytes([0x25, 0x86, 0x88]):
                            tr["name"] = te[q3:q3 + s3].decode()
                        if e3 == bytes([0x63, 0xA2]):
                            tr["private"] = te[q3:q3 + s3]
                        pp2 = q3 + s3
                    tracks[tr["num"]] = tr
                pp = q2 + s2
        if eid == bytes([0x1F, 0x43, 0xB6, 0x75]):  # Cluster
            clusters += 1
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0xA3]):  # SimpleBlock
                    sb = body[q2:q2 + s2]
                    tn, tnlen = read_vint(sb, 0)
                    assert not (sb[tnlen + 2] & 0x06), "lacing not expected"
                    frames.setdefault(tn, []).append(sb[tnlen + 3:])
                pp = q2 + s2
        p = q + sz
    return tracks, clusters, frames


def parse_init(cp):
    """CodecPrivate -> SENA-INIT fields."""
    assert len(cp) >= 18, "short CodecPrivate"
    tag, ch = struct.unpack("<HH", cp[:4])
    rate, _avg = struct.unpack("<II", cp[4:12])
    bits, cb = struct.unpack("<HH", cp[14:18])
    assert tag == 0x534E and ch == 2 and rate == SR and bits == 32, "bad WAVEFORMATEX"
    blob = cp[18:18 + cb]
    assert len(blob) == cb, "truncated SENA-INIT"
    assert blob[:4] == b"SEN2", blob[:4]
    version = struct.unpack("<H", blob[4:6])[0]
    assert version == 2, version
    profile = {0: 300, 1: 600}[blob[6]]
    three_track = bool(blob[7])
    lf_rate = struct.unpack("<I", blob[8:12])[0]
    playable = struct.unpack("<Q", blob[12:20])[0]
    sha = blob[20:52]
    p = 52

    def take16():
        nonlocal p
        n = struct.unpack("<H", blob[p:p + 2])[0]
        v = blob[p + 2:p + 2 + n]
        p += 2 + n
        return v

    asc = take16()
    opus_head = take16()
    hf_head = take16() if three_track else None
    assert p == len(blob), "trailing bytes in SENA-INIT"
    return {
        "profile": profile, "three_track": three_track, "lf_rate": lf_rate,
        "playable": playable, "sha": sha, "asc": asc,
        "opus_head": opus_head, "hf_head": hf_head,
    }


def decode_block(b):
    """composite ES block -> (mid_packet, [lf_au...], [top_packet...])"""
    p = 0
    flags = b[p]; p += 1
    assert flags == 0, flags
    mid_len = struct.unpack("<H", b[p:p + 2])[0]; p += 2
    mid = b[p:p + mid_len]; p += mid_len
    assert mid_len > 0
    lf_count = b[p]; p += 1
    lf = []
    if lf_count:
        p += 3  # absolute AU index of the first AU
        for _ in range(lf_count):
            n = int.from_bytes(b[p:p + 3], "little"); p += 3
            lf.append(b[p:p + n]); p += n
    hf_count = b[p]; p += 1
    hf = []
    for _ in range(hf_count):
        n = struct.unpack("<H", b[p:p + 2])[0]; p += 2
        hf.append(b[p:p + n]); p += n
    assert p == len(b), "trailing bytes in block"
    return mid, lf, hf


# ---------- workdir elementary streams ----------
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
    off = mstart + 2  # exhale's 2-byte "informative" mdat preamble
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


# ---------- reference decode helpers ----------
def opus_duration(pkt):
    toc = pkt[0]
    config = toc >> 3
    code = (toc >> 1) & 0x03
    if config < 12:
        ms = [2.5, 5.0, 10.0, 20.0][config & 3]
        frames = 1 if config < 4 else 2
    else:
        ms = 60.0
        frames = 1 if code < 2 else (pkt[1] + 1 if code == 3 else 2)
    return int(round(ms * 48.0)) * frames


def make_opus_ogg(head, packets, pre_skip, final_granule):
    table = []
    for i in range(256):
        r = i << 24
        for _ in range(8):
            r = ((r << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if r & 0x80000000 else (r << 1) & 0xFFFFFFFF
        table.append(r)
    tags = b"OpusTags" + struct.pack("<I", 4) + b"sena\x00" + struct.pack("<I", 0)

    def ogg_crc(data, crc=0):
        for b in data:
            crc = (crc << 8) ^ table[((crc >> 24) & 0xFF) ^ b]
        return crc & 0xFFFFFFFF

    def page(pkts, granule, serial, seq, htype):
        lacing = []
        for pkt in pkts:
            rem = len(pkt)
            while rem >= 255:
                lacing.append(255)
                rem -= 255
            lacing.append(rem)
        segs = bytes(lacing)
        body = b"".join(pkts)
        hdr = (b"OggS\x00" + bytes([htype]) + struct.pack("<q", granule)
               + struct.pack("<I", serial) + struct.pack("<I", seq)
               + b"\x00\x00\x00\x00" + bytes([len(segs)]) + segs)
        pag = bytearray(hdr + body)
        pag[22:26] = struct.pack("<I", ogg_crc(bytes(pag)))
        return bytes(pag)

    serial = 0x53454E41
    pages = [page([head], 0, serial, 0, 0x02)]
    pages.append(page([tags], 0, serial, 1, 0x00))
    g = pre_skip
    for i, pkt in enumerate(packets):
        g += opus_duration(pkt)
        htype = 0x04 if i == len(packets) - 1 else 0x00
        pages.append(page([pkt], g, serial, i + 2, htype))
    return b"".join(pages)


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
    tracks, clusters, frames = parse_sena(sena_path)
    assert len(tracks) == 1 and tracks[1]["codec"] == "A_MS/ACM", tracks
    init = parse_init(tracks[1]["private"])
    assert init["profile"] == profile, (init["profile"], profile)
    exp_name = f"Sena xAAC-Opus@{profile}" + ("-Opus@12000" if init["three_track"] else "")
    assert tracks[1].get("name") == exp_name, tracks[1].get("name")
    assert clusters > 10, clusters
    playable = init["playable"]

    mid_pkts, lf_aus, top_pkts = [], [], []
    for i, blk in enumerate(frames[1]):
        mid, lf, top = decode_block(blk)
        mid_pkts.append(mid)
        lf_aus.extend(lf)  # block order + in-block order = absolute AU order
        top_pkts.extend(top)

    lf_ref = m4a_aus(f"{wd}/lf.m4a")
    assert len(lf_ref) == len(lf_aus) and all(a == b for a, b in zip(lf_ref, lf_aus)), "LF AUs differ"
    mid_ogg = f"{wd}/mid.opus" if init["three_track"] else f"{wd}/hf.opus"
    mid_ref = ogg_audio(mid_ogg)
    assert len(mid_ref) == len(mid_pkts) and all(a == b for a, b in zip(mid_ref, mid_pkts)), \
        "mid packets differ or tags not stripped"
    if init["three_track"]:
        top_ref = ogg_audio(f"{wd}/hf.opus")
        assert len(top_ref) == len(top_pkts) and all(a == b for a, b in zip(top_ref, top_pkts)), \
            "top packets differ or tags not stripped"
    print(f"container OK: {len(lf_aus)} LF AUs, {len(mid_pkts)} mid + "
          f"{len(top_pkts)} top packets, {clusters} clusters, "
          f"three_track={init['three_track']}")

    subprocess.run(["xhedec", f"{wd}/lf.m4a", f"{wd}/lf_dec.wav"], check=True)
    head = init["opus_head"]
    pre_skip = struct.unpack("<H", head[10:12])[0]
    with open(f"{wd}/mid_re.opus", "wb") as fh:
        fh.write(make_opus_ogg(head, mid_pkts, pre_skip, playable + pre_skip))
    subprocess.run(["opusdec", "--quiet", "--force-wav", f"{wd}/mid_re.opus", f"{wd}/hf_dec.wav"], check=True)
    if init["three_track"]:
        thead = init["hf_head"]
        tskip = struct.unpack("<H", thead[10:12])[0]
        with open(f"{wd}/top_re.opus", "wb") as fh:
            fh.write(make_opus_ogg(thead, top_pkts, tskip, 0))
        subprocess.run(["opusdec", "--quiet", "--force-wav", "--rate", "48000",
                        f"{wd}/top_re.opus", f"{wd}/hf2_dec.wav"], check=True)

    ref, _ = sf.read(inp, dtype="float64", always_2d=True)
    lf, fs = sf.read(f"{wd}/lf_dec.wav", dtype="float64", always_2d=True)
    if fs != SR:
        lf = soxr.resample(lf, fs, SR, quality="HQ")
    hp, _ = sf.read(f"{wd}/hf_dec.wav", dtype="float64", always_2d=True)
    lf /= PAD
    hp /= PAD
    if init["three_track"]:
        hp2, fs2 = sf.read(f"{wd}/hf2_dec.wav", dtype="float64", always_2d=True)
        hp2 /= PAD
        # The A_OPUSHF track carries the 12 kHz+ band SSB-shifted to
        # baseband: shift it back up (analytic signal times
        # exp(+2j*pi*fc*t), take real part) before summing with the mid.
        assert fs2 == SR, fs2
        t = np.arange(len(hp2)) / SR
        hp2 = np.real(hilbert(hp2, axis=0) * np.exp(2j * np.pi * HF_SPLIT_HZ * t)[:, None])
        m = min(len(hp), len(hp2))
        hp = hp[:m] + hp2[:m]
    lag_l = xcorr_lag(ref.mean(axis=1), lf.mean(axis=1), SR)
    lag_h = xcorr_lag(ref.mean(axis=1), hp.mean(axis=1), SR)
    exp_lag = -(1024 * SR // init["lf_rate"])
    assert abs(lag_l - exp_lag) <= 8, f"xhe lag {lag_l} vs {exp_lag}"
    assert abs(lag_h) <= 8, f"opus lag {lag_h}"

    def appl(x, lag):
        return x[lag:] if lag >= 0 else x[-lag:]

    lfs, hps = appl(lf, lag_l), appl(hp, lag_h)
    assert playable == len(ref), (playable, len(ref))
    n = playable
    sena = lfs[:n] + hps[:n]
    refn = ref[:n]
    sig = bsig(refn, sena, 24, 144)
    corr = float(np.corrcoef(refn.mean(axis=1)[::8], sena.mean(axis=1)[::8])[0, 1])
    print(f"lags: xhe={lag_l} opus={lag_h}  LFσ={sig:.3f}  corr={corr:.4f}  n={n}")
    assert sig < 0.10, sig
    assert corr > 0.99, corr


if __name__ == "__main__":
    main()
