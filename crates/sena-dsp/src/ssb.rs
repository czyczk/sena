//! Single-sideband frequency shift (analytic-signal / Hilbert method), used to
//! move a band-limited top band down to baseband for coding and back up after
//! decoding. A band-limited Opus track whose content sits only in the top
//! octave is coded poorly on its own: the band allocation is content-blind,
//! so the empty low bands keep their share and the actual content starves.
//! Shifting the band to baseband puts every bit on the content.

use rustfft::FftPlanner;

use crate::conv::{FftConvolver, fftconvolve_mono};
use crate::window::kaiser_window;
use crate::{PAR_MIN_FRAMES, worker_threads};

/// Hilbert transformer length. 8001 taps (Kaiser beta 9) keep the transition
/// at the DC/Nyquist edges ~35 Hz wide, so the band edge at the shift
/// carrier is reconstructed cleanly.
pub const HILBERT_TAPS: usize = 8001;

/// Half the Hilbert kernel length: the leading/trailing context a shifted
/// output frame needs, in frames at the shift operating rate.
pub const HILBERT_DELAY: usize = (HILBERT_TAPS - 1) / 2;

/// Windowed Hilbert transformer taps (odd length, Kaiser beta 9, the same
/// window family as the crossover tables). Under the zero-phase convolution
/// convention (output i uses input [i-d, i+d]) the filter produces the
/// quadrature component of its input: x + j*H{x} is the analytic signal
/// (negative frequencies suppressed).
pub fn hilbert_taps() -> Vec<f64> {
    let w = kaiser_window(HILBERT_TAPS);
    let d = (HILBERT_TAPS - 1) / 2;
    (0..HILBERT_TAPS)
        .map(|k| {
            let m = k as isize - d as isize;
            if m % 2 == 0 {
                0.0
            } else {
                2.0 / (std::f64::consts::PI * m as f64) * w[k]
            }
        })
        .collect()
}

/// Memoized Hilbert taps: the design is deterministic, so the 8001-tap
/// windowed series is computed once per process.
fn cached_hilbert_taps() -> &'static [f64] {
    static TAPS: std::sync::OnceLock<Vec<f64>> = std::sync::OnceLock::new();
    TAPS.get_or_init(hilbert_taps)
}

/// SSB shift core: out[i] = x[i]*cos(w*(origin+i)) -/+ H{x}[i]*sin(w*(...))
/// with w = 2*pi*fc/fs; `up` selects the sign (down: +sin, up: -sin).
/// `origin` is the absolute frame index carried by output frame 0 (the
/// carrier phase reference; negative values are fine). Channels run on
/// separate threads for large inputs (same threshold class as the
/// crossover fills); values are identical to the serial evaluation.
fn ssb_shift(x: &[f64], ch: usize, fc: f64, fs: u32, origin: i64, up: bool) -> Vec<f64> {
    let h = cached_hilbert_taps();
    let d = HILBERT_DELAY;
    let n = x.len() / ch;
    let w0 = 2.0 * std::f64::consts::PI * fc / fs as f64;
    let shift_channel = |c: usize, dst: &mut Vec<f64>| {
        dst.clear();
        dst.reserve(n);
        let mono: Vec<f64> = x.iter().skip(c).step_by(ch).copied().collect();
        let mut planner = FftPlanner::<f64>::new();
        let conv = fftconvolve_mono(&mono, h, &mut planner);
        for i in 0..n {
            let ph = w0 * (origin + i as i64) as f64;
            let (s, co) = ph.sin_cos();
            let q = conv[i + d];
            dst.push(if up {
                mono[i] * co - q * s
            } else {
                mono[i] * co + q * s
            });
        }
    };
    let mut chans: Vec<Vec<f64>> = vec![Vec::new(); ch];
    let threads = worker_threads(ch);
    if threads > 1 && n >= PAR_MIN_FRAMES {
        let f = &shift_channel;
        std::thread::scope(|s| {
            for (c, dst) in chans.iter_mut().enumerate() {
                s.spawn(move || f(c, dst));
            }
        });
    } else {
        for (c, dst) in chans.iter_mut().enumerate() {
            shift_channel(c, dst);
        }
    }
    let mut out = Vec::with_capacity(x.len());
    for i in 0..n {
        for c in 0..ch {
            out.push(chans[c][i]);
        }
    }
    out
}

/// Shift content at/above `fc` down to baseband (encode side of the
/// three-track top band). Carrier phase zero at output frame 0.
pub fn shift_down(x: &[f64], ch: usize, fc: f64, fs: u32) -> Vec<f64> {
    ssb_shift(x, ch, fc, fs, 0, false)
}

