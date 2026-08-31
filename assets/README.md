# Sena end-to-end assets (assets/e2e)

Self-contained encode/decode fixtures for end-to-end verification of
senaenc/senadec: 3 source tracks × 2 profiles × 20 s clips, plus the
reference decode of each `.sena` in 16-bit and 32-bit float.

## Layout

```
assets/e2e/
  <id>__src.flac                       # 20 s source clip (48 kHz stereo, t=0..20)
  <id>__p<profile>__lf<letter>__hf<n>.sena   # the encode (6 files)
  <id>__p<profile>__lf<letter>__hf<n>-ref16.flac  # reference decode, 16-bit
  <id>__ref32.7z                       # reference decode, 32-bit float WAVs
                                       #   (extract: 7z x <id>__ref32.7z)
assets/tools/decode_ref.py             # .sena -> reference WAVs (see below)
```

`<id>` ∈ 01 / 05 / 09 (no source names are embedded anywhere). Every
file carries exactly 960,000 playable samples (20.0 s @ 48 kHz).

## Naming legend

| field | meaning | values used here |
|---|---|---|
| `<profile>` | crossover / profile number | `p300` = 300 Hz, `p600` = 600 Hz |
| `lf<letter>` | LF band target class | `lfa` = 20 kb/s class (p300), `lfd` = 32 kb/s class (p600) |
| `hf<n>` | HF (Opus) nominal bitrate | `hf144` (p300), `hf136` (p600) |

The LF letter is a bitrate-class label, not the accounted deduction (the
encoder's accounting deducts 16 kb/s for p300 and 24 kb/s for p600 from
the 160 kb/s total; actual LF spend lands in the class named by the letter).

## Profiles & bitrate

| Profile | crossover | LF encoder | LF stream rate | warmup (48k samples) | xHE deduction | Opus nominal |
|---|---|---|---|---|---|---|
| p300 (`lfa`) | 300 Hz | xHE-AAC non-eSBR preset 1 | 16 kHz | 3072 | 16 kbit/s | 144 kbit/s |
| p600 (`lfd`) | 600 Hz | xHE-AAC non-eSBR preset 5 | 32 kHz | 1536 | 24 kbit/s | 136 kbit/s |

Encode command: `senaenc --profile <300|600> --opus-senav 160 <src.wav> <out.sena>`.

## Reference decode (decode_ref.py)

This pipeline is archived documentation of how the reference WAVs were
produced. The product decoder does not need to reproduce this pipeline
(soxr + AOSP libxaac + opusdec) internally, and its output is not required
to match these references bit-exactly; any non-bit-exact delta must be
attributed to an acceptable documented cause before release.

Fully container-driven — no dependency on the encoder's workdir:

1. demux the Matroska container (tracks, CodecPrivate, frames, tags);
2. rebuild the xHE-AAC M4A (ASC + AUs) and the Ogg Opus stream
   (OpusHead + packets, Ogg CRC);
3. decode with the reference cores `xhedec` (AOSP libxaac wrapper) and
   `opusdec` (paths via `XHEDEC`/`OPUSDEC` env vars or PATH);
   up-sample the LF band to 48 kHz (soxr HQ); restore the -4 dB pad;
4. trim the leading warmup by one 1024-sample core frame at the actual LF
   stream rate (3072 / 1536 @ 48 kHz, cross-checked by correlation,
   ±8 samples);
5. sum the two bands and truncate to `SENA_PLAYABLE_SAMPLES` (the
   container's authoritative playable length; asserted equal to the
   source length);
6. write 32-bit float and 16-bit WAVs (the 16-bit one is produced by
   deterministic round-to-nearest + clip, no dither; the FLAC copies are
   lossless versions of those WAVs).

## Verification (all assets)

The table below is the archived reference pipeline's result. The product
decoder is not required to match it bit-exactly; the acceptance criterion
is: same length/lag, source correlation > 0.99, and LF envelope-sigma
within 0.02 dB of the reference value for each asset. Measured with the
first Rust `senadec` implementation (zero-phase `sena_dsp::Resampler`), all
six assets passed; the float32 output sits within -76.6 to -77.1 dB RMS of
the archived 32-bit references (difference attributable to resampler
design and float paths).


| asset | lags (LF, HF) | n | corr |
|---|---|---|---|
| 01__p300__lfa__hf144 | (-3072, 0) | 960000 | 0.9998 |
| 01__p600__lfd__hf136 | (-1536, 0) | 960000 | 0.9998 |
| 05__p300__lfa__hf144 | (-3072, 0) | 960000 | 0.9970 |
| 05__p600__lfd__hf136 | (-1536, 0) | 960000 | 0.9967 |
| 09__p300__lfa__hf144 | (-3072, 0) | 960000 | 0.9992 |
| 09__p600__lfd__hf136 | (-1536, 0) | 960000 | 0.9993 |

## Known issues

1. **Trailing trim is container-driven**: `SENA_PLAYABLE_SAMPLES` is the
   only authoritative end-of-stream length (see the container/decoder
   specs); any decoder must truncate to it. Older .sena files without the
   tag are out of date.
2. **The 2-byte exhale mdat preamble bug**: fixed in sena-enc
   (find_aus skips the 2-byte "informative" mdat preamble); re-encode any
   older artifacts.
3. Git size: the `*__ref32.7z` archives are ~13 MB each because float32
   PCM does not compress. If too heavy: git-lfs the archives, or drop the
   32-bit refs and regenerate them with `decode_ref.py` when needed.

## Notes

- The encoder normalizes any input sample rate (44.1/48/96 kHz etc.) to
  48 kHz with a zero-phase rational resampler (`sena_dsp::Resampler`)
  before the crossover; the LF band is resampled with the same component.
  There is no fractional group delay: the decoded LF band aligns exactly
  (lags -3072/-1536) and LF envelope-sigma is in the reference class
  (0.03-0.06).

## Regeneration

```bash
# prerequisites: senaenc (this repo), exhale >= 1.2.2 and opusenc/opusenc-senav
# next to the binary; xhedec and opusdec (XHEDEC/OPUSDEC env); 7z and flac for
# packaging; python3 + numpy + soundfile + soxr.
senaenc --profile 300 --opus-senav 160 <id>__src.wav <id>__p300__lfa__hf144.sena
senaenc --profile 600 --opus-senav 160 <id>__src.wav <id>__p600__lfd__hf136.sena
python3 assets/tools/decode_ref.py <sena> <src.wav> <out_prefix>   # -> -ref32.wav / -ref16.wav
```
