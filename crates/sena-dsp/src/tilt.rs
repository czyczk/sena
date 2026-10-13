//! Optional gentle HF tilt (pre-filter for the high band).

use rustfft::FftPlanner;
use rustfft::num_complex::Complex64;

/// The tilt design sample rate: the knot table tops out at 24 kHz.
const TILT_DESIGN_FS: f64 = 48000.0;

/// Reference tilt shape: (frequency Hz, gain dB) knots, linearly
/// interpolated. Flat below ~1.9 kHz, then a slow fall of a couple of dB.
pub(crate) const TILT_KNOTS: &[(f64, f64)] = &[
    (0.0, 0.0),
    (1900.0, 0.0),
    (2400.0, -0.05),
    (3024.0, -0.09),
    (3810.0, -0.17),
    (4800.0, -0.31),
    (6048.0, -0.57),
    (7620.0, -0.98),
    (9600.0, -1.61),
    (12095.0, -2.30),
    (15239.0, -1.90),
    (19000.0, -2.00),
    (24000.0, -2.10),
];

fn tilt_target_db(f: f64, strength_pct: f64) -> f64 {
    let k = TILT_KNOTS;
    if f <= k[0].0 {
        return 0.0;
    }
    let mut i = 1;
    while i < k.len() && k[i].0 < f {
        i += 1;
    }
    if i == k.len() {
        return k[k.len() - 1].1 * strength_pct / 100.0;
    }
    let (f0, g0) = k[i - 1];
    let (f1, g1) = k[i];
    let t = (f - f0) / (f1 - f0);
    (g0 + t * (g1 - g0)) * strength_pct / 100.0
}

/// Design the tilt FIR (odd length, linear phase, Hamming-windowed frequency
/// sampling). `strength_pct` scales the knot gains (100 = reference shape,
/// 0 = passthrough).
pub fn tilt_taps(strength_pct: f64) -> Vec<f64> {
    const N: usize = 513;
    const NF: usize = 4096;
    let d = N / 2;
    let mut spec = vec![Complex64::default(); NF];
    for (i, c) in spec.iter_mut().take(NF / 2 + 1).enumerate() {
        let f = i as f64 * TILT_DESIGN_FS / NF as f64;
        let g = 10f64.powf(tilt_target_db(f, strength_pct) / 20.0);
        *c = Complex64::new(g, 0.0);
    }
    for i in NF / 2 + 1..NF {
        spec[i] = spec[NF - i].conj();
    }
    let mut planner = FftPlanner::<f64>::new();
    let inv = planner.plan_fft_inverse(NF);
    inv.process(&mut spec);
    let mut h = vec![0.0; N];
    for k in 0..N {
        let idx = (k + NF - d) % NF;
        let w = 0.54 - 0.46 * (2.0 * std::f64::consts::PI * k as f64 / (N - 1) as f64).cos();
        h[k] = spec[idx].re * w / NF as f64;
    }
    h
}

/// Streaming chunk-fed FIR with the filter delay removed from the output
/// (output frame i uses input [i-d, i+d], zero-padded at both ends; the
/// stream behaves like a zero-phase pass aligned with the unfiltered path).
pub struct FirStream {
    h: Vec<f64>,
    d: usize,
    pend: Vec<Vec<f64>>, // per-channel queue, starting at global frame `pend_start`
    pend_start: usize,
    emitted: usize,
    ch: usize,
    finishing: bool,
}

impl FirStream {
    pub fn new(h: Vec<f64>) -> Self {
        assert!(h.len() % 2 == 1, "FIR length must be odd");
        Self {
            d: (h.len() - 1) / 2,
            h,
            pend: Vec::new(),
            pend_start: 0,
            emitted: 0,
            ch: 0,
            finishing: false,
        }
    }

    pub fn push(&mut self, x: &[f64], ch: usize) -> Vec<f64> {
        assert!(!self.finishing, "push after finish");
        assert!(x.len() % ch == 0, "input not interleaved at {ch} channels");
        if self.ch == 0 {
            self.ch = ch;
            self.pend = vec![Vec::new(); ch];
        } else {
            assert_eq!(self.ch, ch, "channel count changed");
        }
        for (c, q) in self.pend.iter_mut().enumerate() {
            q.extend(x.iter().skip(c).step_by(ch));
        }
        self.emit()
    }

    pub fn finish(&mut self, ch: usize) -> Vec<f64> {
        assert!(!self.finishing, "finish called twice");
        if self.ch == 0 {
            self.ch = ch;
            self.pend = vec![Vec::new(); ch];
        } else {
            assert_eq!(self.ch, ch, "channel count changed");
        }
        self.finishing = true;
        self.emit()
    }

    fn emit(&mut self) -> Vec<f64> {
        let ch = self.ch;
        let avail = self.pend_start + self.pend[0].len();
        let cnt = if self.finishing {
            avail.saturating_sub(self.emitted)
        } else {
            avail.saturating_sub(self.d).saturating_sub(self.emitted)
        };
        if cnt == 0 {
            return Vec::new();
        }
        let n_taps = self.h.len();
        let mut out = vec![0.0; cnt * ch];
        for j in 0..cnt {
            let gi = self.emitted + j; // global output frame
            for c in 0..ch {
                let mut acc = 0.0;
                for (k, &t) in self.h.iter().enumerate() {
                    // input frame index = gi + k - d
                    let ii = gi as isize + k as isize - self.d as isize;
                    if ii < 0 {
                        continue; // leading zero pad
                    }
                    let wpos = ii as isize - self.pend_start as isize;
                    let v = if wpos >= 0 && (wpos as usize) < self.pend[c].len() {
                        self.pend[c][wpos as usize]
                    } else {
                        0.0 // trailing zero pad on finish / not-yet-fed (guarded by cnt)
                    };
                    acc += t * v;
                }
                out[j * ch + c] = acc;
            }
        }
        self.emitted += cnt;
        // drop fully-consumed queue prefix (keep last n_taps-1 frames as tail)
        let keep = n_taps - 1;
        let consumed = self.emitted - self.pend_start;
        if consumed > keep {
            let drop = consumed - keep;
            for q in self.pend.iter_mut() {
                q.drain(..drop);
            }
            self.pend_start += drop;
        }
        out
    }
}
