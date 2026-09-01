//! Resampler quality probe: reads raw f64 mono samples from stdin, writes
//! raw f64 mono at the output rate to stdout.
//! Usage: resample_probe <fs_in> <fs_out>

use sena_dsp::Resampler;
use std::io::{Read, Write};

fn main() {
    let args: Vec<String> = std::env::args().collect();
    let fs_in: u32 = args[1].parse().unwrap();
    let fs_out: u32 = args[2].parse().unwrap();
    let mut buf = Vec::new();
    std::io::stdin().read_to_end(&mut buf).unwrap();
    assert!(buf.len() % 8 == 0, "samples must be f64");
    let x: Vec<f64> = buf
        .chunks_exact(8)
        .map(|c| f64::from_le_bytes(c.try_into().unwrap()))
        .collect();
    let r = Resampler::new(fs_in, fs_out);
    let y = r.process_mono(&x, r.output_frames(x.len()));
    let mut out = Vec::with_capacity(y.len() * 8);
    for v in y {
        out.extend_from_slice(&v.to_le_bytes());
    }
    std::io::stdout().write_all(&out).unwrap();
}
