#!/usr/bin/env python3
"""Scientific resampler quality comparison: sena_dsp::Resampler vs soxr HQ.

Measures, for each rate pair:
  1. passband ripple (dB): sine amplitude deviation 0.05..0.45 x min-nyquist;
  2. stopband attenuation (dB): output energy above 0.5 x min(in,out) rate;
  3. sine error (RMS, dB) at 0.4 x min-nyquist;
  4. delta vs soxr HQ on the same input (dB, lag-aligned on a chirp).
"""
import math
import numpy as np
import soxr
import subprocess
import sys

PROBE = sys.argv[1] if len(sys.argv) > 1 else "target/release/examples/resample_probe"
RATES = [(44100, 48000), (48000, 16000), (48000, 32000), (16000, 48000)]
RNG = np.random.default_rng(1234)


def run_probe(x, fs_in, fs_out):
    p = subprocess.run([PROBE, str(fs_in), str(fs_out)], input=x.astype("<f8").tobytes(),
                       stdout=subprocess.PIPE, check=True)
    return np.frombuffer(p.stdout, dtype="<f8")


def sine(f, fs, n):
    return np.sin(2 * np.pi * f * np.arange(n) / fs).astype(np.float64)


def amp_at(f, fs_in, fs_out, n_sec=3.0):
    """Amplitude of the resampled sine at f (same frequency at both rates)."""
    n = int(fs_in * n_sec)
    x = sine(f, fs_in, n)
    y = run_probe(x, fs_in, fs_out)
    want = sine(f, fs_out, len(y))
    a = int(0.05 * len(y)); b = int(0.95 * len(y))
    seg_a = y[a:b]; seg_w = want[a:b]
    return np.dot(seg_a, seg_w) / np.dot(seg_w, seg_w)


def stopband_att(fs_in, fs_out, n_sec=6.0):
    n = int(fs_in * n_sec)
    x = RNG.standard_normal(n) * 0.1
    y = run_probe(x, fs_in, fs_out)
    a = int(0.05 * len(y)); b = int(0.95 * len(y))
    y = y[a:b]
    w = np.hanning(len(y))
    spec = np.abs(np.fft.rfft(y * w)) ** 2
    freqs = np.fft.rfftfreq(len(y), 1 / fs_out)
    cutoff = 0.5 * min(fs_in, fs_out)
    pass_band = (freqs < 0.4 * min(fs_in, fs_out))
    stop_band = (freqs > cutoff * 1.02) & (freqs < 0.49 * fs_out)
    if not pass_band.any() or not stop_band.any():
        return float("nan")
    return 10 * math.log10(spec[stop_band].sum() / spec[pass_band].sum())


def chirp(fs_in, n_sec=6.0):
    t = np.arange(int(fs_in * n_sec)) / fs_in
    f = 300 + (0.38 * fs_in / 2 - 300) * t / t[-1]  # stay inside the passband
    return np.sin(2 * np.pi * np.cumsum(f) / fs_in).astype(np.float64)


def lag_align(a, b, win=65536):
    """Best lag of b relative to a (b shifted right by lag aligns with a)."""
    n = min(len(a), len(b), win)
    best_lag, best_corr = 0, -1.0
    for lag in range(0, n, 64):
        c = np.corrcoef(a[lag:lag + n - 512:257], b[lag:lag + n - 512:257])[0, 1]
        if c > best_corr:
            best_corr, best_lag = c, lag
    lo = max(0, best_lag - 64); hi = min(n - 1, best_lag + 64)
    for lag in range(lo, hi):
        c = np.corrcoef(a[lag:lag + n - 512:257], b[lag:lag + n - 512:257])[0, 1]
        if c > best_corr:
            best_corr, best_lag = c, lag
    return best_lag


def soxr_delay(fs_in, fs_out):
    # soxr has its own filter latency; compute it exactly with a delta probe.
    x = np.zeros(int(fs_in * 2.0)); x[int(fs_in * 0.5)] = 1.0
    y = soxr.resample(x, fs_in, fs_out, quality="HQ")
    out_at = int(fs_in * 0.5) * fs_out / fs_in
    return int(np.argmax(y)) - int(round(out_at))


def vs_soxr(fs_in, fs_out, n_sec=6.0):
    x = chirp(fs_in, n_sec)
    y_ours = run_probe(x, fs_in, fs_out)
    y_soxr = soxr.resample(x, fs_in, fs_out, quality="HQ")
    n = min(len(y_ours), len(y_soxr))
    lag = soxr_delay(fs_in, fs_out)
    a = y_ours[lag:lag + n - 4096]
    b = y_soxr[lag:lag + n - 4096]
    rmse = np.sqrt(np.mean((a - b) ** 2))
    sig = np.sqrt(np.mean(a ** 2))
    corr = np.corrcoef(a[::7], b[::7])[0, 1]
    return (20 * math.log10(rmse / sig) if sig > 0 else float("nan")), corr, lag


print(f"{'rates':>14} | {'rip(dB)':>8} | {'stop(dB)':>8} | {'sine-err(dB)':>12} | {'vs-soxr(dB)':>11} | {'corr':>7} | {'lag':>5}")
for (fi, fo) in RATES:
    rips = []
    for frac in np.linspace(0.05, 0.45, 9):
        f = frac * min(fi, fo)
        a = amp_at(f, fi, fo)
        rips.append(20 * math.log10(abs(a)) if abs(a) > 1e-6 else float("nan"))
    rip = max(abs(v) for v in rips if not math.isnan(v)) if any(not math.isnan(v) for v in rips) else float("nan")
    stop = stopband_att(fi, fo)
    f = 0.4 * min(fi, fo)
    n = int(fi * 3)
    x = sine(f, fi, n)
    y = run_probe(x, fi, fo)
    ideal = sine(f, fo, len(y))
    a = int(0.05 * len(y)); b = int(0.95 * len(y))
    err = np.sqrt(np.mean((y[a:b] - ideal[a:b]) ** 2))
    sig = np.sqrt(np.mean(ideal[a:b] ** 2))
    err_db = 20 * math.log10(err / sig) if sig > 0 else float("nan")
    v, corr, lag = vs_soxr(fi, fo)
    print(f"{fi:>5}->{fo:<6} | {rip:8.4f} | {stop:8.2f} | {err_db:12.2f} | {v:11.2f} | {corr:7.5f} | {lag:5d}")
