//! FIR crossover against the reference coefficient tables: batch (`split`)
//! and streaming (`CrossoverStream`) forms, subtractive complement.

use crate::conv::{FftConvolver, fftconvolve_interleaved};
use crate::fir_tables;
use crate::{PAR_MIN_FRAMES, worker_threads};

/// Reference FIR for a split point. 300/600 are the profile crossovers;
/// 12000 is the three-track b19 split (cutoff 12120: passband through the
/// 12000 carrier so the top branch is empty below it - SSB shift-down folds
/// any below-carrier top content, phase-flipped, onto 12.0-12.24 kHz).
pub(crate) fn fir_for(fc: f64) -> &'static [f64] {
    if (fc - 300.0).abs() < 1.0 {
        &fir_tables::FIR_300
    } else if (fc - 600.0).abs() < 1.0 {
        &fir_tables::FIR_600
    } else if (fc - 12000.0).abs() < 1.0 {
        &fir_tables::FIR_12000
    } else {
        panic!("no reference FIR for split at {fc} Hz");
    }
}

/// Split stereo f64 samples into low/high bands with the reference FIR.
/// Returns (low, high); both delayed by (N-1)/2 relative to input.
pub fn split(x: &[f64], ch: usize, fc: f64) -> (Vec<f64>, Vec<f64>) {
    let h: &[f64] = fir_for(fc);
    let n = x.len() / ch;
    let mut low = fftconvolve_interleaved(x, h, ch, n);
    // high = delayed x - low
    let d = (h.len() - 1) / 2;
    let mut high = vec![0.0; n * ch];
    for c in 0..ch {
        for i in 0..n {
            let xi = x[i * ch + c];
            high[i * ch + c] = xi - low[(i + d) * ch + c];
        }
    }
    // crop the FIR delay from low (the kept frames are contiguous)
    low.drain(..d * ch);
    low.truncate(n * ch);
    (low, high)
}

/// Streaming chunk-fed zero-phase FIR crossover (subtractive complement).
///
/// Matches `split` output values: low = convolution of the input with the
/// reference FIR (delay removed), high = input minus low. Output latency is
/// `(N-1)/2` input frames (the FIR half-width).
pub struct CrossoverStream {
    h: &'static [f64],
    d: usize,
    win: Vec<f64>, // interleaved, starting at global frame `win_start`
    win_start: usize,
    emitted: usize,
    ch: usize,
    finishing: bool,
    convolver: FftConvolver,
}

impl CrossoverStream {
    pub fn new(fc: f64) -> Self {
        let h: &'static [f64] = fir_for(fc);
        Self {
            h,
            d: (h.len() - 1) / 2,
            win: Vec::new(),
            win_start: 0,
            emitted: 0,
            ch: 0,
            finishing: false,
            convolver: FftConvolver::new(),
        }
    }

    /// Feed interleaved input frames; returns (low, high) interleaved
    /// output frames that are fully settled (may be empty).
    pub fn push(&mut self, x: &[f64], ch: usize) -> (Vec<f64>, Vec<f64>) {
        assert!(!self.finishing, "push after finish");
        assert!(x.len() % ch == 0, "input not interleaved at {ch} channels");
        if self.ch == 0 {
            self.ch = ch;
        } else {
            assert_eq!(self.ch, ch, "channel count changed");
        }
        self.win.extend_from_slice(x);
        self.emit(ch)
    }

    /// Flush the remaining latency (zero padding after the final input
    /// sample), returning the settled (low, high) frames.
    pub fn finish(&mut self, ch: usize) -> (Vec<f64>, Vec<f64>) {
        assert!(!self.finishing, "finish called twice");
        assert_eq!(self.ch, ch, "channel count changed");
        self.finishing = true;
        self.emit(ch)
    }

    fn emit(&mut self, ch: usize) -> (Vec<f64>, Vec<f64>) {
        // While streaming, emit only while i + d is inside the window: the
        // support [i-d, i+d] is fully covered (at the stream start, frames
        // before 0 stay zero inside the window convolution, matching the
        // batch zero padding). On finish, emit everything: the right side of
        // the window is zero-padded, which also matches the batch behavior.
        let end = self.win_start + self.win.len() / ch;
        let cnt = if self.finishing {
            end.saturating_sub(self.emitted)
        } else {
            end.saturating_sub(self.d).saturating_sub(self.emitted)
        };
        if cnt == 0 {
            return (Vec::new(), Vec::new());
        }
        let n = self.win.len() / ch;
        let conv = self.convolver.convolve_interleaved(&self.win, self.h, ch, n);
        let mut low = vec![0.0; cnt * ch];
        let mut high = vec![0.0; cnt * ch];
        // Every output is independent: fill in parallel for large chunks.
        let threads = worker_threads(usize::MAX);
        if threads > 1 && cnt >= PAR_MIN_FRAMES {
            let chunk = cnt.div_ceil(threads);
            std::thread::scope(|s| {
                for (idx, (low_p, high_p)) in low
                    .chunks_mut(chunk * ch)
                    .zip(high.chunks_mut(chunk * ch))
                    .enumerate()
                {
                    let j0 = idx * chunk;
                    let self_ref = &*self;
                    let conv_ref = &conv;
                    s.spawn(move || self_ref.fill_range(conv_ref, low_p, high_p, j0, ch));
                }
            });
        } else {
            self.fill_range(&conv, &mut low, &mut high, 0, ch);
        }
        self.emitted += cnt;
        // Keep only the past context needed by the next output.
        let keep_from = self.emitted.saturating_sub(self.d);
        let drop = (keep_from - self.win_start) * ch;
        if drop > 0 {
            self.win.drain(..drop);
            self.win_start = keep_from;
        }
        (low, high)
    }

    /// Fill (low, high) frame chunks starting at emitted-relative frame j0.
    fn fill_range(&self, conv: &[f64], low: &mut [f64], high: &mut [f64], j0: usize, ch: usize) {
        for (j, (lo, hi)) in low.chunks_mut(ch).zip(high.chunks_mut(ch)).enumerate() {
            let gi = self.emitted + j0 + j;
            let wi = gi - self.win_start;
            for c in 0..ch {
                let l = conv[(wi + self.d) * ch + c];
                lo[c] = l;
                hi[c] = self.win[wi * ch + c] - l;
            }
        }
    }
}
