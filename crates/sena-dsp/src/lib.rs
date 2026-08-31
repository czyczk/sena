//! DSP: FIR crossover (reference coefficient tables), FFT convolution,
//! subtractive complement, zero-phase rational-ratio resampling.

pub mod fir_tables;

use rustfft::num_complex::Complex64;
use rustfft::FftPlanner;

/// Split stereo f64 samples into low/high bands with the reference FIR.
/// Returns (low, high); both delayed by (N-1)/2 relative to input.
pub fn split(x: &[f64], ch: usize, fc: f64) -> (Vec<f64>, Vec<f64>) {
    let h: &[f64] = if (fc - 300.0).abs() < 1.0 {
        &fir_tables::FIR_300
    } else {
        &fir_tables::FIR_600
    };
    let n = x.len() / ch;
    let low = fftconvolve_stereo(x, h, ch, n);
    // high = delayed x - low
    let d = (h.len() - 1) / 2;
    let mut high = vec![0.0; n * ch];
    for c in 0..ch {
        for i in 0..n {
            let xi = x[i * ch + c];
            high[i * ch + c] = xi - low[(i + d) * ch + c];
        }
    }
    // crop the FIR delay from low
    let mut low_c = vec![0.0; n * ch];
    for c in 0..ch {
        for i in 0..n {
            low_c[i * ch + c] = low[(i + d) * ch + c];
        }
    }
    (low_c, high)
}

fn fftconvolve_stereo(x: &[f64], h: &[f64], ch: usize, n: usize) -> Vec<f64> {
    let out_len = n + h.len() - 1;
    let fft_len = out_len.next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(fft_len);
    let inv = planner.plan_fft_inverse(fft_len);
    let mut hspec = vec![Complex64::default(); fft_len];
    for (i, &v) in h.iter().enumerate() {
        hspec[i] = Complex64::new(v, 0.0);
    }
    fwd.process(&mut hspec);

    let mut out = vec![0.0; out_len * ch];
    let mut buf = vec![Complex64::default(); fft_len];
    for c in 0..ch {
        for i in 0..fft_len {
            buf[i] = Complex64::new(if i < n { x[i * ch + c] } else { 0.0 }, 0.0);
        }
        fwd.process(&mut buf);
        for i in 0..fft_len {
            buf[i] = buf[i] * hspec[i];
        }
        inv.process(&mut buf);
        let scale = 1.0 / fft_len as f64;
        for i in 0..out_len {
            out[i * ch + c] = buf[i].re * scale;
        }
    }
    out
}

/// 'full' linear convolution of one channel.
fn fftconvolve_mono(x: &[f64], h: &[f64]) -> Vec<f64> {
    let out_len = x.len() + h.len() - 1;
    let fft_len = out_len.next_power_of_two();
    let mut planner = FftPlanner::<f64>::new();
    let fwd = planner.plan_fft_forward(fft_len);
    let inv = planner.plan_fft_inverse(fft_len);
    let mut hs = vec![Complex64::default(); fft_len];
    for (i, &v) in h.iter().enumerate() {
        hs[i] = Complex64::new(v, 0.0);
    }
    fwd.process(&mut hs);
    let mut buf = vec![Complex64::default(); fft_len];
    for (i, &v) in x.iter().enumerate() {
        buf[i] = Complex64::new(v, 0.0);
    }
    fwd.process(&mut buf);
    for i in 0..fft_len {
        buf[i] = buf[i] * hs[i];
    }
    inv.process(&mut buf);
    let scale = 1.0 / fft_len as f64;
    let mut out = vec![0.0; out_len];
    for (i, v) in out.iter_mut().enumerate() {
        *v = buf[i].re * scale;
    }
    out
}

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Modified Bessel function I0 (series), used by the Kaiser window.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0;
    let mut term = 1.0;
    let xh = x / 2.0;
    for k in 1..64 {
        term *= xh * xh / (k * k) as f64;
        sum += term;
        if term < 1e-18 * sum {
            break;
        }
    }
    sum
}

/// Kaiser window (beta = 9.0), length `len` (scipy-compatible).
fn kaiser_window(len: usize) -> Vec<f64> {
    const BETA: f64 = 9.0;
    let m = (len - 1) as f64 / 2.0;
    let denom = bessel_i0(BETA);
    (0..len)
        .map(|i| {
            let t = (i as f64 - m) / m;
            bessel_i0(BETA * (1.0 - t * t).sqrt()) / denom
        })
        .collect()
}

/// Zero-phase rational-ratio resampler: output rate = input rate * n / m
/// (n/m reduced integers). The prototype is a symmetric windowed-sinc FIR
/// applied as: zero-stuff by n -> symmetric convolution (zero phase) ->
/// decimate by m, processed in overlapping blocks (memory-bounded).
pub struct Resampler {
    n: usize,
    m: usize,
    kernel: Vec<f64>, // symmetric, odd length, DC gain = n (at rate fs_in*n)
    delay: usize,     // (kernel.len()-1)/2, in fs_in*n samples
    guard_in: usize,  // kernel tail in input samples
}

