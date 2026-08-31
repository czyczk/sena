#!/usr/bin/env python3
"""Reference decode for Sena assets: .sena -> playable 48 kHz stereo WAV.

Self-contained: demuxes the Matroska container itself (no reliance on the
encoder's workdir), reconstructs the xHE-AAC M4A (ASC + AUs) and the Ogg
Opus stream (OpusHead + packets), decodes with the reference cores
(xhedec / opusdec), applies the per-profile warmup constants, restores the
-4 dB pad, sums the two bands, truncates to the source length, and writes
32-bit float / 24-bit / 16-bit WAV references.

usage: decode_ref.py <file.sena> <src.wav> <out_prefix>
  env: XHEDEC (default .../libxaac-wrapper/build/xhedec)
       OPUSDEC (default .../vendor/tools/bin/opusdec)
"""
import os
import shutil
import struct
import subprocess
import sys

import numpy as np
import soundfile as sf
import soxr

SR = 48000
PAD = 0.631  # -4 dB pre-gain, restored on decode
HERE = os.path.dirname(os.path.abspath(__file__))
XHEDEC = os.environ.get("XHEDEC") or shutil.which("xhedec")
OPUSDEC = os.environ.get("OPUSDEC") or shutil.which("opusdec")


# ---------- EBML / Matroska ----------
def read_vint(f, p):
    mask = 0x80
    while mask and not (f[p] & mask):
        mask >>= 1
    size = 0
    if mask:
        size = 8 - (mask.bit_length() - 1)
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
    eid = f[p : p + id_len]
    sz, szlen = read_vint(f, p + id_len)
    return eid, p + id_len + szlen, sz


def parse_sena(path):
    """Return (tracks, tags, frames) where tracks[t]=dict, frames[t]=[bytes]."""
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
    tracks, tags, frames = {}, {}, {}
    p = seg[0]
    while p < seg[1]:
        eid, q, sz = read_elem(f, p)
        body = f[q : q + sz]
        if eid == bytes([0x16, 0x54, 0xAE, 0x6B]):
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0xAE]):
                    te = body[q2 : q2 + s2]
                    tr = {"private": None, "rate": 0.0, "delay_ns": None}
                    pp2 = 0
                    while pp2 < len(te):
                        e3, q3, s3 = read_elem(te, pp2)
                        if e3 == bytes([0xD7]):
                            tr["num"] = int.from_bytes(te[q3 : q3 + s3], "big")
                        if e3 == bytes([0x86]):
                            tr["codec"] = te[q3 : q3 + s3].decode()
                        if e3 == bytes([0x56, 0xAA]):
                            tr["delay_ns"] = int.from_bytes(te[q3 : q3 + s3], "big")
                        if e3 == bytes([0x63, 0xA2]):
                            tr["private"] = te[q3 : q3 + s3]
                        if e3 == bytes([0xE1]):
                            au = te[q3 : q3 + s3]
                            pp4 = 0
                            while pp4 < len(au):
                                e4, q4, s4 = read_elem(au, pp4)
                                if e4 == bytes([0xB5]):
                                    tr["rate"] = (
                                        struct.unpack(">f", au[q4 : q4 + s4])[0]
                                        if s4 == 4
                                        else struct.unpack(">d", au[q4 : q4 + s4])[0]
                                    )
                                pp4 = q4 + s4
                        pp2 = q3 + s3
                    tracks[tr["num"]] = tr
                pp = q2 + s2
        if eid == bytes([0x12, 0x54, 0xC3, 0x67]):
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0x73, 0x73]):
                    tg = body[q2 : q2 + s2]
                    pp2 = 0
                    while pp2 < len(tg):
                        e3, q3, s3 = read_elem(tg, pp2)
                        if e3 == bytes([0x67, 0xC8]):
                            st = tg[q3 : q3 + s3]
                            k = v = None
                            pp3 = 0
                            while pp3 < len(st):
                                e4, q4, s4 = read_elem(st, pp3)
                                if e4 == bytes([0x45, 0xA3]):
                                    k = st[q4 : q4 + s4].decode()
                                if e4 == bytes([0x44, 0x87]):
                                    v = st[q4 : q4 + s4].decode()
                                pp3 = q4 + s4
                            if k:
                                tags[k] = v
                        pp2 = q3 + s3
                pp = q2 + s2
        if eid == bytes([0x1F, 0x43, 0xB6, 0x75]):
            pp = 0
            while pp < len(body):
                e2, q2, s2 = read_elem(body, pp)
                if e2 == bytes([0xA3]):
                    sb = body[q2 : q2 + s2]
                    tn, tnlen = read_vint(sb, 0)
                    assert not (sb[tnlen + 2] & 0x06), "lacing not expected"
                    frames.setdefault(tn, []).append(sb[tnlen + 3 :])
                pp = q2 + s2
        p = q + sz
    return tracks, tags, frames


