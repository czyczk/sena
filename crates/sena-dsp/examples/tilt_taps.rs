fn main() {
    let h = sena_dsp::tilt_taps(100.0);
    for v in h {
        println!("{v:.17e}");
    }
}