impl Resampler {
    pub fn new(fs_in: u32, fs_out: u32) -> Self {
        let g = gcd(fs_in, fs_out);
        let n = (fs_out / g) as usize;
        let m = (fs_in / g) as usize;
        let fs_lcm = fs_in as u64 * n as u64;
        let min_nyq = fs_in.min(fs_out) as f64 / 2.0;
        // Upsampling (n > 1): the sinc zeros must sit at multiples of the
        // input sample spacing (cutoff = input Nyquist) so that the
        // interpolation property holds at the original sample points.
        let pass = if n > 1 { 0.98 * min_nyq } else { 0.90 * min_nyq };
        let trans = min_nyq - pass;
        let fc_sinc = if n > 1 { min_nyq } else { pass + 0.5 * trans };
        let taps = ((fs_lcm as f64) * 12.0 / trans).ceil() as usize | 1;
        let delay = (taps - 1) / 2;
        let win = kaiser_window(taps);
        let mut kernel: Vec<f64> = (0..taps)
            .map(|i| {
                let x = 2.0 * fc_sinc * (i as f64 - delay as f64) / fs_lcm as f64;
                let sinc = if x == 0.0 {
                    1.0
                } else {
                    (std::f64::consts::PI * x).sin() / (std::f64::consts::PI * x)
                };
                sinc * win[i]
            })
            .collect();
        // normalize DC gain to n (upsampler convention -> passband gain 1)
        let dc: f64 = kernel.iter().sum();
        for v in kernel.iter_mut() {
            *v *= n as f64 / dc;
        }
        // guard covers the kernel half-width (delay) plus the decimation
        // stride (m), so every output of a block has complete kernel support.
        let guard_in = (delay + m).div_ceil(n) + 1;
        Resampler {
            n,
            m,
            kernel,
            delay,
            guard_in,
        }
    }

    /// Number of output frames produced for a whole-buffer input of
    /// `input_frames` frames (matching `process`).
    pub fn output_frames(&self, input_frames: usize) -> usize {
        (input_frames * self.n + self.m / 2) / self.m
    }

    /// Smallest number of input frames whose whole-buffer resample yields at
    /// least `output_frames`, clamped to at most `output_frames` (the exact
    /// prefix length used by streaming consumers). Returns `None` when the
    /// rational ratio cannot represent that output count.
    pub fn input_frames_for_output(&self, output_frames: usize) -> Option<usize> {
        let est = output_frames.saturating_mul(self.m) / self.n;
        let lo = est.saturating_sub(2);
        let hi = est.saturating_add(2);
        for k in lo..=hi {
            if self.output_frames(k) == output_frames {
                return Some(k);
            }
        }
        None
    }

    /// Resample a whole interleaved buffer (offline; block-wise internally).
    pub fn process(&self, x: &[f64], ch: usize) -> Vec<f64> {
        let n_in = x.len() / ch;
        let n_out = self.output_frames(n_in);
        let mut out = vec![0.0; n_out * ch];
        for c in 0..ch {
            let mono: Vec<f64> = x.iter().skip(c).step_by(ch).copied().collect();
            let res = self.process_mono(&mono, n_out);
            for (o, &v) in res.iter().enumerate() {
                out[o * ch + c] = v;
            }
        }
        out
    }

