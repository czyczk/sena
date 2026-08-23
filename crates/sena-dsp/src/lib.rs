//! DSP: FIR crossover (reference coefficient tables), FFT convolution,
//! subtractive complement, rubato resampling.

pub mod fir_tables;

use rustfft::num_complex::Complex64;
use rustfft::FftPlanner;
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters, SincInterpolationType, WindowFunction,
};

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

/// High-quality 48k -> 16k resampler for the low-frequency band.
pub struct Downsampler {
    rs: SincFixedIn<f64>,
    in_buf: Vec<f64>,
    n_in: usize,
}

impl Downsampler {
    pub fn new(input_len: usize) -> Self {
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };
        let rs = SincFixedIn::<f64>::new(
            16000.0 / 48000.0,
            2.0,
            params,
            input_len,
            2,
        )
        .expect("downsampler init");
        Downsampler {
            rs,
            in_buf: vec![0.0; input_len],
            n_in: input_len,
        }
    }

    /// Consume one full-length stereo buffer (48k), return 16k stereo.
    pub fn process(&mut self, x: &[f64]) -> Vec<f64> {
        assert_eq!(x.len(), self.n_in * 2);
        let ch0: Vec<f64> = x.chunks(2).map(|p| p[0]).collect();
        let ch1: Vec<f64> = x.chunks(2).map(|p| p[1]).collect();
        let out = self.rs.process(&[ch0, ch1], None).expect("resample");
        let n = out[0].len();
        let mut y = vec![0.0; n * 2];
        for i in 0..n {
            y[i * 2] = out[0][i];
            y[i * 2 + 1] = out[1][i];
        }
        y
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn complement_reconstructs() {
        // impulse train + tone; check low + high == delayed original
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
}