# ---------- MP4 (minimal xHE-AAC mux for xhedec) ----------
def box(tag, payload):
    return struct.pack(">I4s", 8 + len(payload), tag) + payload


def descr(tag, payload):
    """MPEG-4 descriptor with the expanded 0x80 0x80 0x80 <len> size form
    (the exact form xhedec's esds scanner looks for)."""
    assert len(payload) < 128
    return bytes([tag, 0x80, 0x80, 0x80, len(payload)]) + payload


def make_m4a(asc, aus, rate):
    esds = descr(
        0x03,
        struct.pack(">HH", 0, 0)
        + descr(
            0x04,
            struct.pack(">BBBBB", 0x40, 0x15, 0, 0, 0) + descr(0x05, asc),
        ),
    )
    # AudioSampleEntry: 8-byte box header + 6 reserved + dri + 8 reserved
    # + ch/bit/predef/reserved + samplerate -> children at offset 36.
    mp4a = box(
        b"mp4a",
        b"\x00" * 6
        + struct.pack(">H", 1)
        + b"\x00" * 8
        + struct.pack(">HHHH", 2, 16, 0, 0)
        + struct.pack(">I", rate << 16)
        + box(b"esds", struct.pack(">I", 0) + esds),
    )
    stsd = box(b"stsd", struct.pack(">II", 0, 1) + mp4a)
    stts = box(b"stts", struct.pack(">IIII", 0, 1, len(aus), 1024))
    stsz = box(
        b"stsz",
        struct.pack(">III", 0, 0, len(aus))
        + b"".join(struct.pack(">I", len(a)) for a in aus),
    )
    stsc = box(b"stsc", struct.pack(">IIII", 0, 1, 1, len(aus)) + struct.pack(">I", 1))
    ftyp = box(b"ftyp", b"m4a \x00\x00\x02\x00m4a \x00\x00\x00\x00")
    mdat_off = len(ftyp) + 8  # first AU right after the mdat header
    stco = box(b"stco", struct.pack(">II", 0, 1) + struct.pack(">I", mdat_off))
    stbl = box(b"stbl", stsd + stts + stsz + stsc + stco)
    smhd = box(b"smhd", struct.pack(">IHH", 0, 0, 0))
    url = box(b"url ", struct.pack(">I", 1))
    dref = box(b"dref", struct.pack(">II", 0, 1) + url)
    dinf = box(b"dinf", dref)
    minf = box(b"minf", smhd + dinf + stbl)
    hdlr = box(
        b"hdlr", struct.pack(">II4s", 0, 0, b"soun") + b"\x00" * 12 + b"SoundHandler\x00"
    )
    mdhd = box(
        b"mdhd",
        struct.pack(">IIII", 0, 0, 0, rate)
        + struct.pack(">I", len(aus) * 1024)
        + struct.pack(">HH", 2, 16),
    )
    mdia = box(b"mdia", mdhd + hdlr + minf)
    dur_ms = len(aus) * 1024 * 1000 // rate
    tkhd = box(
        b"tkhd",
        struct.pack(">IIIIII", 3, 0, 0, 1, 0, dur_ms)
        + b"\x00" * 52
        + struct.pack(">HHHH", 0x0100, 0, 0, 0),
    )
    trak = box(b"trak", tkhd + mdia)
    mvhd = box(
        b"mvhd",
        struct.pack(">IIII", 0, 0, 0, 1000)
        + struct.pack(">I", dur_ms)
        + b"\x00" * 80
        + struct.pack(">H", 0x0100)
        + b"\x00" * 10,
    )
    moov = box(b"moov", mvhd + trak)
    mdat = box(b"mdat", b"".join(aus))
    return ftyp + mdat + moov


