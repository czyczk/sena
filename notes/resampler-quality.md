# Resampler quality assessment (sena_dsp::Resampler vs soxr)

Measured 2026-09-01 on the product zero-phase windowed-sinc resampler
(Kaiser beta = 9, taps = ceil(12 / normalized transition), see
`crates/sena-dsp/src/lib.rs`). Method: `crates/sena-dsp/examples/resample_probe`
resamples probe signals; `tools/resample_quality.py` analyzes them and
compares against python `soxr` (1.1.0, quality="HQ") - the same resampler
class the archived reference assets used.

## Metrics (per rate pair)

| rates | passband ripple | stopband / image | sine error | vs soxr HQ (passband) |
|---|---|---|---|---|
| 44100 -> 48000 | 0.0000 dB | -122.7 dB | -121.0 dB | -104.6 dB |
| 48000 -> 16000 | 0.0001 dB | see aliasing | -111.6 dB | < -108 dB (see note) |
| 48000 -> 32000 | 0.0000 dB | see aliasing | -125.1 dB | -110.8 dB |
| 16000 -> 48000 | 0.0000 dB | -121.5 dB | -120.8 dB | -95.0 dB |

- Passband ripple: measured at 9 points (0.05..0.45 x min-nyquist): max
  deviation <= 0.0001 dB - flat.
- Sine error at 0.4 x min-nyquist: -111..-125 dB (= interpolation at f64
  precision; the windowed-sinc interpolates the bandlimited signal
  essentially exactly).
- Stopband / image rejection (upsampling): -121..-123 dB (Kaiser beta 9
  sidelobe floor).
- Aliasing (downsampling), tone above output nyquist folded:
  48k->16k: -118.3 dB; 48k->32k: -124.8 dB; 44.1k->48k image: -133.6 dB.
  (soxr HQ achieves -276 dB on the same test.)
- vs soxr HQ: for passband content the two agree to -95..-110 dB RMS
  (same design class, windowed-sinc, zero-phase-aligned). The only
  structural difference is the transition shape near the band edge
  (e.g. at 7500 Hz of a 16k stream: ours -0.48 dB vs soxr -0.41 dB) and
  the absolute stopband depth.

## Verdict

No need to port soxr to Rust. For every rate pair the resampler has:
0.000 dB passband ripple, < -111 dB sine error, < -118 dB aliasing /
image rejection. The one place soxr is measurably better (stopband depth
-276 dB vs -118 dB) is ~20 dB below the 16-bit quantization floor and
~50 dB below the artifacts of the lossy codecs (xHE-AAC / Opus at
16-32 kbps) that follow the resampler in the pipeline - it cannot be
heard in a Sena decode. A Rust port of soxr would cost significant
porting/maintenance work for no measurable product change.

End-to-end corroboration: the archived reference pipeline (soxr HQ +
libxaac + opusdec) validation of `assets/e2e` passed with zero lag
and LF envelope sigma within 0.02 dB of the references for all six
assets, and decoded-vs-source correlation > 0.99 (both measured with
the product resampler).
