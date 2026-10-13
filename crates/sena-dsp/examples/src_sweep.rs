// SRC quality probe: roundtrip 48k -> 44.1k -> 48k through sena-dsp
// resamplers (default full-Nyquist spec and the 13 kHz band-limited spec),
// writing multi-tone / sweep / out-of-band-probe WAVs for analysis.
use std::f64::consts::PI;

fn tone(freq: f64, n: usize) -> Vec<f64> {
    (0..n).map(|i| (2.0 * PI * freq * i as f64 / 48000.0).sin()).collect()
}

fn main() {
    let dir = std::env::args().nth(1).unwrap_or("/tmp/src_probe".into());
    std::fs::create_dir_all(&dir).unwrap();
    let n = 48000 * 10;
    // per-tone files are cleaner for exact gain measurement
    let freqs = [100.0, 500.0, 1000.0, 2000.0, 4000.0, 6000.0, 8000.0, 10000.0, 11000.0, 12000.0,
                 13000.0, 14000.0, 16000.0, 18000.0, 20000.0, 21000.0, 22000.0, 23000.0, 24000.0];
    for spec in [("default", None), ("passband13k", Some(13_000.0))] {
        for f in freqs {
            let x = tone(f, n);
            let down = match spec.1 {
                None => sena_dsp::Resampler::new(48000, 44100).process(&x, 1),
                Some(p) => sena_dsp::Resampler::new_with_passband(48000, 44100, p).process(&x, 1),
            };
            let up = match spec.1 {
                None => sena_dsp::Resampler::new(44100, 48000).process(&down, 1),
                Some(p) => sena_dsp::Resampler::new_with_passband(44100, 48000, p).process(&down, 1),
            };
            let bytes: Vec<u8> = up.iter().flat_map(|v| (*v as f32).to_le_bytes()).collect();
            std::fs::write(format!("{}/{}_{}.raw", dir, spec.0, f as u32), bytes).unwrap();
        }
    }
    println!("wrote {} tones x2 specs to {dir}", freqs.len());
}
