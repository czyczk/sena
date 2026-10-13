# The LF timeline model (definitive reference)

This note records, once and for all, how xHE-AAC LF AUs map onto the
playable 48 kHz timeline in Sena (v2 ES and any future v3), what the
correct anchor arithmetic is, and the empirical evidence behind it. It
exists because this model was derived twice from scratch during the v2
work (with contradictory intermediate conclusions — "one AU early" vs
"one AU late" — before converging on the statements below, all of which
are validated bit-exactly against the independent reference decode).

## 1. Coverage model (the authority)

- One LF AU = 1024 samples at the stream rate (16 kHz @300, 32 kHz @600).
- **AU 0** (stream start) decodes to 2048 samples = preroll(1024) +
  main(1024); it covers playable **[-1024, 1024)** core samples. The
  1024-sample "warmup" that every decode trims *is* this preroll.
- **AU k (k ≥ 1)** covers playable **[(k-1)*1024, k*1024)** core samples
  ("model A").
- Raw↔playable mapping is a constant shift: `playable_core = raw_core -
  1024` (the warmup trim). Total decoded span is `AUs*1024` core; after
  the trim it is `AUs*1024 - 1024`, which must be ≥ the playable length
  (the encoder guarantees this; the decoder fails rather than pads).
- A **fresh** decoder started mid-stream at an independency AU emits
  exactly 1024 per AU — *except* the first preroll AU of an independency
  pair (see §3).