# ---------- Ogg Opus (minimal mux for opusdec) ----------
def ogg_crc(data, table, crc=0):
    for b in data:
        crc = (crc << 8) ^ table[((crc >> 24) & 0xFF) ^ b]
    return crc & 0xFFFFFFFF


def make_opus_ogg(head, packets, pre_skip, final_granule):
    """head: OpusHead packet; packets: audio packets; paginate simply
    (one page per packet); final_granule sets the EOS-page end trim."""
    table = []
    for i in range(256):
        r = i << 24
        for _ in range(8):
            r = ((r << 1) ^ 0x04C11DB7) & 0xFFFFFFFF if r & 0x80000000 else (r << 1) & 0xFFFFFFFF
        table.append(r)
    tags = b"OpusTags" + struct.pack("<I", 4) + b"sena\x00" + struct.pack("<I", 0)

    def page(pkts, granule, serial, seq, htype):
        lacing = []
        for p in pkts:
            rem = len(p)
            while rem >= 255:
                lacing.append(255)
                rem -= 255
            lacing.append(rem)  # 0 terminates a packet at a 255 boundary
        segs = bytes(lacing)
        body = b"".join(pkts)
        assert all(len(p) <= 255 * 255 for p in pkts)
        hdr = b"OggS\x00" + bytes([htype]) + struct.pack("<q", granule) + struct.pack("<I", serial) + struct.pack("<I", seq) + b"\x00\x00\x00\x00" + bytes([len(segs)]) + segs
        pag = bytearray(hdr + body)
        pag[22:26] = struct.pack("<I", ogg_crc(bytes(pag), table))
        return bytes(pag)

    serial = 0x53454E41
    pages = [page([head], 0, serial, 0, 0x02)]  # BOS
    pages.append(page([tags], 0, serial, 1, 0x00))
    g = pre_skip
    for i, p in enumerate(packets):
        g += opus_duration(p)
        htype = 0x04 if i == len(packets) - 1 else 0x00
        pages.append(page([p], g, serial, i + 2, htype))
    return b"".join(pages)


def opus_duration(pkt):
    """Samples @48 kHz in an audio packet (RFC 6716 frame size parse)."""
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


# ---------- metrics ----------
def xcorr_lag(a, b, fs):
    n = min(len(a), len(b))
    aa, bb = a[:n:4], b[:n:4]
    L = 1 << (2 * len(aa) - 1).bit_length()
    c = np.fft.irfft(np.fft.rfft(aa, L) * np.conj(np.fft.rfft(bb, L)), L)
    w = int(0.5 * fs / 4)
    cc = np.concatenate([c[-w:], c[:w]])
    lag = (np.argmax(cc) - w) * 4
    best, bl = -2.0, 0
    for dl in range(max(-4096, lag - 16), lag + 17, 4):
        if dl >= 0:
            aa2, bb2 = a[dl:], b[: len(a) - dl]
        else:
            aa2, bb2 = a[: len(b) + dl], b[-dl:]
        m = min(len(aa2), len(bb2))
        if m < fs:
            continue
        cc2 = np.corrcoef(aa2[:m:8], bb2[:m:8])[0, 1]
        if cc2 > best:
            best, bl = cc2, dl
    return bl, best


def quant(x, bits):
    s = (1 << (bits - 1)) - 1
    v = np.round(np.clip(x, -1.0, 1.0) * s)
    return v.astype(np.int32 if bits > 16 else np.int16)


def write_pcm_wav(path, data, bits):
    """data: (n, 2) float64 in [-1,1]."""
    n = len(data)
    bytes_per = bits // 8
    pcm = quant(data, bits)
    if bits == 16:
        body = pcm.astype("<i2").tobytes()
    elif bits == 24:
        b = bytearray()
        for v in pcm.ravel():
            b += int(v).to_bytes(3, "little", signed=True)
        body = bytes(b)
    else:
        body = data.astype("<f4").tobytes()
    hdr = (
        struct.pack("<4sI4s", b"RIFF", 36 + len(body), b"WAVE")
        + struct.pack("<4sI", b"fmt ", 16)
        + struct.pack("<HHIIHH", 3 if bits == 32 else 1, 2, SR, SR * 2 * bytes_per, 2 * bytes_per, bits)
        + struct.pack("<4sI", b"data", len(body))
    )
    with open(path, "wb") as f:
        f.write(hdr + body)


