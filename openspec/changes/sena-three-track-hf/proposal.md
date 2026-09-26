# Change: Three-track HF layout (>=256k) and SenaV topband-stereo defaults

## Why
Opus bitrate allocation cannot target bands 19/20 specifically: raising the
nominal rate leaks into low/mid bands, and after intensity stereo exits the
top two bands lack quantization precision. A dedicated opus track for the
top band bypasses the allocator entirely. Directly coding the 15.6 kHz+
band does not work either: the codec's band allocation is content-blind, so
a track whose only content sits in the top octave starves (bits sink into
the empty low bands and the coded band set collapses). Shifting the top
band down to baseband (analytic-signal SSB shift by the split frequency)
puts the content where the codec serves it well, and the fixed 64 kbit/s
(measured sufficient) is then actually spent on it. At lower rates (Opus
budget < 192k) the opusenc-senav `AUDIFF_TOPBAND_STEREO` floor is the right
tool and should default on in the tier where it applies.

## What Changes
- Three-track layout for nominal totals >= 256 kbit/s (both @300 and @600,
  both opus modes): exhale LF (deducted) + `A_OPUS` mid (600 Hz - 15.6 kHz,
  total - deduct - 64) + new `A_OPUSHF` top (the 15.6 kHz+ band
  SSB-shifted down to baseband, 16 kHz stream, fixed 64 kbit/s nominal,
  20 ms frames).
- Second crossover at 15600 Hz (the Opus b19 edge at 48 kHz/20 ms):
  linear-phase FIR, Kaiser beta 9, steep 240 Hz transition, stopband at
  15600 Hz; subtractive complement, mid + top reconstructs the 600 Hz-high
  band exactly (before the top band's shift chain).
- Top-band shift: 8001-tap windowed Hilbert FIR (Kaiser beta 9) gives the
  quadrature component; down-shift by 15600 Hz at 48 kHz, zero-phase
  resample to 16 kHz for the encode. Decode reverses: decode at 16 kHz,
  zero-phase upsample back to 48 kHz, shift back up with the carrier phase
  locked to the playable timeline, then mix.
- Profile tags extend to `<lf>@<hf>` (`300@15600`, `600@15600`); plain
  `300`/`600` remain the two-track layout. `SENA_VERSION` stays 1.
- `A_OPUSHF`: codec id in the container, OpusHead private (16000 Hz), track
  rate 16000 Hz stereo, own CodecDelay/pre-skip (48 kHz units per RFC
  7845); audio SHA-256 domain v2 (3 streams) for three-track files
  (two-track keeps v1 bytes).
- Decode: whole-file pipeline and the streaming decoder handle three
  tracks concurrently (min-ready chunking, per-track pre-skip trims, seek
  by per-track packet index with a short context back-off for the
  upsample/shift chain, bit accounting includes the third track).
- senaenc `--opus-topband-stereo <1-500>`: passes the value straight to
  the opusenc-senav mid encode (`AUDIFF_TOPBAND_STEREO`), no Sena-side
  arithmetic. Default `opus_kbps` when opus-senav is active and the total
  is in [192, 256). Ignored with a warning under `--opus-original`. Never
  applied to the `A_OPUSHF` track.

## Impact
- Decoder (Rust core, C ABI, foobar2000/ffmpeg plugins) must accept the
  third track and the extended profile tag; old decoders fail cleanly on
  the new tag value, and files from the superseded direct-48 kHz variant
  are rejected by the A_OPUSHF rate check.
- Encoder pipeline gains a streaming second crossover, the shift/downsample
  chain, and a third codec subprocess run in parallel.
