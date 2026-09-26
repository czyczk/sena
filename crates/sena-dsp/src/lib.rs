//! DSP: FIR crossover (reference coefficient tables), FFT convolution,
//! subtractive complement, zero-phase rational-ratio resampling.

pub mod fir_tables;

use rustfft::FftPlanner;
use rustfft::num_complex::Complex64;

/// Reference FIR for a split point. 300/600 are the profile crossovers;
/// 15600 is the three-track b19 split (cutoff 15480, stopband at 15600).
pub(crate) fn fir_for(fc: f64) -> &'static [f64] {
    if (fc - 300.0).abs() < 1.0 {
        &fir_tables::FIR_300
    } else if (fc - 600.0).abs() < 1.0 {
        &fir_tables::FIR_600
    } else if (fc - 15600.0).abs() < 1.0 {
        &fir_tables::FIR_15600
    } else {
        panic!("no reference FIR for split at {fc} Hz");
    }
}

/// Split stereo f64 samples into low/high bands with the reference FIR.
/// Returns (low, high); both delayed by (N-1)/2 relative to input.
pub fn split(x: &[f64], ch: usize, fc: f64) -> (Vec<f64>, Vec<f64>) {
    let h: &[f64] = fir_for(fc);
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
    // Polyphase decomposition of the kernel, precomputed once: per phase p
    // in [0, n), `phase_shift[p]` is the input-sample offset of the first
    // tap and `phase_taps[p]` the contiguous (reversed) tap values, so an
    // output sample is a plain dot product of two contiguous slices.
    phase_shift: Vec<usize>,
    phase_taps: Vec<Vec<f64>>,
}

impl Resampler {
    pub fn new(fs_in: u32, fs_out: u32) -> Self {
        if fs_in == fs_out {
            // Identity: the general direct formula degenerates exactly to
            // y[o] = x[o] with a unit kernel, so no special case is needed
            // anywhere downstream (no latency, no filtering).
            return Self::with_kernel(1, 1, vec![1.0], 0, 0);
        }
        let g = gcd(fs_in, fs_out);
        let n = (fs_out / g) as usize;
        let m = (fs_in / g) as usize;
        let fs_lcm = fs_in as u64 * n as u64;
        let min_nyq = fs_in.min(fs_out) as f64 / 2.0;
        // Upsampling (n > 1): the sinc zeros must sit at multiples of the
        // input sample spacing (cutoff = input Nyquist) so that the
        // interpolation property holds at the original sample points.
        let pass = if n > 1 {
            0.98 * min_nyq
        } else {
            0.90 * min_nyq
        };
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
        Self::with_kernel(n, m, kernel, delay, guard_in)
    }

    /// Build the phase tables for a given kernel (also used by the identity
    /// fast path).
    fn with_kernel(n: usize, m: usize, kernel: Vec<f64>, delay: usize, guard_in: usize) -> Self {
        let l = kernel.len();
        let mut phase_shift = Vec::with_capacity(n);
        let mut phase_taps = Vec::with_capacity(n);
        for p in 0..n {
            let base = p + delay;
            let lo_abs = base / n; // floor: k_lo = -lo_abs
            let k_hi = ((l - 1 - base) / n) as isize;
            let k_lo = -(lo_abs as isize);
            let mut taps = Vec::with_capacity((k_hi - k_lo + 1) as usize);
            for k in k_lo..=k_hi {
                taps.push(kernel[(base as isize + k * n as isize) as usize]);
            }
            taps.reverse(); // y[o] = sum_j x[q - lo_abs + j'] * taps_rev[j']
            phase_shift.push(lo_abs);
            phase_taps.push(taps);
        }
        Resampler {
            n,
            m,
            kernel,
            delay,
            guard_in,
            phase_shift,
            phase_taps,
        }
    }

    /// Number of output frames produced for a whole-buffer input of
    /// `input_frames` frames (matching `process`).
    pub fn output_frames(&self, input_frames: usize) -> usize {
        (input_frames * self.n + self.m / 2) / self.m
    }