def main():
    sena_path, src_path, out_prefix = sys.argv[1:4]
    if not XHEDEC or not OPUSDEC:
        sys.exit("xhedec/opusdec not found: set XHEDEC and OPUSDEC env vars")
    tracks, tags, frames = parse_sena(sena_path)
    assert tracks[1]["codec"] == "A_OPUS" and tracks[2]["codec"] == "A_SENALF", tracks
    profile = tags.get("SENA_PROFILE")
    lf_rate = int(round(tracks[2]["rate"]))
    assert lf_rate in (16000, 32000), lf_rate
    warmup48 = 1024 * SR // lf_rate  # one 1024-sample core frame, in 48k samples
    assert abs(int(tracks[2]["delay_ns"]) - 1024 * 1_000_000_000 // lf_rate) <= 3_000_000

    tmp = os.path.join(os.path.dirname(out_prefix) or ".", f".ref-{os.path.basename(sena_path)}-{os.getpid()}")
    os.makedirs(tmp, exist_ok=True)
    try:
        m4a = os.path.join(tmp, "lf.m4a")
        open(m4a, "wb").write(make_m4a(tracks[2]["private"], frames[2], lf_rate))
        ogg = os.path.join(tmp, "hf.opus")
        head = tracks[1]["private"]
        pre_skip = struct.unpack("<H", head[10:12])[0]
        with sf.SoundFile(src_path) as s:
            ref = s.read(dtype="float64", always_2d=True)
        # authoritative playable length from the container
        playable = int(tags["SENA_PLAYABLE_SAMPLES"])
        assert playable == len(ref), (playable, len(ref))
        target = playable
        open(ogg, "wb").write(make_opus_ogg(head, frames[1], pre_skip, target + pre_skip))

        lf_wav = os.path.join(tmp, "lf_dec.wav")
        hf_wav = os.path.join(tmp, "hf_dec.wav")
        subprocess.run([XHEDEC, m4a, lf_wav], check=True, capture_output=True)
        subprocess.run([OPUSDEC, "--quiet", "--force-wav", ogg, hf_wav], check=True, capture_output=True)

        lf, lrate = sf.read(lf_wav, dtype="float64", always_2d=True)
        if lrate != SR:
            lf = soxr.resample(lf, lrate, SR, quality="HQ")
        hp, _ = sf.read(hf_wav, dtype="float64", always_2d=True)
        lf = lf / PAD
        hp = hp / PAD

        refm = ref.mean(axis=1)
        lag_l = xcorr_lag(refm, lf.mean(axis=1), SR)[0]
        lag_h = xcorr_lag(refm, hp.mean(axis=1), SR)[0]
        assert abs(lag_l - (-warmup48)) <= 8, f"LF lag {lag_l} vs {-warmup48}"
        assert abs(lag_h) <= 8, f"HF lag {lag_h}"

        def appl(x, lag):
            return x[lag:] if lag >= 0 else x[-lag:]

        lfs, hps = appl(lf, lag_l), appl(hp, lag_h)
        n = min(len(lfs), len(hps), target)
        assert n == target, f"short: {n} vs {target}"
        sena = (lfs[:n] + hps[:n])
        corr = float(np.corrcoef(refm[::8], sena.mean(axis=1)[::8])[0, 1])
        write_pcm_wav(f"{out_prefix}-ref32.wav", sena, 32)
        write_pcm_wav(f"{out_prefix}-ref16.wav", sena, 16)
        print(
            f"{basename(sena_path)}: profile={profile} lf_rate={lf_rate} warmup48={warmup48} "
            f"lags=({lag_l},{lag_h}) n={n} corr={corr:.5f}"
        )
    finally:
        import shutil

        shutil.rmtree(tmp, ignore_errors=True)


def basename(p):
    return os.path.basename(p)


if __name__ == "__main__":
    main()
