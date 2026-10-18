//! Zero-phase rational-ratio resampling: batch (`Resampler`) and streaming
//! (`StreamResampler`) forms, bit-identical over the same input.

use rustfft::FftPlanner;

use crate::conv::fftconvolve_mono;
use crate::window::kaiser_window;
use crate::{PAR_MIN_FRAMES, worker_threads};

fn gcd(mut a: u32, mut b: u32) -> u32 {
    while b != 0 {
        let t = a % b;
        a = b;
        b = t;
    }
    a
}

/// Dot product of two equal-length slices with 8 independent accumulators
/// (keeps FMA chains short and lets the compiler pipeline the scalar loop).
/// The accumulation order is part of the bit-exactness contract.
fn dot8(x: &[f64], t: &[f64]) -> f64 {
    debug_assert_eq!(x.len(), t.len());
    let mut accs = [0.0f64; 8];
    let mut i = 0usize;
    while i + 8 <= x.len() {
        for k in 0..8 {
            accs[k] += x[i + k] * t[i + k];
        }
        i += 8;
    }
    while i < x.len() {
        accs[0] += x[i] * t[i];
        i += 1;
    }
    accs.iter().sum()
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
        let min_nyq = fs_in.min(fs_out) as f64 / 2.0;
        // Upsampling (n > 1): the sinc zeros must sit at multiples of the
        // input sample spacing (cutoff = input Nyquist) so that the
        // interpolation property holds at the original sample points.
        let pass = if n > 1 {
            0.98 * min_nyq
        } else {
            0.90 * min_nyq
        };
        Self::build(fs_in, fs_out, min_nyq - pass)
    }

    /// Band-limited variant: the passband edge is `pass_hz` (content is
    /// known to end there) while the stopband still starts at the smaller
    /// Nyquist. The default constructor preserves ~98% of Nyquist, which
    /// for awkward ratios (e.g. 44100 -> 48000, lcm 7.056 MHz) forces a
    /// ~192k-tap kernel; a band-limited content spec shrinks the kernel
    /// proportionally with no in-band consequence.
    pub fn new_with_passband(fs_in: u32, fs_out: u32, pass_hz: f64) -> Self {
        if fs_in == fs_out {
            return Self::with_kernel(1, 1, vec![1.0], 0, 0);
        }
        let min_nyq = fs_in.min(fs_out) as f64 / 2.0;
        let trans = min_nyq - pass_hz;
        assert!(pass_hz > 0.0 && trans > 0.0, "bad passband spec");
        Self::build(fs_in, fs_out, trans)
    }

    fn build(fs_in: u32, fs_out: u32, trans: f64) -> Self {
        let g = gcd(fs_in, fs_out);
        let n = (fs_out / g) as usize;
        let m = (fs_in / g) as usize;
        let fs_lcm = fs_in as u64 * n as u64;
        let min_nyq = fs_in.min(fs_out) as f64 / 2.0;
        // Upsampling keeps the sinc cutoff at the input Nyquist (the
        // interpolation property at original sample points); downsampling
        // centers the transition band.
        let fc_sinc = if n > 1 { min_nyq } else { min_nyq - 0.5 * trans };
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

    /// The interpolation factor n of the n/m ratio: a windowed consumer
    /// keeping chunk sizes on this output grid preserves the whole-file
    /// phase grid across window boundaries.
    pub fn phases(&self) -> usize {
        self.n
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
        // Identity fast path (the unit kernel): the generic direct/FFT
        // machinery would convolve pointlessly (measured: the 44.1
        // kHz-output streams' two identity lanes dominated streaming
        // decode time).
        if self.n == 1 && self.m == 1 && self.kernel.len() == 1 {
            return x.to_vec();
        }
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
        if self.n == 1 && self.m == 1 && self.kernel.len() == 1 {
            return x[..n_out].to_vec();
        }
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
    pub(crate) fn process_mono_direct(&self, x: &[f64], n_out: usize) -> Vec<f64> {
        let mut out = vec![0.0; n_out];
        if n_out == 0 {
            return out;
        }
        let threads = worker_threads(usize::MAX);        if threads > 1 && n_out >= PAR_MIN_FRAMES {
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
                acc = dot8(x_slice, &taps[a..b]);
            }
            *y = acc;
        }
    }

    /// Block-wise zero-stuff -> FFT convolution -> decimate (n <= 8 only).
    pub(crate) fn process_mono_fft(&self, x: &[f64], n_out: usize) -> Vec<f64> {
        let mut out = vec![0.0; n_out];
        // input block size: multiple of m, FFT-bounded
        let mut block = 65536usize.div_ceil(self.m) * self.m;
        while (block + 2 * self.guard_in) * self.n + self.kernel.len() > (1 << 22) {
            block = (block / 2).div_ceil(self.m).max(1) * self.m;
        }
        let mut planner = FftPlanner::<f64>::new();
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
            let conv = fftconvolve_mono(&up, &self.kernel, &mut planner);
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
        Self::with_resampler(Resampler::new(fs_in, fs_out))
    }

    pub fn new_with_passband(fs_in: u32, fs_out: u32, pass_hz: f64) -> Self {
        Self::with_resampler(Resampler::new_with_passband(fs_in, fs_out, pass_hz))
    }

    fn with_resampler(base: Resampler) -> Self {
        Self {
            base,
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
        let threads = worker_threads(usize::MAX);
        if threads > 1 && out.len() >= ch * PAR_MIN_FRAMES {
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
                acc = dot8(x_slice, &taps[a..b]);
            }
            *y = acc;
        }
    }
}
