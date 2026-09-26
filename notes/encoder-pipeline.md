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
                                     |                \ high
                                     v                  v
                              LF downsample       (two-track: 600 Hz..24 kHz -> hf.wav)
                              48k -> 16k/32k       (three-track, total >= 256k:
                                     |              CrossoverStream @15600 -> mid.wav
                                     |              + ShiftStream down 15.6 kHz -> baseband
                                     |              + 48k->16k -> hf.wav)
                                     v                           |
                                 lf.wav (s16, streaming)         v
                                     |                    mid.wav (48k f32) / hf.wav (16k f32)
                                     |
                     (EOF)  ------+---------------------------+------>
                     run exhale (lf.wav) || opusenc (mid) || opusenc (hf, 3-track only)
                                     |
                         extract AUs/packets -> mux -> .sena
```

- Two-track layout (`SENA_PROFILE` `300`/`600`): exhale LF + one opus track
  over 600 Hz..Nyquist at `total - deduct`.
- Three-track layout (nominal total >= 256 kbit/s, `SENA_PROFILE`
  `<lf>@15600`): the 600 Hz-high band splits again at the Opus b19 edge
  (15600 Hz; FIR_15600, 2001 taps, cutoff 15480, 240 Hz transition ->
  >= ~95 dB at 15600). The mid track (`A_OPUS`) codes 600 Hz..15.6 kHz at
  `total - deduct - 64`. The top band is not coded directly: an Opus track
  whose content sits only above 15.6 kHz starves under the codec's
  content-blind band allocation (the empty low bands keep their share), so
  the top band is SSB-shifted down to baseband (analytic signal via an
  8001-tap Hilbert FIR, Kaiser beta 9; carrier = the 15600 Hz split) and
  resampled 48k -> 16k; the top track (`A_OPUSHF`) codes that 16 kHz
  baseband at a fixed 64k nominal. Decoding reverses it: decode at 16 kHz,
  zero-phase upsample back to 48 kHz, shift back up (carrier phase locked
  to the playable timeline), mix. Both opus tracks use whichever opusenc
  build the mode selected (`--opus-senav` / `--opus-original`); the three
  codec processes run concurrently. `--hf-tilt`, when enabled, shapes the
  mid band only.
- opus-senav topband-stereo: `AUDIFF_TOPBAND_STEREO` is armed for the mid
  encode only - by default at the Opus budget when the total is in
  [192, 256) kbit/s with senav, or verbatim via
  `--opus-topband-stereo <1-500>` (straight passthrough, no Sena-side
  arithmetic; warned+ignored under `--opus-original`; never on `A_OPUSHF`).

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

## Parallelism map (2026-09-25)

What runs concurrently, end to end:

- DSP kernels are internally threaded across outputs/channels for large
  chunks (polyphase resampler emit, both crossover FFT fills; threshold
  16K frames/channels).
- Three-track chunks run the LF branch (downsample + LF write) and the HF
  branch (15600 Hz split + tilt + mid/top writes) on two scoped threads -
  the p600 LF downsample no longer serializes behind the second crossover.
  Two-track keeps the sequential order (its HF branch is a fraction of the
  LF branch, and SENAENC_TIME stage sums stay comparable with the tables
  above). With overlapped branches the three-track SENAENC_TIME sum can
  exceed the wall time.
- The codec subprocesses (exhale, opusenc mid, opusenc top) spawn together
  and each is drained on its own thread (a chatty child can no longer stall
  on a full pipe while an earlier one is waited on). Verified: the 3-track
  e2e asset is bit-identical whether the band branches run sequentially or
  concurrently.
- Whole-file decode (senadec CLI / validation) decodes the tracks
  concurrently (LF / mid / top on scoped threads).

Deliberately not parallelized:

- Codec-vs-DSP overlap (pipe-feeding opusenc while the bands stream): the
  encoders read complete WAVs; exhale needs a file anyway, so the win is
  bounded (~the opusenc wall time). Same call as the 2026-09-01 pass.
- The streaming product decoder stays single-threaded: tracks are consumed
  interleaved (no track waits for another, min-ready chunking), decode runs
  far ahead of realtime, and plugin hosts give the decode thread a 544 KiB
  stack where worker threads are not welcome.

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