    /// The decimation factor m of the n/m ratio: a windowed consumer that
    /// drains consumed input frames must keep the window start on this grid
    /// (drain multiples of m), or later outputs shift by a fraction of a
    /// frame against the whole-file phase grid.
    pub fn decimation(&self) -> usize {
        self.m
    }

    /// Context, in input frames, that a windowed consumer must keep on each
    /// side of the emitted range so that every emitted output has complete
    /// kernel support (windowed output then equals the corresponding range of
    /// a whole-buffer `process`). Rounded up to a whole decimation cycle so
    /// the output phase grid is preserved across window boundaries.
    pub fn guard_frames(&self) -> usize {
        self.guard_in.div_ceil(self.m.max(1)) * self.m.max(1)
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

    /// y[o] = sum_j x[s0 + j] * phase_taps[p][j] for the phase of output o,
    /// both slices contiguous (vectorizable); out-of-buffer inputs are zero.
    fn direct_range(&self, out: &mut [f64], x: &[f64], o_start: usize) {
        let m = self.m;
        let n = self.n;
        let xlen = x.len() as isize;
        for (j, y) in out.iter_mut().enumerate() {
            let o = o_start + j;
            let pos = o * m;
            let q = (pos / n) as isize;
            let p = pos % n;
            let shift = self.phase_shift[p] as isize;
            let taps = &self.phase_taps[p];
            let len = taps.len() as isize;
            // y[o] = sum_j x[q - (len-1) + shift + j] * taps[j]
            let s0 = q - (len - 1) + shift;
            let lo = s0.max(0);
            let hi = (s0 + len).min(xlen);
            let mut acc = 0.0;
            if lo < hi {
                let a = (lo - s0) as usize;
                let b = (hi - s0) as usize;
                let xa = lo as usize;
                let x_slice = &x[xa..xa + (b - a)];
                let mut accs = [0.0f64; 8];
                let mut i = 0usize;
                let t_slice = &taps[a..b];
                while i + 8 <= x_slice.len() {
                    for k in 0..8 {
                        accs[k] += x_slice[i + k] * t_slice[i + k];
                    }
                    i += 8;
                }
                while i < x_slice.len() {
                    accs[0] += x_slice[i] * t_slice[i];
                    i += 1;
                }
                acc = accs.iter().sum();
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
    pend: Vec<Vec<f64>>, // per channel, starting at global frame `pend_start`
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
            self.pend = vec![Vec::new(); ch];
        } else {
            assert_eq!(self.ch, ch, "channel count changed");
        }
        let frames = x.len() / ch;
        for (c, pend_c) in self.pend.iter_mut().enumerate() {
            pend_c.extend(x.iter().skip(c).step_by(ch));
        }
        self.total_in += frames;
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
        let pend_end = self.pend_start + self.pend[0].len();
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
        // Keep only the past context needed by the next output.
        let keep_from = (self.next_out * self.base.m / self.base.n)
            .saturating_sub((self.base.kernel.len() - 1) / self.base.n);
        let drop = keep_from - self.pend_start;
        if drop > 0 {
            for pend_c in self.pend.iter_mut() {
                pend_c.drain(..drop);
            }
            self.pend_start = keep_from;
        }
        out
    }

    /// Emit out[o_lo..) interleaved; dot products use contiguous slices
    /// (phase taps per output, per-channel pending), split across threads.
    fn emit_range(&self, out: &mut [f64], o_lo: usize, ch: usize) {
        let threads = std::thread::available_parallelism()
            .map(|n| n.get())
            .unwrap_or(1)
            .min(16);
        if threads > 1 && out.len() >= ch * 16 * 1024 {
            let frames = out.len() / ch;
            let chunk_frames = frames.div_ceil(threads);
            let chunk = chunk_frames * ch;
            std::thread::scope(|s| {
                for (idx, part) in out.chunks_mut(chunk).enumerate() {
                    let o = o_lo + idx * chunk_frames;
                    let this = &*self;
                    s.spawn(move || this.emit_range_inner(part, o, ch));
                }
            });
        } else {
            self.emit_range_inner(out, o_lo, ch);
        }
    }

    fn emit_range_inner(&self, out: &mut [f64], o_lo: usize, ch: usize) {
        let m = self.base.m;
        let n = self.base.n;
        let pend_start = self.pend_start as isize;
        let frames = self.pend[0].len() as isize;
        for (j, y) in out.iter_mut().enumerate() {
            let o = o_lo + j / ch;
            let c = j % ch;
            let pos = o * m;
            let q = (pos / n) as isize;
            let p = pos % n;
            let shift = self.base.phase_shift[p] as isize;
            let taps = &self.base.phase_taps[p];
            let len = taps.len() as isize;
            // y[o] = sum_j x[q - (len-1) + shift + j] * taps[j]
            let s0 = q - (len - 1) + shift - pend_start;
            let lo = s0.max(0);
            let hi = (s0 + len).min(frames);
            let mut acc = 0.0;
            if lo < hi {
                let a = (lo - s0) as usize;
                let b = (hi - s0) as usize;
                let xa = lo as usize;
                let x_slice = &self.pend[c][xa..xa + (b - a)];
                // 8 independent accumulators: keeps FMA chains short and
                // lets the compiler pipeline the scalar loop.
                let mut accs = [0.0f64; 8];
                let mut i = 0usize;
                let t_slice = &taps[a..b];
                while i + 8 <= x_slice.len() {
                    for k in 0..8 {
                        accs[k] += x_slice[i + k] * t_slice[i + k];
                    }
                    i += 8;
                }
                while i < x_slice.len() {
                    accs[0] += x_slice[i] * t_slice[i];
                    i += 1;
                }
                acc = accs.iter().sum();
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
    conv_cache: Option<ConvCache>,
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
            conv_cache: None,
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
        let conv = self.win_convolve(ch, n);
        let mut low = vec![0.0; cnt * ch];
        let mut high = vec![0.0; cnt * ch];
        // Every output is independent: fill in parallel for large chunks.
        let threads = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(1)
            .min(16);
        if threads > 1 && cnt >= 16 * 1024 {
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
                    s.spawn(move || {
                        for (j, (lo, hi)) in
                            low_p.chunks_mut(ch).zip(high_p.chunks_mut(ch)).enumerate()
                        {
                            let gi = self_ref.emitted + j0 + j;
                            let wi = gi - self_ref.win_start;
                            for c in 0..ch {
                                let l = conv_ref[(wi + self_ref.d) * ch + c];
                                lo[c] = l;
                                hi[c] = self_ref.win[wi * ch + c] - l;
                            }
                        }
                    });
                }
            });
        } else {
            for j in 0..cnt {
                let gi = self.emitted + j;
                let wi = gi - self.win_start;
                for c in 0..ch {
                    let l = conv[(wi + self.d) * ch + c];
                    low[j * ch + c] = l;
                    high[j * ch + c] = self.win[wi * ch + c] - l;
                }
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

    /// FFT convolution of the current window with the FIR kernel, reusing the
    /// plan and kernel spectrum across chunks (the window length only changes
    /// when the chunk size does, so the cache hits on every chunk).
    fn win_convolve(&mut self, ch: usize, n: usize) -> Vec<f64> {
        let out_len = n + self.h.len() - 1;
        let fft_len = out_len.next_power_of_two();
        let cache_ok = self
            .conv_cache
            .as_ref()
            .map(|c| c.fft_len == fft_len)
            .unwrap_or(false);
        if !cache_ok {
            let mut planner = FftPlanner::<f64>::new();
            let fwd = planner.plan_fft_forward(fft_len);
            let inv = planner.plan_fft_inverse(fft_len);
            let mut hspec = vec![Complex64::default(); fft_len];
            for (i, &v) in self.h.iter().enumerate() {
                hspec[i] = Complex64::new(v, 0.0);
            }
            fwd.process(&mut hspec);
            self.conv_cache = Some(ConvCache {
                fft_len,
                fwd,
                inv,
                hspec,
            });
        }
        let c = self.conv_cache.as_ref().unwrap();
        let mut out = vec![0.0; out_len * ch];
        let mut buf = vec![Complex64::default(); fft_len];
        for i in 0..ch {
            for j in 0..fft_len {
                buf[j] = Complex64::new(if j < n { self.win[j * ch + i] } else { 0.0 }, 0.0);
            }
            c.fwd.process(&mut buf);
            for j in 0..fft_len {
                buf[j] = buf[j] * c.hspec[j];
            }
            c.inv.process(&mut buf);
            let scale = 1.0 / fft_len as f64;
            for j in 0..out_len {
                out[j * ch + i] = buf[j].re * scale;
            }
        }
        out
    }
}

struct ConvCache {
    fft_len: usize,
    fwd: std::sync::Arc<dyn rustfft::Fft<f64>>,
    inv: std::sync::Arc<dyn rustfft::Fft<f64>>,
    hspec: Vec<Complex64>,
}

// ---------------------------------------------------------------------------
// Single-sideband frequency shift (analytic-signal / Hilbert method), used to
// move a band-limited top band down to baseband for coding and back up after
// decoding. A band-limited Opus track whose content sits only in the top
// octave is coded poorly on its own: the band allocation is content-blind,
// so the empty low bands keep their share and the actual content starves.
// Shifting the band to baseband puts every bit on the content.

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

/// SSB shift core: out[i] = x[i]*cos(w*(origin+i)) -/+ H{x}[i]*sin(w*(...))
/// with w = 2*pi*fc/fs; `up` selects the sign (down: +sin, up: -sin).
/// `origin` is the absolute frame index carried by output frame 0 (the
/// carrier phase reference; negative values are fine). Channels run on
/// separate threads for large inputs (same threshold class as the
/// crossover fills); values are identical to the serial evaluation.
fn ssb_shift(x: &[f64], ch: usize, fc: f64, fs: u32, origin: i64, up: bool) -> Vec<f64> {
    let h = hilbert_taps();
    let d = HILBERT_DELAY;
    let n = x.len() / ch;
    let w0 = 2.0 * std::f64::consts::PI * fc / fs as f64;
    let shift_channel = |c: usize, dst: &mut Vec<f64>| {
        dst.clear();
        dst.reserve(n);
        let mono: Vec<f64> = x.iter().skip(c).step_by(ch).copied().collect();
        let conv = fftconvolve_mono(&mono, &h);
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
    let threads = std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .min(ch)
        .min(16);
    if threads > 1 && n >= 16 * 1024 {
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
    h: Vec<f64>,
    fc: f64,
    fs: u32,
    win: Vec<f64>, // interleaved, starting at global frame `win_start`
    win_start: usize,
    emitted: usize,
    ch: usize,
    finishing: bool,
    conv_cache: Option<ConvCache>,
}

impl ShiftStream {
    /// Down-shift by `fc` (encode side of the three-track top band).
    pub fn down(fc: f64, fs: u32) -> Self {
        Self {
            h: hilbert_taps(),
            fc,
            fs,
            win: Vec::new(),
            win_start: 0,
            emitted: 0,
            ch: 0,
            finishing: false,
            conv_cache: None,
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
        let conv = self.win_convolve(ch, n);
        let w0 = 2.0 * std::f64::consts::PI * self.fc / self.fs as f64;
        let mut out = vec![0.0; cnt * ch];
        // Every output frame is independent: fill in parallel for large
        // chunks (same pattern as the crossover emit).
        let threads = std::thread::available_parallelism()
            .map(|t| t.get())
            .unwrap_or(1)
            .min(16);
        if threads > 1 && cnt >= 16 * 1024 {
            let chunk = cnt.div_ceil(threads) * ch;
            std::thread::scope(|s| {
                for (idx, part) in out.chunks_mut(chunk).enumerate() {
                    let j0 = idx * (chunk / ch);
                    let self_ref = &*self;
                    let conv_ref = &conv;
                    s.spawn(move || {
                        for (jj, o) in part.chunks_mut(ch).enumerate() {
                            let j = j0 + jj;
                            let gi = self_ref.emitted + j;
                            let wi = gi - self_ref.win_start;
                            let ph = w0 * gi as f64;
                            let (sn, co) = ph.sin_cos();
                            for c in 0..ch {
                                // down-shift: Re{(x + j*H{x}) e^{-jwt}}
                                o[c] = self_ref.win[wi * ch + c] * co
                                    + conv_ref[(wi + d) * ch + c] * sn;
                            }
                        }
                    });
                }
            });
        } else {
            for j in 0..cnt {
                let gi = self.emitted + j;
                let wi = gi - self.win_start;
                let ph = w0 * gi as f64;
                let (s, co) = ph.sin_cos();
                for c in 0..ch {
                    // down-shift: Re{(x + j*H{x}) * e^{-jwt}} = x*cos + H{x}*sin
                    out[j * ch + c] = self.win[wi * ch + c] * co + conv[(wi + d) * ch + c] * s;
                }
            }
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

    /// FFT convolution of the current window with the Hilbert kernel,
    /// reusing the plan and kernel spectrum across chunks (same caching
    /// strategy as the crossover).
    fn win_convolve(&mut self, ch: usize, n: usize) -> Vec<f64> {
        let out_len = n + self.h.len() - 1;
        let fft_len = out_len.next_power_of_two();
        let cache_ok = self
            .conv_cache
            .as_ref()
            .map(|c| c.fft_len == fft_len)
            .unwrap_or(false);
        if !cache_ok {
            let mut planner = FftPlanner::<f64>::new();
            let fwd = planner.plan_fft_forward(fft_len);
            let inv = planner.plan_fft_inverse(fft_len);
            let mut hspec = vec![Complex64::default(); fft_len];
            for (i, &v) in self.h.iter().enumerate() {
                hspec[i] = Complex64::new(v, 0.0);
            }
            fwd.process(&mut hspec);
            self.conv_cache = Some(ConvCache {
                fft_len,
                fwd,
                inv,
                hspec,
            });
        }
        let c = self.conv_cache.as_ref().unwrap();
        let mut out = vec![0.0; out_len * ch];
        let mut buf = vec![Complex64::default(); fft_len];
        for i in 0..ch {
            for j in 0..fft_len {
                buf[j] = Complex64::new(if j < n { self.win[j * ch + i] } else { 0.0 }, 0.0);
            }
            c.fwd.process(&mut buf);
            for j in 0..fft_len {
                buf[j] = buf[j] * c.hspec[j];
            }
            c.inv.process(&mut buf);
            let scale = 1.0 / fft_len as f64;
            for j in 0..out_len {
                out[j * ch + i] = buf[j].re * scale;
            }
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Optional gentle HF tilt (pre-filter for the high band).

/// Reference tilt shape: (frequency Hz, gain dB) knots, linearly
/// interpolated. Flat below ~1.9 kHz, then a slow fall of a couple of dB.
const TILT_KNOTS: &[(f64, f64)] = &[
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
        let f = i as f64 * 48000.0 / NF as f64;
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
        let _ = n_taps;
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
        assert!(
            (y[p] - expected).abs() < 1e-4,
            "peak {} vs {expected}",
            y[p]
        );
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
        let x: Vec<f64> = (0..n)
            .map(|i| (2.0 * std::f64::consts::PI * 100.0 * i as f64 / 96000.0).sin())
            .collect();
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
            assert_eq!(
                a,
                b,
                "stream != batch at {}",
                streamed.iter().position(|v| v == a).unwrap()
            );
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
        for fc in [300.0, 600.0, 15600.0] {
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
            let ml = low
                .iter()
                .zip(blow.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            let mh = high
                .iter()
                .zip(bhigh.iter())
                .map(|(a, b)| (a - b).abs())
                .fold(0.0, f64::max);
            assert!(ml < 1e-8, "fc={fc} low error {ml}");
            assert!(mh < 1e-8, "fc={fc} high error {mh}");
        }
    }

    /// The three-track b19 split: the low (mid-band) output must have no
    /// measurable energy at or above 15600 Hz (-6 dB cutoff 15480, 240 Hz
    /// transition), and low+high must reconstruct the input exactly.
    #[test]
    fn crossover_15600_stopband_and_complement() {
        // Two tones straddling the split + broadband noise, 2 s stereo.
        let n = 48_000 * 2;
        let mut x = Vec::with_capacity(n * 2);
        let mut rng: u64 = 0x9E3779B97F4A7C15;
        let mut noise = move || {
            rng ^= rng << 13;
            rng ^= rng >> 7;
            rng ^= rng << 17;
            (rng as f64 / u64::MAX as f64) * 2.0 - 1.0
        };
        for i in 0..n {
            let t = i as f64 / 48000.0;
            let v = (2.0 * std::f64::consts::PI * 10000.0 * t).sin() * 0.5
                + (2.0 * std::f64::consts::PI * 18000.0 * t).sin() * 0.4
                + noise() * 0.1;
            x.push(v);
            x.push(v * 0.7);
        }
        let (low, high) = split(&x, 2, 15600.0);
        // Complementarity (minus the edge regions of the convolution).
        let skip = 2100;
        for i in skip..n - skip {
            for c in 0..2 {
                let sum = low[i * 2 + c] + high[i * 2 + c];
                assert!((sum - x[i * 2 + c]).abs() < 1e-9, "frame {i} ch {c}");
            }
        }
        // Low energy above 15600: FFT the settled region of one channel.
        let m = 1 << 16;
        let mut re = vec![0.0f64; m];
        let mut im = vec![0.0f64; m];
        for (k, i) in (skip..skip + m).enumerate() {
            // Hann window (gain-compensated): only relative band energy matters.
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / m as f64).cos();
            re[k] = low[i * 2] * w;
        }
        let mut planner = rustfft::FftPlanner::<f64>::new();
        let fft = planner.plan_fft_forward(m);
        use rustfft::num_complex::Complex64 as C;
        let mut buf: Vec<C> = re
            .iter()
            .zip(im.iter())
            .map(|(&r, &i)| C::new(r, i))
            .collect();
        fft.process(&mut buf);
        let band_energy = |f0: f64, f1: f64| -> f64 {
            let k0 = (f0 / 48000.0 * m as f64) as usize;
            let k1 = (f1 / 48000.0 * m as f64) as usize;
            buf[k0..k1].iter().map(|c| c.norm_sqr()).sum::<f64>()
        };
        let pass = band_energy(2000.0, 14000.0);
        let stop = band_energy(15700.0, 24000.0);
        assert!(
            stop / pass < 1e-12,
            "stopband leak {} vs pass {}",
            stop,
            pass
        );
    }

    #[test]
    fn tilt_taps_response_matches_knots() {
        let h = tilt_taps(100.0);
        assert_eq!(h.len(), 513);
        // DTFT at knot frequencies
        for &(f, g_db) in TILT_KNOTS.iter().skip(2).take(10) {
            let w = 2.0 * std::f64::consts::PI * f / 48000.0;
            let mut re = 0.0;
            let mut im = 0.0;
            for (k, &t) in h.iter().enumerate() {
                re += t * (w * k as f64).cos();
                im -= t * (w * k as f64).sin();
            }
            let got = 20.0 * (re * re + im * im).sqrt().log10();
            assert!(
                (got - g_db).abs() < 0.08,
                "at {f} Hz: got {got:.3} dB, want {g_db}"
            );
        }
    }

    #[test]
    fn tilt_taps_zero_strength_is_passthrough() {
        let h = tilt_taps(0.0);
        let mut delta = vec![0.0; h.len()];
        delta[h.len() / 2] = 1.0;
        let e = h
            .iter()
            .zip(delta.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(
            e < 1e-9,
            "strength 0 should be a pure delay line, max err {e}"
        );
    }

    /// The Hilbert filter turns a cosine into a sine (quadrature) with
    /// unit gain across the band, so x + j*H{x} is analytic.
    #[test]
    fn hilbert_quadrature_accuracy() {
        let h = hilbert_taps();
        let d = HILBERT_DELAY;
        let n = 48_000usize;
        for freq in [1000.0, 5000.0, 12000.0, 18000.0, 22000.0] {
            let x = sine(freq, 48000, n);
            let conv = fftconvolve_mono(&x, &h);
            let mut maxe = 0.0f64;
            for i in d + 100..n - d - 100 {
                let got = conv[i + d];
                let phase = 2.0 * std::f64::consts::PI * freq * i as f64 / 48000.0;
                // x = sin(w i) -> quadrature is -cos(w i)
                let want_q = -phase.cos();
                maxe = maxe.max((got - want_q).abs());
            }
            assert!(maxe < 2e-3, "hilbert quadrature at {freq} Hz: {maxe}");
        }
    }

    /// shift_down moves a tone at fc+df to df, with the conjugate image
    /// (at 2*fc+df equivalents) suppressed by the Hilbert stopband.
    #[test]
    fn shift_down_places_band_at_baseband() {
        let n = 48_000usize;
        let fc = 15600.0;
        // tone at fc + 2000 Hz plus one at fc + 7000 Hz
        let mut x = vec![0.0; n];
        for i in 0..n {
            let t = i as f64 / 48000.0;
            x[i] = 0.5 * (2.0 * std::f64::consts::PI * 17600.0 * t).cos()
                + 0.3 * (2.0 * std::f64::consts::PI * 22600.0 * t).cos();
        }
        let y = shift_down(&x, 1, fc, 48000);
        // FFT of the settled region; expect peaks at 2000 and 7000 Hz only.
        let m = 1 << 15;
        let skip = HILBERT_DELAY + 100;
        let mut buf: Vec<Complex64> = (0..m)
            .map(|k| Complex64::new(y[skip + k], 0.0))
            .collect();
        let mut planner = FftPlanner::<f64>::new();
        planner.plan_fft_forward(m).process(&mut buf);
        let peak_at = |f: f64| -> f64 {
            let k = (f / 48000.0 * m as f64).round() as usize;
            buf[k].norm()
        };
        let p_lo = peak_at(2000.0);
        let p_hi = peak_at(7000.0);
        assert!(p_lo > 0.4 * 0.5 * m as f64, "2 kHz peak too small: {p_lo}");
        assert!(p_hi > 0.4 * 0.3 * m as f64, "7 kHz peak too small: {p_hi}");
        // images: 2*fc - band content would appear as mirrored junk; check a
        // few bins that must be empty (e.g. 4800 Hz = image of nothing, and
        // 13600 = 15600-2000 is below-carrier mirror of the shifted band).
        for f in [4800.0, 13600.0, 11600.0] {
            let p = peak_at(f);
            assert!(p < 1e-3 * p_lo, "image at {f} Hz: {p} vs {p_lo}");
        }
    }

    /// shift_up inverts shift_down for band content clear of the carrier
    /// edges: tones between fc+400 Hz and 22.5 kHz come back within the
    /// Hilbert/resampler ripple.
    #[test]
    fn shift_roundtrip_recovers_band() {
        let n = 48_000usize;
        let fc = 15600.0;
        let mut x = vec![0.0; n];
        for (f, a) in [(16000.0, 0.4), (17500.0, 0.3), (19000.0, 0.2), (22500.0, 0.2)] {
            for (i, v) in x.iter_mut().enumerate() {
                *v += a * (2.0 * std::f64::consts::PI * f * i as f64 / 48000.0).cos();
            }
        }
        let down = shift_down(&x, 1, fc, 48000);
        let back = shift_up(&down, 1, fc, 48000);
        let mut maxe = 0.0f64;
        let skip = HILBERT_DELAY + 200;
        for i in skip..n - skip {
            maxe = maxe.max((back[i] - x[i]).abs());
        }
        assert!(maxe < 2e-3, "roundtrip error {maxe}");
    }

    /// Content within ~35 Hz of the carrier sits in the Hilbert transition
    /// of the decode-side shift: a tone at fc+50 Hz must still come back at
    /// nearly full amplitude, and its mirror image below fc stays small.
    #[test]
    fn shift_roundtrip_carrier_edge() {
        let n = 48_000usize * 2;
        let fc = 15600.0;
        let f = 15650.0;
        let x = sine(f, 48000, n);
        let back = shift_up(&shift_down(&x, 1, fc, 48000), 1, fc, 48000);
        let skip = HILBERT_DELAY + 200;
        let m = 1 << 16;
        // Hann window (coherent gain 1/2) to keep spectral leakage from
        // contaminating the image measurement.
        let mut buf: Vec<Complex64> = (0..m)
            .map(|k| {
                let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / m as f64).cos();
                Complex64::new(back[skip + k] * w, 0.0)
            })
            .collect();
        let mut planner = FftPlanner::<f64>::new();
        planner.plan_fft_forward(m).process(&mut buf);
        let mag = |f: f64| buf[(f / 48000.0 * m as f64).round() as usize].norm() * 4.0 / m as f64;
        let main = mag(f);
        let image = mag(2.0 * fc - f);
        assert!(main > 0.8, "edge tone amplitude collapsed: {main}");
        assert!(image < 0.2 * main, "edge image {image} vs main {main}");
    }

    /// Chunk-fed ShiftStream must equal the batch shift_down.
    #[test]
    fn shift_stream_matches_batch() {
        let n = 48_000 * 3;
        let x: Vec<f64> = sine(18_100.0, 48000, n)
            .iter()
            .flat_map(|&v| [v, v * 0.5])
            .collect();
        let batch = shift_down(&x, 2, 15600.0, 48000);
        let mut s = ShiftStream::down(15600.0, 48000);
        let mut streamed = Vec::new();
        for chunk in x.chunks(48_000) {
            streamed.extend(s.push(chunk, 2));
        }
        streamed.extend(s.finish(2));
        assert_eq!(streamed.len(), batch.len());
        let maxe = streamed
            .iter()
            .zip(batch.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(maxe < 1e-8, "stream vs batch shift error {maxe}");
    }

    /// The encode-side top-band chain is a near-perfect roundtrip before the
    /// codec: split complement -> shift down -> 48k->16k -> 16k->48k ->
    /// shift up recovers the band (tones clear of both edges).
    #[test]
    fn top_band_channel_roundtrip() {
        let n = 48_000usize * 2;
        let fc = 15600.0;
        let mut x = vec![0.0; n * 2];
        for (f, a) in [(16100.0, 0.3), (18000.0, 0.3), (21000.0, 0.2), (22400.0, 0.15)] {
            for i in 0..n {
                let v = a * (2.0 * std::f64::consts::PI * f * i as f64 / 48000.0).cos();
                x[i * 2] += v;
                x[i * 2 + 1] += 0.7 * v;
            }
        }
        let (_low, top) = split(&x, 2, fc);
        let down = shift_down(&top, 2, fc, 48000);
        let bb = Resampler::new(48000, 16000).process(&down, 2);
        let up = Resampler::new(16000, 48000).process(&bb, 2);
        let back = shift_up(&up, 2, fc, 48000);
        let skip = HILBERT_DELAY + 2400;
        let n_up = back.len() / 2;
        let mut maxe = 0.0f64;
        for i in skip..n_up.min(n) - skip {
            for c in 0..2 {
                maxe = maxe.max((back[i * 2 + c] - top[i * 2 + c]).abs());
            }
        }
        assert!(maxe < 5e-3, "top band roundtrip error {maxe}");
    }

    #[test]
    fn fir_stream_matches_batch() {
        let h = tilt_taps(100.0);
        let d = (h.len() - 1) / 2;
        let n = 100_000usize;
        let x: Vec<f64> = (0..n)
            .map(|i| ((i * 2654435761usize) % 1000) as f64 / 1000.0 - 0.5)
            .collect();
        // batch reference: y[i] = sum_k h[k] x[i+k-d], zero-padded
        let want: Vec<f64> = (0..n)
            .map(|i| {
                let mut acc = 0.0;
                for (k, &t) in h.iter().enumerate() {
                    let ii = i as isize + k as isize - d as isize;
                    if ii >= 0 && (ii as usize) < n {
                        acc += t * x[ii as usize];
                    }
                }
                acc
            })
            .collect();
        let mut s = FirStream::new(h.clone());
        let mut got = Vec::new();
        for chunk in x.chunks(17_321) {
            got.extend(s.push(chunk, 1));
        }
        got.extend(s.finish(1));
        assert_eq!(got.len(), want.len());
        let e = got
            .iter()
            .zip(want.iter())
            .map(|(a, b)| (a - b).abs())
            .fold(0.0, f64::max);
        assert!(e < 1e-12, "stream vs batch error {e}");
    }
}