/// Inverse of [`shift_down`]: move a baseband signal back up by `fc`.
/// Carrier phase zero at output frame 0.
pub fn shift_up(x: &[f64], ch: usize, fc: f64, fs: u32) -> Vec<f64> {
    ssb_shift(x, ch, fc, fs, 0, true)
}

/// [`shift_up`] with an explicit carrier origin: output frame 0 carries the
/// phase of absolute frame `origin` (used when the shifted window does not
/// start at the playable timeline origin).
pub fn shift_up_offset(x: &[f64], ch: usize, fc: f64, fs: u32, origin: i64) -> Vec<f64> {
    ssb_shift(x, ch, fc, fs, origin, true)
}

/// Streaming chunk-fed SSB down-shift matching [`shift_down`]: output frame
/// i uses input [i-d, i+d] (zero-phase aligned, d = [`HILBERT_DELAY`]) and
/// the carrier phase of the global frame index, so chunked output equals the
/// batch function over the concatenated input. Output latency is d frames.
pub struct ShiftStream {
    h: &'static [f64],
    fc: f64,
    fs: u32,
    win: Vec<f64>, // interleaved, starting at global frame `win_start`
    win_start: usize,
    emitted: usize,
    ch: usize,
    finishing: bool,
    convolver: FftConvolver,
}

impl ShiftStream {
    /// Down-shift by `fc` (encode side of the three-track top band).
    pub fn down(fc: f64, fs: u32) -> Self {
        Self {
            h: cached_hilbert_taps(),
            fc,
            fs,
            win: Vec::new(),
            win_start: 0,
            emitted: 0,
            ch: 0,
            finishing: false,
            convolver: FftConvolver::new(),
        }
    }

    /// Feed interleaved input frames; returns the shifted interleaved
    /// frames that are fully settled (may be empty).
    pub fn push(&mut self, x: &[f64], ch: usize) -> Vec<f64> {
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
    /// sample), returning the settled shifted frames.
    pub fn finish(&mut self, ch: usize) -> Vec<f64> {
        assert!(!self.finishing, "finish called twice");
        assert_eq!(self.ch, ch, "channel count changed");
        self.finishing = true;
        self.emit(ch)
    }

    fn emit(&mut self, ch: usize) -> Vec<f64> {
        let d = HILBERT_DELAY;
        let end = self.win_start + self.win.len() / ch;
        let cnt = if self.finishing {
            end.saturating_sub(self.emitted)
        } else {
            end.saturating_sub(d).saturating_sub(self.emitted)
        };
        if cnt == 0 {
            return Vec::new();
        }
        let n = self.win.len() / ch;
        let conv = self.convolver.convolve_interleaved(&self.win, self.h, ch, n);
        let w0 = 2.0 * std::f64::consts::PI * self.fc / self.fs as f64;
        let mut out = vec![0.0; cnt * ch];
        // Every output frame is independent: fill in parallel for large
        // chunks (same pattern as the crossover emit).
        let threads = worker_threads(usize::MAX);
        if threads > 1 && cnt >= PAR_MIN_FRAMES {
            let chunk = cnt.div_ceil(threads) * ch;
            std::thread::scope(|s| {
                for (idx, part) in out.chunks_mut(chunk).enumerate() {
                    let j0 = idx * (chunk / ch);
                    let self_ref = &*self;
                    let conv_ref = &conv;
                    s.spawn(move || self_ref.fill_range(conv_ref, part, j0, ch, w0));
                }
            });
        } else {
            self.fill_range(&conv, &mut out, 0, ch, w0);
        }
        self.emitted += cnt;
        // Keep only the past context needed by the next output.
        let keep_from = self.emitted.saturating_sub(d);
        let drop = (keep_from - self.win_start) * ch;
        if drop > 0 {
            self.win.drain(..drop);
            self.win_start = keep_from;
        }
        out
    }

    /// Fill shifted frame chunks starting at emitted-relative frame j0:
    /// down-shift, Re{(x + j*H{x}) * e^{-jwt}} = x*cos + H{x}*sin.
    fn fill_range(&self, conv: &[f64], out: &mut [f64], j0: usize, ch: usize, w0: f64) {
        let d = HILBERT_DELAY;
        for (jj, o) in out.chunks_mut(ch).enumerate() {
            let j = j0 + jj;
            let gi = self.emitted + j;
            let wi = gi - self.win_start;
            let ph = w0 * gi as f64;
            let (sn, co) = ph.sin_cos();
            for c in 0..ch {
                o[c] = self.win[wi * ch + c] * co + conv[(wi + d) * ch + c] * sn;
            }
        }
    }
}
