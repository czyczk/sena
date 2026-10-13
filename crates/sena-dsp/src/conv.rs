//! FFT convolution primitives: one-shot whole-buffer convolution and the
//! plan/spectrum cache shared by the streaming windowed convolvers.

use rustfft::FftPlanner;
use rustfft::num_complex::Complex64;

/// 'full' convolution of interleaved multi-channel input with a mono kernel.
pub(crate) fn fftconvolve_interleaved(x: &[f64], h: &[f64], ch: usize, n: usize) -> Vec<f64> {
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

/// 'full' linear convolution of one channel. The planner is supplied by
/// the caller so block loops can reuse their plans (planning is far more
/// expensive than the transforms it produces).
pub(crate) fn fftconvolve_mono(
    x: &[f64],
    h: &[f64],
    planner: &mut FftPlanner<f64>,
) -> Vec<f64> {
    let out_len = x.len() + h.len() - 1;
    let fft_len = out_len.next_power_of_two();
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

struct ConvCache {
    fft_len: usize,
    fwd: std::sync::Arc<dyn rustfft::Fft<f64>>,
    inv: std::sync::Arc<dyn rustfft::Fft<f64>>,
    hspec: Vec<Complex64>,
}

/// FFT convolution of a sliding interleaved window with a fixed kernel,
/// reusing the plan and kernel spectrum across calls (the window length only
/// changes when the chunk size does, so the cache hits on every chunk).
pub(crate) struct FftConvolver {
    cache: Option<ConvCache>,
}

impl FftConvolver {
    pub(crate) fn new() -> Self {
        Self { cache: None }
    }

    /// Convolve `win` (interleaved, `n` frames at `ch` channels) with `h`.
    pub(crate) fn convolve_interleaved(&mut self, win: &[f64], h: &[f64], ch: usize, n: usize) -> Vec<f64> {
        let out_len = n + h.len() - 1;
        let fft_len = out_len.next_power_of_two();
        let cache_ok = self
            .cache
            .as_ref()
            .map(|c| c.fft_len == fft_len)
            .unwrap_or(false);
        if !cache_ok {
            let mut planner = FftPlanner::<f64>::new();
            let fwd = planner.plan_fft_forward(fft_len);
            let inv = planner.plan_fft_inverse(fft_len);
            let mut hspec = vec![Complex64::default(); fft_len];
            for (i, &v) in h.iter().enumerate() {
                hspec[i] = Complex64::new(v, 0.0);
            }
            fwd.process(&mut hspec);
            self.cache = Some(ConvCache {
                fft_len,
                fwd,
                inv,
                hspec,
            });
        }
        let c = self.cache.as_ref().unwrap();
        let mut out = vec![0.0; out_len * ch];
        let mut buf = vec![Complex64::default(); fft_len];
        for i in 0..ch {
            for j in 0..fft_len {
                buf[j] = Complex64::new(if j < n { win[j * ch + i] } else { 0.0 }, 0.0);
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
