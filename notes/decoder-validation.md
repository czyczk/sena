# senadec reference-asset validation notes

Run (Linux, rustc 1.97.1, simd feature):

```bash
cargo build -p senadec
senadec assets/e2e/<asset>.sena -o /tmp/<asset>.wav --format wav-f32
```

Compare against the archived reference pipeline outputs (soxr HQ + AOSP
libxaac + opusdec) in `assets/e2e/*-ref16.flac` and `*__ref32.7z`.

Measured 2026-08-31 with the first sena-dec implementation
(`sena_dsp::Resampler`, rxaac-dec-lib, opus-decoder/ropus simd):

| asset | length | lag @48k | ref LFσ dB | senadec LFσ dB | senadec-vs-ref32 RMS dB | max abs |
|---|---|---|---|---|---|---|
| 01 p300 | 960000 | 0 | 0.0366 | 0.0366 | -78.03 | 0.0007 |
| 01 p600 | 960000 | 0 | 0.0572 | 0.0572 | -78.63 | 0.0006 |
| 05 p300 | 960000 | 0 | 0.1037 | 0.1037 | -76.61 | 0.0009 |
| 05 p600 | 960000 | 0 | 0.1371 | 0.1372 | -77.10 | 0.0012 |
| 09 p300 | 960000 | 0 | 0.0702 | 0.0702 | -77.37 | 0.0010 |
| 09 p600 | 960000 | 0 | 0.0897 | 0.0898 | -77.94 | 0.0014 |

All assets pass: exact playable length, lag 0, source correlation > 0.99,
and per-asset LF envelope sigma matches the archived reference within
0.02 dB.

Non-bit-exactness is fully attributed:
- different LF resampler design (`sena_dsp::Resampler` zero-phase
  windowed-sinc vs soxr HQ in the archived reference);
- different float signal paths (rxaac-dec-lib / ropus vs AOSP libxaac
  `xhedec` / `opusdec`);
- 16-bit references additionally differ by the deterministic no-dither
  quantization policy of the archive.

The RMS delta of -76.6 to -78.6 dB against the archived 32-bit float
reference is codec/reference-class noise, not an implementation bug.

## Gapless sequence check (manual golden, 2026-08-31)

Split `01__src.flac` into two 480000-frame PCM16 WAVs, encoded each with
`senaenc --profile 300 --opus-original 160`, decoded each with `senadec
--format wav-f32`, concatenated:

- output lengths: 480000 + 480000 = 960000 (exact)
- lag vs source: 0 @48k
- correlation: 0.999757
- LF envelope sigma: 0.0391 dB

Encoder note: `find_aus` now uses the MP4 `stco` first chunk offset instead
of a hard-coded 2-byte mdat preamble. Some exhale outputs have a 3-byte
preamble, which previously shifted every AU by one byte and made rxaac-dec
panic/error. Fixed and re-verified above.
