//! Pipeline stage micro-benchmarks (encoder/decoder DSP hot spots).
//! Usage: cargo run --release -p sena-dsp --example bench_pipeline [seconds]
//! Times each stage on synthetic stereo audio of the given length.

use sena_dsp::Resampler;
use std::time::Instant;

fn stereo(frames: usize, fs: u32, seed: u32) -> Vec<f64> {
    let mut x = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        for ch in 0..2 {
            let t = i as f64 / fs as f64;
            let tone = (2.0 * std::f64::consts::PI * 130.0 * t).sin()
                * (2.0 * std::f64::consts::PI * 0.25 * t).sin();
            let noise = ((((i as u64).wrapping_mul(2_654_435_761).wrapping_add(seed as u64 + ch as u64)) >> 33)
                as f64
                / (1u64 << 31) as f64
                - 1.0)
                * 0.01;
            x.push(tone * 0.8 + noise);
        }
    }
    x
}

fn stage(name: &str, f: impl FnOnce() -> usize) {
    let t = Instant::now();
    let n = f();
    eprintln!("{name:22} {:8.2} s  ({n} frames out)", t.elapsed().as_secs_f64());
}

fn main() {
    let secs: usize = std::env::args()
        .nth(1)
        .and_then(|s| s.parse().ok())
        .unwrap_or(240); // 4 minutes
    let frames = secs * 48_000;
    let frames44 = secs * 44_100;
    eprintln!("bench: {secs} s of audio ({frames} frames @48k, {frames44} @44.1k)");

    let x44 = stereo(frames44, 44_100, 1);
    stage("normalize 44.1k->48k", || {
        Resampler::new(44_100, 48_000).process(&x44, 2).len()
    });
    drop(x44);

    let x48 = stereo(frames, 48_000, 2);
    stage("crossover split @300", || {
        let (lo, hi) = sena_dsp::split(&x48, 2, 300.0);
        lo.len() + hi.len()
    });
    stage("crossover split @600", || {
        let (lo, hi) = sena_dsp::split(&x48, 2, 600.0);
        lo.len() + hi.len()
    });
    stage("lf downsample 48k->16k", || {
        Resampler::new(48_000, 16_000).process(&x48, 2).len()
    });
    stage("lf downsample 48k->32k", || {
        Resampler::new(48_000, 32_000).process(&x48, 2).len()
    });
    drop(x48);

    // decoder path: 16k LF -> 48k
    let x16 = stereo(frames / 3, 16_000, 3);
    stage("lf upsample 16k->48k", || {
        Resampler::new(16_000, 48_000).process(&x16, 2).len()
    });

    // streaming equivalents (chunk-fed, 1 s chunks)
    let x48s = stereo(frames, 48_000, 4);
    stage("stream normalize 44.1k->48k", || {
        let mut s = sena_dsp::StreamResampler::new(44_100, 48_000);
        let mut out = 0usize;
        for c in x44_2().chunks(44_100) {
            out += s.push(c, 2).len();
        }
        out += s.finish(2).len();
        out
    });
    stage("stream lf-down 48k->16k", || {
        let mut s = sena_dsp::StreamResampler::new(48_000, 16_000);
        let mut out = 0usize;
        for c in x48s.chunks(48_000) {
            out += s.push(c, 2).len();
        }
        out += s.finish(2).len();
        out
    });
    stage("stream lf-down 48k->32k", || {
        let mut s = sena_dsp::StreamResampler::new(48_000, 32_000);
        let mut out = 0usize;
        for c in x48s.chunks(48_000) {
            out += s.push(c, 2).len();
        }
        out += s.finish(2).len();
        out
    });
    stage("stream split @300", || {
        let mut s = sena_dsp::CrossoverStream::new(300.0);
        let mut out = 0usize;
        for c in x48s.chunks(48_000) {
            let (l, h) = s.push(c, 2);
            out += l.len() + h.len();
        }
        let (l, h) = s.finish(2);
        out + l.len() + h.len()
    });
}

fn x44_2() -> Vec<f64> {
    let frames = 44_100 * 30;
    let mut x = Vec::with_capacity(frames * 2);
    for i in 0..frames {
        for ch in 0..2 {
            let t = i as f64 / 44_100.0;
            let tone = (2.0 * std::f64::consts::PI * 130.0 * t).sin();
            x.push(tone * 0.8 + (ch as f64) * 0.01);
        }
    }
    x
}
