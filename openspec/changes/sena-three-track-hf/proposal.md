# Change: Three-track HF layout (>=256k) and SenaV topband-stereo defaults

## Why
Opus bitrate allocation cannot target bands 19/20 specifically: raising the
nominal rate leaks into low/mid bands, and after intensity stereo exits the
top two bands lack quantization precision. A dedicated opus track for
b19/b20 (fixed 64 kbit/s, measured sufficient) bypasses the allocator
entirely. At lower rates (Opus budget < 192k) the opusenc-senav
`AUDIFF_TOPBAND_STEREO` floor is the right tool and should default on in
the tier where it applies.

## What Changes
- Three-track layout for nominal totals >= 256 kbit/s (both @300 and @600,
  both opus modes): exhale LF (deducted) + `A_OPUS` mid (600 Hz - 15.6 kHz,
  total - deduct - 64) + new `A_OPUSHF` top (15.6 kHz - 24 kHz, fixed
  64 kbit/s nominal, 20 ms frames).
- Second crossover at 15600 Hz (the Opus b19 edge at 48 kHz/20 ms):
  linear-phase FIR, Kaiser beta 9, steep 240 Hz transition, stopband at
  15600 Hz; subtractive complement, mid + top reconstructs the 600 Hz-high
  band exactly.
- Profile tags extend to `<lf>@<hf>` (`300@15600`, `600@15600`); plain
  `300`/`600` remain the two-track layout. `SENA_VERSION` stays 1.
- `A_OPUSHF`: codec id in the container, OpusHead private, 48 kHz stereo,
  own CodecDelay/pre-skip; audio SHA-256 domain v2 (3 streams) for
  three-track files (two-track keeps v1 bytes).
- Decode: whole-file pipeline and the streaming decoder handle three
  tracks concurrently (min-ready chunking, per-track pre-skip trims, seek
  by per-track packet index, bit accounting includes the third track).
- senaenc `--opus-topband-stereo <1-500>`: passes the value straight to
  the opusenc-senav mid encode (`AUDIFF_TOPBAND_STEREO`), no Sena-side
  arithmetic. Default `opus_kbps` when opus-senav is active and the total
  is in [192, 256). Ignored with a warning under `--opus-original`. Never
  applied to the `A_OPUSHF` track.

## Impact
- Decoder (Rust core, C ABI, foobar2000/ffmpeg plugins) must accept the
  third track and the extended profile tag; old decoders fail cleanly on
  the new tag value.
- Encoder pipeline gains a streaming second crossover and a third codec
  subprocess run in parallel.
