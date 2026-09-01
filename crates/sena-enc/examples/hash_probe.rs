//! Hash diagnostics: batch-read PCM -> audio_sha256_of.
use sena_enc::{audio_sha256_of, wav};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let (x, ch, rate) = wav::read_f64_bytes(&bytes).unwrap();
    eprintln!("rate {rate} ch {ch} frames {}", x.len() / ch);
    println!("batch: {}", audio_sha256_of(&x));
}
