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

    /// Resample one mono channel (offline).
    ///
    /// Small upsampling factors use the block-wise FFT convolver; large
    /// upsampling factors (e.g. 44.1k -> 48k with n = 160) would require
    /// huge zero-stuffed FFTs, so those use the equivalent direct polyphase
    /// evaluation in the input domain: every output sample is independent
    /// (exactly partition-independent, trivially parallel).
    pub fn process_mono(&self, x: &[f64], n_out: usize) -> Vec<f64> {
        if self.n > 8 {
            self.process_mono_direct(x, n_out)
        } else {
            self.process_mono_fft(x, n_out)
        }
    }

    /// y[o] = sum_k x[q - k] * kernel[p + d + k*n], with q = (o*m)/n,
    /// p = (o*m)%n, d = (L-1)/2: the zero-phase windowed-sinc convolution
    /// evaluated with the phase-kernel selected per output sample. Input
    /// samples outside the buffer contribute zero (same padding as FFT
    /// blocks). Cost per output = kernel.len()/n MACs; no upsampled buffer.
    fn process_mono_direct(&self, x: &[f64], n_out: usize) -> Vec<f64> {
        let mut out = vec![0.0; n_out];
        if n_out == 0 {
            return out;
        }
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16);
        if threads > 1 && n_out >= 16 * 1024 {
            let chunk = n_out.div_ceil(threads);
            std::thread::scope(|s| {
                for (idx, part) in out.chunks_mut(chunk).enumerate() {
                    let o_start = idx * chunk;
                    s.spawn(move || self.direct_range(part, x, o_start));
                }
            });
        } else {
            self.direct_range(&mut out, x, 0);
        }
        out
    }

    fn direct_range(&self, out: &mut [f64], x: &[f64], o_start: usize) {
        let l = self.kernel.len();
        let n = self.n;
        let m = self.m;
        let d = self.delay;
        let kern = &self.kernel;
        let n_as_isize = n as isize;
        for (j, y) in out.iter_mut().enumerate() {
            let pos = (o_start + j) * m;
            let q = pos / n;
            let p = pos % n;
            let s = p + d;
            // k such that 0 <= s + k*n < l; k_lo <= 0 <= k_hi.
            let k_hi = ((l - 1 - s) / n) as isize;
            let k_lo = -((s / n) as isize);
            let q_isize = q as isize;
            let mut acc = 0.0;
            let mut k = k_lo;
            while k <= k_hi {
                let i = q_isize - k;
                if i >= 0 && (i as usize) < x.len() {
                    let idx = (s as isize + k * n_as_isize) as usize;
                    acc += x[i as usize] * kern[idx];
                }
                k += 1;
            }
            *y = acc;
        }
    }

    /// Block-wise zero-stuff -> FFT convolution -> decimate (n <= 8 only).
    fn process_mono_fft(&self, x: &[f64], n_out: usize) -> Vec<f64> {
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
            // The last block must also emit the final output(s) whose padded
            // kernel index (j - bstart*n + delay) still falls inside conv:
            // o*m < seg_len*n + delay. floor((b+block)*n/m) drops the last
            // output whenever input_frames*n is not a multiple of m.
            let o_end = if bend == x.len() {
                o_start + (seg_len * self.n + self.delay).div_ceil(self.m)
            } else {
                (b + block) * self.n / self.m
            }
            .min(n_out);
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

/// Streaming chunk-fed zero-phase rational resampler (direct polyphase).
///
/// The batch `Resampler` evaluates every output sample independently, so a
/// streaming wrapper can emit each output as soon as its full kernel support
/// has arrived and emit the same values: `push` + `finish` over chunked
/// input is bit-identical to `Resampler::process` over the concatenated
/// input. Output latency is `(delay + n - 1) / n` input frames.
pub struct StreamResampler {
    base: Resampler,
    pend: Vec<f64>, // interleaved, starting at global frame `pend_start`
    pend_start: usize,
    total_in: usize,
    next_out: usize,
    ch: usize,
    finished: bool,
}

impl StreamResampler {
    pub fn new(fs_in: u32, fs_out: u32) -> Self {
        Self {
            base: Resampler::new(fs_in, fs_out),
            pend: Vec::new(),
            pend_start: 0,
            total_in: 0,
            next_out: 0,
            ch: 0,
            finished: false,
        }
    }

    /// Total output frames for the input frames fed so far (the count
    /// `finish` will have emitted).
    pub fn output_frames_total(&self) -> usize {
        self.base.output_frames(self.total_in)
    }

    fn ctx_future(&self) -> usize {
        let n = self.base.n;
        (self.base.delay + n - 1) / n
    }

    /// Feed interleaved input frames; returns the interleaved output frames
    /// whose full kernel support is now available (may be empty).
    pub fn push(&mut self, x: &[f64], ch: usize) -> Vec<f64> {
        assert!(!self.finished, "push after finish");
        assert!(x.len() % ch == 0, "input not interleaved at {ch} channels");
        if self.ch == 0 {
            self.ch = ch;
        } else {
            assert_eq!(self.ch, ch, "channel count changed");
        }
        self.pend.extend_from_slice(x);
        self.total_in += x.len() / ch;
        self.drain()
    }

    /// Flush the tail (zeros past the final input sample, matching `process`)
    /// and return the remaining outputs.
    pub fn finish(&mut self, ch: usize) -> Vec<f64> {
        assert!(!self.finished, "finish called twice");
        assert_eq!(self.ch, ch, "channel count changed");
        self.finished = true;
        let total_out = self.base.output_frames(self.total_in);
        let cnt = total_out.saturating_sub(self.next_out);
        let mut out = vec![0.0; cnt * ch];
        if cnt > 0 {
            self.emit_range(&mut out, self.next_out, ch);
            self.next_out = total_out;
        }
        out
    }

    fn drain(&mut self) -> Vec<f64> {
        let ch = self.ch;
        let ctx_future = self.ctx_future();
        let pend_end = self.pend_start + self.pend.len() / ch;
        // Emit o while its maximum needed input index q + ctx_future is
        // inside the pending window (q grows with o, so the prefix is
        // contiguous). The left context is guaranteed by the trim below.
        let mut cnt = 0usize;
        while {
            let o = self.next_out + cnt;
            ((o * self.base.m / self.base.n) + ctx_future) < pend_end
        } {
            cnt += 1;
        }
        if cnt == 0 {
            return Vec::new();
        }
        let mut out = vec![0.0; cnt * ch];
        self.emit_range(&mut out, self.next_out, ch);
        self.next_out += cnt;
        // Keep only the past-context needed by the next output.
        let keep_from = (self.next_out * self.base.m / self.base.n)
            .saturating_sub((self.base.kernel.len() - 1) / self.base.n);
        let drop = (keep_from - self.pend_start) * ch;
        if drop > 0 {
            self.pend.drain(..drop);
            self.pend_start = keep_from;
        }
        out
    }

    /// y[o] = sum_k x[q - k] * kernel[p + d + k*n] for o in [o_lo, o_hi),
    /// reading from the pending window; out-of-window inputs are zero.
    fn emit_range(&self, out: &mut [f64], o_lo: usize, ch: usize) {
        let l = self.base.kernel.len();
        let n = self.base.n;
        let m = self.base.m;
        let d = self.base.delay;
        let kern = &self.base.kernel;
        let frames = self.pend.len() / ch;
        let n_as_isize = n as isize;
        for (j, y) in out.iter_mut().enumerate() {
            let o = o_lo + j / ch;
            let c = j % ch;
            let pos = o * m;
            let q = pos / n;
            let p = pos % n;
            let s = p + d;
            let k_hi = ((l - 1 - s) / n) as isize;
            let k_lo = -((s / n) as isize);
            let q_isize = q as isize;
            let mut acc = 0.0;
            let mut k = k_lo;
            while k <= k_hi {
                let gi = q_isize - k;
                let idx = gi - self.pend_start as isize;
                if idx >= 0 && (idx as usize) < frames {
                    let ker = kern[(s as isize + k * n_as_isize) as usize];
                    acc += self.pend[(idx as usize) * ch + c] * ker;
                }
                k += 1;
            }
            *y = acc;
        }
    }
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
}

impl CrossoverStream {
    pub fn new(fc: f64) -> Self {
        let h: &'static [f64] = if (fc - 300.0).abs() < 1.0 {
            &fir_tables::FIR_300
        } else {
            &fir_tables::FIR_600
        };
        Self {
            h,
            d: (h.len() - 1) / 2,
            win: Vec::new(),
            win_start: 0,
            emitted: 0,
            ch: 0,
            finishing: false,
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
        let conv = fftconvolve_stereo(&self.win, self.h, ch, n);
        let mut low = vec![0.0; cnt * ch];
        let mut high = vec![0.0; cnt * ch];
        for j in 0..cnt {
            let gi = self.emitted + j;
            let wi = gi - self.win_start;
            for c in 0..ch {
                let l = conv[(wi + self.d) * ch + c];
                low[j * ch + c] = l;
                high[j * ch + c] = self.win[wi * ch + c] - l;
            }
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
        let x: Vec<f64> = (0..n).map(|i| (2.0*std::f64::consts::PI*100.0*i as f64/96000.0).sin()).collect();
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

    #[test]
    fn direct_matches_fft_44100_to_48() {
        // The polyphase direct path (n > 8) must equal the zero-stuff FFT
        // convolution: same kernel, same zero-phase convention. (Short
        // input: the FFT reference side is very slow for large n.)
        let n = 12000;
        let x = sine(100.0, 44100, n);
        let r = Resampler::new(44100, 48000);
        let n_out = r.output_frames(n);
        let direct = r.process_mono_direct(&x, n_out);
        let fft = r.process_mono_fft(&x, n_out);
        let mut maxe = 0.0f64;
        for i in 0..n_out {
            maxe = maxe.max((direct[i] - fft[i]).abs());
        }
        assert!(maxe < 1e-8, "direct vs FFT error {maxe}");
    }

    #[test]
    fn direct_stitching_44100_to_48() {
        // Large-n (direct) path is partition independent: split the input at
        // a frame count whose output count is exact for the 160/147 ratio
        // (14700 input frames = exactly 16000 outputs), so the two calls
        // sit on the same timeline. Outputs within ~650 samples of the seam
        // (kernel context crosses the split) are excluded, like the FFT
        // stitching test.
        let n = 48000;
        let x = sine(37.0, 44100, n);
        let r = Resampler::new(44100, 48000);
        let n_out = r.output_frames(n);
        let whole = r.process_mono(&x, n_out);
        let split_in = 14700;
        let n_half = r.output_frames(split_in);
        assert_eq!(n_half, 16000);
        let mut cat = vec![];
        cat.extend(r.process_mono(&x[..split_in], n_half));
        cat.extend(r.process_mono(&x[split_in..], n_out - n_half));
        let mut maxe = 0.0f64;
        let mut worst_i = 0usize;
        for i in 0..(n_half - 1000) {
            let e = (whole[i] - cat[i]).abs();
            if e > maxe {
                maxe = e;
                worst_i = i;
            }
        }
        for i in (n_half + 1000)..n_out {
            let e = (whole[i] - cat[i]).abs();
            if e > maxe {
                maxe = e;
                worst_i = i;
            }
        }
        assert!(maxe < 1e-9, "direct stitching error {maxe} at {worst_i}");
    }

    #[test]
    fn stream_resampler_matches_batch() {
        // Chunk-fed streaming must equal the batch resampler. For the direct
        // (n > 8) path the values are bit-identical per output sample.
        let n = 132_300; // 3 s at 44.1 kHz
        let x: Vec<f64> = sine(101.0, 44100, n)
            .iter()
            .flat_map(|&v| [v, v * 0.5])
            .collect();
        let r = Resampler::new(44100, 48000);
        let batch = r.process(&x, 2);
        let mut s = StreamResampler::new(44100, 48000);
        let mut streamed = Vec::new();
        for chunk in x.chunks(44100) {
            // 1 s chunks
            let out = s.push(chunk, 2);
            streamed.extend(out);
        }
        streamed.extend(s.finish(2));
        assert_eq!(streamed.len(), batch.len());
        for (a, b) in streamed.iter().zip(batch.iter()) {
            assert_eq!(a, b, "stream != batch at {}", streamed.iter().position(|v| v == a).unwrap());
        }
    }

    #[test]
    fn stream_resampler_matches_batch_fft_ratio() {
        // n <= 8 path (batch uses FFT blocks): streaming (direct) matches
        // within FFT rounding.
        let n = 48_000 * 3;
        let x: Vec<f64> = sine(101.0, 48000, n)
            .iter()
            .flat_map(|&v| [v, v * 0.5])
            .collect();
        let r = Resampler::new(48000, 16000);
        let batch = r.process(&x, 2);
        let mut s = StreamResampler::new(48000, 16000);
        let mut streamed = Vec::new();
        for chunk in x.chunks(48000) {
            streamed.extend(s.push(chunk, 2));
        }
        streamed.extend(s.finish(2));
        assert_eq!(streamed.len(), batch.len());
        let maxe = streamed
            .iter()
            .zip(batch.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(maxe < 1e-7, "stream vs batch (FFT path) error {maxe}");
    }

    #[test]
    fn crossover_stream_matches_split() {
        let n = 48_000 * 3;
        let x: Vec<f64> = sine(101.0, 48000, n)
            .iter()
            .flat_map(|&v| [v, v * 0.5])
            .collect();
        for fc in [300.0, 600.0] {
            let (blow, bhigh) = split(&x, 2, fc);
            let mut s = CrossoverStream::new(fc);
            let mut low = Vec::new();
            let mut high = Vec::new();
            for chunk in x.chunks(48_000) {
                let (l, h) = s.push(chunk, 2);
                low.extend(l);
                high.extend(h);
            }
            let (l, h) = s.finish(2);
            low.extend(l);
            high.extend(h);
            assert_eq!(low.len(), blow.len());
            assert_eq!(high.len(), bhigh.len());
            let ml = low.iter().zip(blow.iter()).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            let mh = high.iter().zip(bhigh.iter()).map(|(a, b)| (a - b).abs()).fold(0.0, f64::max);
            assert!(ml < 1e-8, "fc={fc} low error {ml}");
            assert!(mh < 1e-8, "fc={fc} high error {mh}");
        }
    }
}


