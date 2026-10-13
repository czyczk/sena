//! Env-gated stage timing for pipeline analysis (SENAENC_TIME=1 prints the
//! per-stage CPU/wall breakdown at the end of an encode).

/// Per-stage wall-clock accumulators (seconds). All fields are public: the
/// pipeline folds ticks into them via [`end_tick`], which takes the tick
/// field and one accumulator as disjoint borrows.
#[derive(Default)]
pub struct StageTimes {
    pub(crate) on: bool,
    pub(crate) t: Option<std::time::Instant>,
    pub norm: f64,   // input rate -> 48k normalization
    pub split: f64,  // crossover split
    pub lf: f64,     // LF downsample 48k -> lf rate
    pub write: f64,  // band WAV IO (16/32-bit conversion + file write)
    pub check: f64,  // tool version checks
    pub exhale: f64, // xHE-AAC encode
    pub opus: f64,   // Opus encode (mid track; three-track layouts attribute
    // the shared codec wall time to both opus accumulators)
    pub opus_hf: f64, // Opus encode (A_OPUSHF top track, three-track only)
    pub mux: f64,     // packet extraction + container mux
}

impl StageTimes {
    pub fn new() -> Self {
        Self {
            on: std::env::var_os("SENAENC_TIME").is_some(),
            ..Default::default()
        }
    }

    pub fn begin(&mut self) {
        if self.on {
            self.t = Some(std::time::Instant::now());
        }
    }

    pub fn report(&self, playable: usize) {
        if !self.on {
            return;
        }
        let total = self.norm
            + self.split
            + self.lf
            + self.write
            + self.check
            + self.exhale
            + self.opus
            + self.opus_hf
            + self.mux;
        eprintln!(
            "SENAENC_TIME: playable={playable} frames\n\
             \x20 normalize {:.3}s  split {:.3}s  lf-down {:.3}s  write {:.3}s\n\
             \x20 check {:.3}s  exhale {:.3}s  opus {:.3}s  opus-hf {:.3}s  mux {:.3}s\n\
             \x20 summed {:.3}s",
            self.norm,
            self.split,
            self.lf,
            self.write,
            self.check,
            self.exhale,
            self.opus,
            self.opus_hf,
            self.mux,
            total
        );
    }
}

/// Fold a pending begin() tick into an accumulator. Takes the two disjoint
/// fields separately so call sites can borrow them without aliasing.
pub(crate) fn end_tick(tick: &mut Option<std::time::Instant>, acc: &mut f64) {
    if let Some(t) = tick.take() {
        *acc += t.elapsed().as_secs_f64();
    }
}
