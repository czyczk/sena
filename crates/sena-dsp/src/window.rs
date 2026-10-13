//! Window functions shared by the resampler prototype and the Hilbert taps.

/// Modified Bessel function I0 (series), used by the Kaiser window.
pub(crate) fn bessel_i0(x: f64) -> f64 {
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
pub(crate) fn kaiser_window(len: usize) -> Vec<f64> {
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
