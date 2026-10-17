//! DSP: FIR crossover (reference coefficient tables), FFT convolution,
//! subtractive complement, zero-phase rational-ratio resampling.

mod conv;
mod crossover;
pub mod fir_tables;
mod resampler;
mod ssb;
mod tilt;
mod window;

pub use crossover::{CrossoverStream, split};
pub use resampler::{Resampler, StreamResampler};
pub use ssb::{HILBERT_DELAY, HILBERT_TAPS, ShiftStream, hilbert_taps, shift_down, shift_up, shift_up_offset};
pub use tilt::{FirStream, tilt_taps};

/// Cap on spawned worker threads in the parallel fill loops.
const MAX_WORKERS: usize = 16;

/// Frame count below which the parallel fills run serially (spawning threads
/// costs more than the fill at small sizes).
const PAR_MIN_FRAMES: usize = 16 * 1024;

/// Available parallelism capped at MAX_WORKERS (and optionally `cap`).
fn worker_threads(cap: usize) -> usize {
    std::thread::available_parallelism()
        .map(|t| t.get())
        .unwrap_or(1)
        .min(cap)
        .min(MAX_WORKERS)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::conv::fftconvolve_mono;
    use crate::tilt::TILT_KNOTS;
    use rustfft::FftPlanner;
    use rustfft::num_complex::Complex64;

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
    fn block_stitching_consistent() {
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
        assert!(maxe < 1e-8, "block stitching error {maxe}");
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
        for (i, (a, b)) in streamed.iter().zip(batch.iter()).enumerate() {
            assert_eq!(a, b, "stream != batch at {i}");
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
        for fc in [300.0, 600.0, 12000.0] {
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

    /// The three-track b19 split: the low (mid-band) output passes through
    /// 12000 Hz (-6 dB cutoff 12120, 240 Hz transition above the carrier),
    /// low+high reconstructs the input exactly, and the high (top branch)
    /// must be empty below the 12000 Hz SSB carrier.
    #[test]
    fn crossover_12000_stopband_and_complement() {
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
        let (low, high) = split(&x, 2, 12000.0);
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
        let im = vec![0.0f64; m];
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
        let pass = band_energy(2000.0, 11000.0);
        let stop = band_energy(12300.0, 24000.0);
        assert!(
            stop / pass < 1e-12,
            "stopband leak {} vs pass {}",
            stop,
            pass
        );
        // The top branch must be empty below the 12000 Hz SSB carrier:
        // measure the high output's sub-12k energy against its own passband.
        let m2 = 1 << 16;
        let mut re2 = vec![0.0f64; m2];
        for (k, i) in (skip..skip + m2).enumerate() {
            let w = 0.5 - 0.5 * (2.0 * std::f64::consts::PI * k as f64 / m2 as f64).cos();
            re2[k] = high[i * 2] * w;
        }
        let mut buf2: Vec<C> = re2.iter().map(|&r| C::new(r, 0.0)).collect();
        fft.process(&mut buf2);
        let band_energy2 = |f0: f64, f1: f64| -> f64 {
            let k0 = (f0 / 48000.0 * m2 as f64) as usize;
            let k1 = (f1 / 48000.0 * m2 as f64) as usize;
            buf2[k0..k1].iter().map(|c| c.norm_sqr()).sum::<f64>()
        };
        let below = band_energy2(1000.0, 11900.0);
        let above = band_energy2(13000.0, 20000.0);
        assert!(
            below / above < 1e-8,
            "top branch below-carrier leak {below} vs passband {above}"
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
        let mut planner = FftPlanner::<f64>::new();
        for freq in [1000.0, 5000.0, 12000.0, 18000.0, 22000.0] {
            let x = sine(freq, 48000, n);
            let conv = fftconvolve_mono(&x, &h, &mut planner);
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
        let fc = 12000.0;
        // tone at fc + 2000 Hz plus one at fc + 7000 Hz
        let mut x = vec![0.0; n];
        for i in 0..n {
            let t = i as f64 / 48000.0;
            x[i] = 0.5 * (2.0 * std::f64::consts::PI * 14000.0 * t).cos()
                + 0.3 * (2.0 * std::f64::consts::PI * 19000.0 * t).cos();
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
        // 10000 = 12000-2000 is below-carrier mirror of the shifted band).
        for f in [4800.0, 10000.0, 5000.0] {
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
        let fc = 12000.0;
        let mut x = vec![0.0; n];
        for (f, a) in [(12500.0, 0.4), (15000.0, 0.3), (18000.0, 0.2), (22000.0, 0.2)] {
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
        let fc = 12000.0;
        let f = 12050.0;
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

    /// Regression: with the split FIR transition straddling the 12 kHz
    /// carrier (cutoff 11880), top-branch content at 11.76-12 kHz landed at
    /// negative baseband frequencies under the SSB shift-down and real()
    /// folded it, phase-flipped, onto 12.0-12.24 kHz while its true place
    /// kept only the mid branch's half amplitude. The split FIR puts its
    /// transition entirely above the carrier (cutoff 12120): tones at and
    /// below the carrier rejoin at full amplitude with no mirror at 24k-f.
    #[test]
    fn crossover_12000_no_ssb_foldover() {
        let n = 48_000usize * 2;
        let skip = HILBERT_DELAY + 2400;
        let m = 1 << 16;
        let mag = |y: &[f64], f: f64| -> f64 {
            // Quadrature correlation: exact at any f (no FFT scalloping).
            let w0 = 2.0 * std::f64::consts::PI * f / 48000.0;
            let mut re = 0.0;
            let mut im = 0.0;
            for k in 0..m {
                re += y[skip + k] * (w0 * k as f64).cos();
                im -= y[skip + k] * (w0 * k as f64).sin();
            }
            2.0 * (re * re + im * im).sqrt() / m as f64
        };
        for f in [11800.0f64, 11880.0, 11900.0, 12000.0] {
            let x = sine(f, 48000, n);
            let (low, top) = split(&x, 1, 12000.0);
            let back = shift_up(&shift_down(&top, 1, 12000.0, 48000), 1, 12000.0, 48000);
            let rejoin: Vec<f64> = (0..n).map(|i| low[i] + back[i]).collect();
            let main = mag(&rejoin, f);
            let mirror = mag(&rejoin, 24000.0 - f);
            let top_leak = mag(&top, f);
            assert!(main > 0.95, "f={f}: rejoin amplitude collapsed: {main}");
            if (24000.0 - f - f).abs() > 1.0 {
                assert!(mirror < 0.02, "f={f}: mirror junk {mirror} vs main {main}");
            }
            assert!(top_leak < 0.02, "f={f}: top branch below carrier {top_leak}");
        }
    }

    /// Chunk-fed ShiftStream must equal the batch shift_down.
    #[test]
    fn shift_stream_matches_batch() {
        let n = 48_000 * 3;
        let x: Vec<f64> = sine(13_100.0, 48000, n)
            .iter()
            .flat_map(|&v| [v, v * 0.5])
            .collect();
        let batch = shift_down(&x, 2, 12000.0, 48000);
        let mut s = ShiftStream::down(12000.0, 48000);
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
        let fc = 12000.0;
        let mut x = vec![0.0; n * 2];
        for (f, a) in [(12400.0, 0.3), (15000.0, 0.3), (17500.0, 0.2), (19400.0, 0.15)] {
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

/// SRC quality regression pins (card 81): the 44.1k<->48k roundtrip must
/// stay flat through the 12 kHz content band and keep ultrasonic probe
/// leakage below the codec noise floor. Guards future kernel/spec changes.
#[cfg(test)]
mod src_quality {
    use super::*;
    use std::f64::consts::PI;

    fn tone(freq: f64, n: usize) -> Vec<f64> {
        (0..n).map(|i| (2.0 * PI * freq * i as f64 / 48000.0).sin()).collect()
    }

    fn tone_gain(y: &[f64], f: f64) -> f64 {
        let x = &y[4800..y.len() - 4800];
        let (mut re, mut im) = (0.0, 0.0);
        for (i, &v) in x.iter().enumerate() {
            let ph = 2.0 * PI * f * i as f64 / 48000.0;
            re += v * ph.cos();
            im += v * ph.sin();
        }
        2.0 * (re * re + im * im).sqrt() / x.len() as f64
    }

    /// Energy below 20 kHz excluding the probe tone's own +-300 Hz
    /// (fold-back residue after the roundtrip).
    fn band_residual(y: &[f64], probe: f64) -> f64 {
        let x = &y[4800..y.len() - 4800];
        let n = x.len();
        let mut acc = 0.0f64;
        // Goertzel per 200 Hz bin center is overkill; a direct DFT on the
        // 0..20k band edges is cheap enough for a test.
        let mut f = 100.0;
        while f < 20_000.0 {
            if (f - probe).abs() > 300.0 {
                let (mut re, mut im) = (0.0, 0.0);
                for (i, &v) in x.iter().enumerate() {
                    let ph = 2.0 * PI * f * i as f64 / 48000.0;
                    re += v * ph.cos();
                    im += v * ph.sin();
                }
                acc += 2.0 * (re * re + im * im).sqrt() / n as f64;
            }
            f += 200.0;
        }
        acc
    }

    fn roundtrip(x: &[f64], pass: Option<f64>) -> Vec<f64> {
        let down = match pass {
            None => Resampler::new(48000, 44100).process(x, 1),
            Some(p) => Resampler::new_with_passband(48000, 44100, p).process(x, 1),
        };
        match pass {
            None => Resampler::new(44100, 48000).process(&down, 1),
            Some(p) => Resampler::new_with_passband(44100, 48000, p).process(&down, 1),
        }
    }

    /// LF-lane kernels for the 44.1 kHz-output streams: 16k/32k -> 44.1k
    /// (441/160, 441/320). The LF content ends at the 300/600 Hz crossover,
    /// so a 1 kHz passband spec shrinks the 7.056/14.112 MHz-lcm kernels to
    /// ~25 taps/phase while the stopband still floors the imaging.
    #[test]
    fn lf_to_44k1_quality() {
        fn tone_at(freq: f64, rate: u32, secs: usize) -> Vec<f64> {
            (0..secs * rate as usize)
                .map(|i| (2.0 * PI * freq * i as f64 / rate as f64).sin())
                .collect()
        }
        fn gain_at(y: &[f64], f: f64, rate: u32) -> f64 {
            let edge = (rate as usize) / 10;
            let x = &y[edge..y.len() - edge];
            let (mut re, mut im) = (0.0, 0.0);
            for (i, &v) in x.iter().enumerate() {
                let ph = 2.0 * PI * f * i as f64 / rate as f64;
                re += v * ph.cos();
                im += v * ph.sin();
            }
            2.0 * (re * re + im * im).sqrt() / x.len() as f64
        }
        for in_rate in [16_000u32, 32_000] {
            let up = Resampler::new_with_passband(in_rate, 44100, 1_000.0);
            // passband flat to 700 Hz (covers both crossovers with margin)
            for f in [100.0, 300.0, 500.0, 700.0] {
                let y = up.process(&tone_at(f, in_rate, 4), 1);
                let g = gain_at(&y, f, 44100);
                assert!((g - 1.0).abs() < 1e-3, "{in_rate}->44.1k gain at {f} Hz: {g}");
            }
            // imaging of a 500 Hz tone: residues at 16k/32k multiples +- f
            // must sit at the stopband floor.
            let y = up.process(&tone_at(500.0, in_rate, 4), 1);
            let img = in_rate as f64;
            for fimg in [img - 500.0, img + 500.0] {
                if fimg < 22_050.0 {
                    let g = gain_at(&y, fimg, 44100);
                    assert!(g < 1e-5, "{in_rate}->44.1k image at {fimg} Hz: {g}");
                }
            }
        }
    }

    #[test]
    fn passband_flat_and_alias_floor() {
        let n = 48000 * 5;
        for (spec, pass) in [("default", None), ("band13k", Some(13_000.0))] {
            for f in [500.0, 4000.0, 8000.0, 11000.0, 12000.0] {
                let g = tone_gain(&roundtrip(&tone(f, n), pass), f);
                assert!(
                    (g - 1.0).abs() < 1e-3,
                    "{spec} passband gain at {f} Hz: {g}"
                );
            }
            // 22 kHz probe: rejected into the stopband, and nothing audible
            // folds back below 20 kHz.
            let y = roundtrip(&tone(22_000.0, n), pass);
            let res = band_residual(&y, 22_000.0);
            assert!(res < 1e-4, "{spec} 22k fold-back residual {res}");
        }
    }
}
