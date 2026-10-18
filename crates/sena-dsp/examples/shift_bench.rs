// Micro-bench: ssb shift_up cost per call at 44.1k vs 48k, tx-window sizes.
use std::time::Instant;
fn main() {
    for (rate, n) in [(44100u32, 9279usize), (48000, 9600)] {
        let x = vec![0.5f64; n * 2];
        // warmup
        let _ = sena_dsp::shift_up_offset(&x, 2, 12000.0, rate, 0);
        let t0 = Instant::now();
        let reps = 50;
        for _ in 0..reps {
            let _ = sena_dsp::shift_up_offset(&x, 2, 12000.0, rate, 0);
        }
        let dt = t0.elapsed().as_secs_f64() / reps as f64 * 1000.0;
        println!("{rate}Hz n={n}: {dt:.2} ms/call");
    }
}