"Model B" you may meet in old code/comments ("AU k covers
[k*1024, (k+1)*1024)") is the *emission-slot* view of the same stream
when you start counting raw positions at the stream head (AU0's extra
1024 preroll pushes every later AU one slot). Model A is the view that
matters for *anchors and placement*, because the placement rule (§2) is
by playable END. The two reconcile through the constant −1024 shift:
an AU whose model-A window is [(k-1)*1024, k*1024) sits at raw slot
[(k+1)*1024, (k+2)*1024), i.e. model-B [k*1024, (k+1)*1024) playable.
Bit accounting keeps both views: infos are anchored on the raw grid
(`lf_total_before = k*1024` at a jump) and map to playable with the
−1024 trim; anchors (below) are on the playable grid.

## 2. Placement in the ES block grid (by playable END)

One SimpleBlock = one 20 ms slot (960 frames). Placement is by playable
END, clamped to the final block:

- LF AU k → block `floor((k*unit48 - 1) / 960)`, `unit48 = 1024 * 48000 /
  lf_rate` (3072 @16 kHz, 1536 @32 kHz), clamped to `n_blocks - 1`.
- Top-layer Opus packet j → block `j` exactly (pre-skip < 960 is enforced
  at INIT validation).
- Mid Opus packet i → block `i` by definition of the grid.

Why by-END: it puts the AU covering any block boundary *inside* that
block, so a demuxer-cued cold start never sees an LF hole. (By-start
placement, tried first, leaves up to a 64 ms hole plus a block-0
double-AU special case.) The EOS clamp means the final block can carry
up to 2+ AUs — hence the block format's `lf_count` is a counted list
(0..4), not presence flags.

## 3. Independency pairs and the preroll duplicate

exhale marks independency in pairs (e.g. AUs 29,30; 44,45): an IPF AU
plus an AudioPreroll AU (the pair's second). On a decoder with history,
the preroll AU emits only its 1024 main (preroll data skipped as
redundant). On a **fresh** decoder (seek/ramp start):

- the pair-first IPF emits 1024 (its main),
- the pair-second then emits **2048** = a preroll duplicate of the
  *previous window's content* (verified bit-close, max 2.87e-07 vs the
  previous AU's decoded window) + its own 1024 main.

Naive handling (play the 2048 sequentially) delays the whole LF by one
AU permanently — this was the "one AU late" failure class. Correct
handling: anchor the fresh start's first AU at its **model-A start**
(`(k-1)*unit48`, i.e. one AU before the emission-slot position). The
first main then lands one AU early — a 64 ms join artifact inside the
ramp transient — and the preroll duplicate that follows lands exactly on
the correct slot, so everything after the duplicate is aligned
("self-healing" at pair-first anchors). Container seeks prefer
pair-second anchors (or back off one extra independency for resampler
context), where the uniform 1024-per-AU trim arithmetic swallows the
duplicate exactly.

## 4. The anchor formula and the join rule

Given a block timestamp `ts` and the AU's absolute index `k` in it:

```
unit48 = 1024 * 48000 / lf_rate
au_start48(k) = ts48 + ((k*unit48 - 1) mod 960 + 1) - unit48     (k >= 1)
au_start48(0) = -unit48
```

`ts48 = ts_ns / 1e6 * 48`. This reconstructs the AU's model-A grid start
from (block ts, au_index) alone and is shift-invariant: it works for
absolute timestamps (LAV) and for cue-shifted timestamps (CLI -ss
tsfixup). Do **not** "simplify" it to `ts48 + r` (that is the emission
slot, one AU late with a duplicate following).

Ramp join (cold start, LF muted until first independency AU): compare
`au_start48` against the **current production point**
(`tf_seek_base + produced`), never against the stream start. If the AU
covers it → decode with trim (context kept); if it starts in the future
→ silence until then (chunks are capped at the join). Comparing against
the stream start instead once landed the whole LF lane ~1 chunk late
permanently (peak diff 1.6).

## 5. The other "fraction of a frame" grid (the "2/3" you half-remember)

Two independent grid facts, don't conflate:

- **Resampler decimation grid.** 16k→48k is ×3 (decimation m=1), 32k→48k
  is ×3/2 (m=2). The LF resampler window drain must stay a multiple of
  m; an off-grid drain shifts all later LF output by a fraction of a
  frame against the whole-file decode (v1-era bug, guarded by
  `Resampler::decimation()` in the drain path).
- **AU/block phase.** AU periods are 3.2 blocks (@300) and 1.6 blocks
  (@600): placement phases repeat every 5 AUs. Nothing in the decoder
  may assume one AU per block — hence counted lists everywhere.

## 6. Empirical anchors (re-derive any change against these)

- `rxaac` fresh-from-AU29 vs continuous-from-0 (same slot): max 2.1e-3
  over the whole file — the codec itself is fine at fresh starts.
- preroll duplicate vs previous AU's window: max 2.87e-07.
- Continuous decode AU widths: 1024 everywhere except AU0 = 2048.
  Fresh-from-29 widths: [1024, 2048, 1024, ...].
- `abs750`/`abs050` (ES cold starts) vs continuous decode: ≤ 2e-5 after
  the ramp settle (≈1 s), join transient localized at the join.
- v1↔v2 remux fixtures decode bit-exactly (7/7); container seeks
  bit-exact at multiple targets; warmup seeks bit-exact vs container
  seeks.

If a future change breaks one of these, it broke the model, not the
test.

## 7. Mid-stream diverged-state preroll emissions (2026-10 seek saga)

§3's pair-second preroll is not the only 2048 emission. **Any independency
AU can emit 2048 mid-stream when the decoder's state trajectory has
diverged from the continuous one** (i.e. after a restart whose transient
has not fully reconverged). Observed: 30-170 AUs after a seek anchor, on
both lanes, on real music. The extra 1024 samples entering a lane's queue
shift every later sample one AU off the playable timeline *permanently and
silently* (later AUs stay bit-identical, so per-AU content checks do not
catch it — only timeline bookkeeping does).

**Lane-opposite layouts, both verified by direct comparison against the
continuous decode:**

- LF lane (exhale-encoded): `[content 1024][extra 1024]` — drop the TAIL.
  (Contrast §3's fresh-start pair-second, whose duplicate comes FIRST;
  the two cases are both real, order differs — verify locally, never
  assume.)
- xHE mid lane (FhG-IIS encoded): `[preroll 1024][content 1024]` — drop
  the HEAD.

Normalization lives in `decode_lf_au` / `decode_mx_au`, gated on the
seek-anchored path (`lf_trim_preroll_assumed` / `mx.trim_preroll_assumed`).
The push cold-start's coverage bookkeeping predates this normalization and
keeps legacy behavior until separately audited.

## 8. The 44.1 kHz mid lane's phase grid (147/160)

The mid resampler (44.1k→48k, m=147/n=160) anchors its phase at the window
start, so the window base's *content* position must stay ≡ 0 (mod 147) or
every later output shifts by a fraction of a sample vs the whole-file
decode (a 1024-sample base slip = 5/147-sample phase error = permanent
mid-band decorrelation at corr ~0.5). Drain history alone cannot hold this
across seeks (a clamped drain slips by one AU); `mx_window` recomputes the
residue every chunk from `total_appended + content_shift` (anchor-class
dependent: 2048-class restarts start one AU earlier) and shifts the window
front accordingly. On the whole-file trajectory the correction is
identically zero (golden suite is the witness). The drain keeps one extra
grid step of context so the correction never undercuts kernel support.

## 9. Seek settle windows (2026-10)

Restart transients are anchor-content-dependent and lane-specific:

- LF (USAC @32k): worst measured ~2.0 s (bass band fully decorrelated);
  bare-rxaac restarts reconverge bit-exactly by ≤8 s. seek() anchors the
  LF lane **4 s** of core content before the target.
- xHE mid (@44.1k): transient mild (band corr ~0.98 immediately); settle
  **88 AUs (~2.0 s)**, chosen so a §7 preroll event at the first indep AU
  after the anchor (max indep gap 86 AUs) lands inside the silent window.
- Top (Opus): converges within one packet; no settle.

warmup_seek shares seek() via its shadow store, so push/pull stay
bit-identical (the warmup_seek_matches_container_seek test gates this).
Cost: ~45-70 ms extra decode per seek (native release).

Known residual: at targets with target ≢ 0 (mod 160) the mid lane can sit
at a constant sub-sample fractional offset vs the continuous decode
(measured 0.58 samples @44.1k; pure delay — linear phase, 0.04 rad
residual) because the seek trim quantizes to integer 44.1k samples while
the continuous abs0 grid is 160-locked. Perceptually transparent (no
smearing/echo); fixing requires regridding the chunk lattice — documented
as a bound, not done.