    /// Resample one mono channel (offline; block-wise internally).
    pub fn process_mono(&self, x: &[f64], n_out: usize) -> Vec<f64> {
        let mut out = vec![0.0; n_out];
        // input block size: multiple of m, FFT-bounded
        let mut block = 65536usize.div_ceil(self.m) * self.m;
        while (block + 2 * self.guard_in) * self.n + self.kernel.len() > (1 << 22) {
            block = (block / 2).div_ceil(self.m).max(1) * self.m;
        }
        let mut b = 0usize;
        while b < x.len() {
            let bstart = b.saturating_sub(self.guard_in);
            // extend the segment by guard on both sides so every output of
            // this block has complete kernel support (seam-free stitching)
            let bend = (b + block + self.guard_in).min(x.len());
            let seg_len = bend - bstart;
            let mut up = vec![0.0; seg_len * self.n];
            for i in 0..seg_len {
                up[i * self.n] = x[bstart + i];
            }
            let conv = fftconvolve_mono(&up, &self.kernel);
            let o_start = b * self.n / self.m;
            let o_end = ((b + block).min(x.len()) * self.n / self.m).min(n_out);
            for o in o_start..o_end {
                let j = o * self.m;
                let cidx = (j - bstart * self.n) + self.delay;
                out[o] = if cidx < conv.len() { conv[cidx] } else { 0.0 };
            }
            b += block;
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complement_reconstructs() {
        let n = 48000;
        let mut x = vec![0.0; n * 2];
        for i in 0..n {
            let v = 0.5 * (2.0 * std::f64::consts::PI * 80.0 * i as f64 / 48000.0).sin();
            x[i * 2] = v;
            x[i * 2 + 1] = v * 0.8;
        }
        x[1200] = 0.9;
        let (low, high) = split(&x, 2, 300.0);
        let d = (fir_tables::FIR_300.len() - 1) / 2;
        let mut maxe = 0.0f64;
        for i in 0..n - 2 * d {
            let e = (low[i * 2] + high[i * 2] - x[i * 2]).abs();
            maxe = maxe.max(e);
        }
        assert!(maxe < 1e-10, "complement error {maxe}");
    }

    fn sine(freq: f64, fs: u32, n: usize) -> Vec<f64> {
        (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * freq * i as f64 / fs as f64).sin())
            .collect()
    }

    #[test]
    fn impulse_is_zero_phase() {
        // delta at the middle of the buffer: the response must be symmetric
        // around the corresponding output sample; the peak equals the
        // band-limited impulse amplitude 2*fc_sinc/fs_lcm (unity DC gain).
        let n = 48000;
        let mut x = vec![0.0; n];
        x[24000] = 1.0;
        let r = Resampler::new(48000, 16000);
        let y = r.process_mono(&x, n / 3);
        assert_eq!(y.len(), 16000);
        let p = 24000 / 3;
        let expected = 2.0 * 7600.0 / 48000.0; // fc_sinc = 0.95*8000
        assert!((y[p] - expected).abs() < 1e-4, "peak {} vs {expected}", y[p]);
        for k in 1..4000 {
            let d = (y[p + k] - y[p - k]).abs();
            assert!(d < 1e-6 * expected, "asymmetry at {k}: {d}");
        }
    }

    #[test]
    fn sine_fidelity_48_to_16() {
        let n = 48000;
        let x = sine(100.0, 48000, n);
        let r = Resampler::new(48000, 16000);
        let y = r.process_mono(&x, 16000);
        let mut maxe = 0.0f64;
        for i in 1000..14000 {
            let e = (y[i] - (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 16000.0).sin()).abs();
            maxe = maxe.max(e);
        }
        assert!(maxe < 2e-4, "sine error {maxe}");
    }

    #[test]
    fn sine_fidelity_48_to_32() {
        let n = 48000;
        let x = sine(100.0, 48000, n);
        let r = Resampler::new(48000, 32000);
        let y = r.process_mono(&x, 32000);
        let mut maxe = 0.0f64;
        for i in 1000..30000 {
            let e = (y[i] - (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 32000.0).sin()).abs();
            maxe = maxe.max(e);
        }
        assert!(maxe < 2e-4, "sine error {maxe}");
    }

    #[test]
    fn sine_fidelity_44100_to_48() {
        let n = 44100;
        let x = sine(100.0, 44100, n);
        let r = Resampler::new(44100, 48000);
        let y = r.process_mono(&x, (n as f64 * 48000.0 / 44100.0).round() as usize);
        let mut maxe = 0.0f64;
        for i in 2000..y.len() - 2000 {
            let e = (y[i] - (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 48000.0).sin()).abs();
            maxe = maxe.max(e);
        }
        assert!(maxe < 3e-4, "sine error {maxe}");
    }

    #[test]
    fn sine_fidelity_96_to_48() {
        let n = 96000;
        let x = sine(100.0, 96000, n);
        let r = Resampler::new(96000, 48000);
        let y = r.process_mono(&x, n / 2);
        let mut maxe = 0.0f64;
        for i in 1000..y.len() - 1000 {
            let e = (y[i] - (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 48000.0).sin()).abs();
            maxe = maxe.max(e);
        }
        assert!(maxe < 2e-4, "sine error {maxe}");
    }

    #[test]
    fn block_stiching_consistent() {
        // resampling must not depend on the block partition: compare a
        // long buffer resampled in one call against resampling its halves.
        let n = 48000;
        let x = sine(37.0, 48000, n);
        let r = Resampler::new(48000, 16000);
        let whole = r.process_mono(&x, 16000);
        // manual half-splitting via two calls
        let mut cat = vec![];
        cat.extend(r.process_mono(&x[..24000], 8000));
        cat.extend(r.process_mono(&x[24000..], 8000));
        let mut maxe = 0.0f64;
        for i in 2000..7880 {
            maxe = maxe.max((whole[i] - cat[i]).abs());
        }
        for i in 8120..14000 {
            maxe = maxe.max((whole[i] - cat[i]).abs());
        }
        assert!(maxe < 1e-8, "block stiching error {maxe}");
    }
}
