# Encoder pipeline analysis (architecture, measurements, optimization space)

Measured 2026-09-01 with `SENAENC_TIME=1` (env-gated per-stage timing in
`crates/sena-enc/src/lib.rs`), 4-minute stereo inputs, release build, on
the 12-core WSL ARM64 dev box (single-channel figures; a Windows x86-64
desktop with more cores is faster overall).

## Architecture (current)

```
stdin/file WAV --WavStream--> [normalize any rate -> 48 kHz]
                                   StreamResampler (direct polyphase, threaded)
                                    |
                                    v
                             CrossoverStream  (split low/high, per-chunk FFT, cached plan)
                                    | low (+PRE_GAIN)
                                    v
                             LF downsample 48k -> 16k/32k (StreamResampler)
                                    |                          \ high (+PRE_GAIN)
                                    v                           v
                                lf.wav (s16, streaming)    hf.wav (f32, streaming)
                                    |                           |
                        (EOF)  ------+---------------------------+------>
                        run exhale (lf.wav)  ||  opusenc (hf.wav)   <- run in parallel
                                    |
                        extract AUs/packets -> mux -> .sena
```

- Streaming end to end: the input is consumed chunk by chunk and the temp
  WAVs are written as the DSP runs, so a feeding host (foobar2000
  converter) sees its progress bar advance with the real work
  (backpressure via the stdin pipe). Memory is bounded (~chunk size).
- The two BANDS are computed together in one convolution (high = input -
  low), so "LF/HF in parallel" is not how the split works; the split is
  already the minimum work. What can parallelize: the DSP stages across
  threads and the two codec processes.
- The cross-domain note in the question: the HF band is already at 48 kHz
  after normalization, so Opus gets 48 kHz directly - no HF resampling
  exists and none is needed (Opus' own resampler would only add loss).

## Measured breakdown (4-minute songs, 12-core ARM)

Before the 2026-09-01 optimization pass (all stages single-threaded):

| stage | 44.1 kHz @300 | 48 kHz @600 |
|---|---|---|
| normalize | 50.4 s | 7.6 s |
| split | 4.0 s | 2.1 s |
| lf-down | 10.1 s | 38.3 s |
| exhale | 5.0 s | 2.6 s |
| opusenc | 3.0 s | 1.9 s |
| **total** | **72.6 s** | **52.7 s** |

After: threaded streaming emit, identity fast path for 48 kHz input,
precomputed contiguous phase kernels (vectorizable dots), cached FFT
plan/kernel per chunk in the crossover, and exhale+opus encoders run in
parallel:

| stage | 44.1 kHz @300 | 48 kHz @600 |
|---|---|---|
| normalize | 1.9 s | 0.32 s |
| split | 2.7 s | 1.6 s |
| lf-down | 0.93 s | 4.5 s |
| exhale + opus (parallel) | ~4.9 s | ~2.4 s |
| **total** | **~12.7 s** | **~11.9 s** |

(Old codec stage ~8 s / ~4.5 s sequential; total ~5.7x / ~4.4x faster.)

## Remaining optimization space (measured, not speculative)

1. Streaming LF-down 48k->32k is ~2.4x slower than the batch FFT path for
   the same ratio (streaming n<=8 uses the direct per-sample path; batch
   uses FFT blocks). Fixing this (streaming FFT with guard state for
   n<=8) would cut ~2.5 s off the p600 total - the largest single
   remaining item. Medium complexity, correctness needs the same
   equivalence tests as the direct path.
2. Split per-chunk FFT: plan+kernel spectrum are now cached; remaining
   cost is the FFTs themselves (1.6-2.7 s). Overlap-save instead of
   whole-window convolution would roughly halve it. Low priority.
3. The direct polyphase loops are scalar f64 (no SIMD enabled): unrolled
   8-accumulator loops gave 2-4x; explicit SIMD (or `target-cpu=native`
   for local builds) would give more on x86-64. Building with
   `-C target-cpu=native`/portable `x86-64-v3` is the cheapest win for
   the Windows release.
4. opusenc could be fed over a pipe while the HF band is still being
   computed (overlapping codec with DSP); exhale needs a file, so the
   real win is only the parallelism already implemented (#exhale||opus).

## How to measure (scientific method)

- `SENAENC_TIME=1 senaenc ...` prints the per-stage wall time at the end
  of every encode; stages are instrumented at the call sites.
- `cargo run --release -p sena-dsp --example bench_pipeline [seconds]`
  times each stage in isolation, batch vs streaming.
- `cargo run --release -p sena-dsp --example resample_probe` +
  `tools/resample_quality.py` = resampler quality suite (ripple,
  stopband, aliasing, vs soxr HQ).
- For regressions: `cargo test --workspace` includes the
  stream-vs-batch DSP equivalence tests (bit-identical for the direct
  path).
