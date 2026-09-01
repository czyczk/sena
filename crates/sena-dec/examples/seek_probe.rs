//! Seek diagnostics: jump-seek timing and correctness vs the full decode.

use sena_dec::demux::Demuxed;
use std::time::Instant;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_probe <file.sena>");
    let bytes = std::fs::read(&path).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let playable: u64 = demux.tag("SENA_PLAYABLE_SAMPLES").unwrap().parse().unwrap();
    eprintln!(
        "lf AUs={} hf packets={} lf_rate={}",
        demux.frames_for(demux.track("A_SENALF").unwrap().number).count(),
        demux.frames_for(demux.track("A_OPUS").unwrap().number).count(),
        demux.track("A_SENALF").unwrap().sample_rate
    );

    // full reference decode (batch)
    let mut ref_dec = sena_dec::pipeline::Decoder::open(&demux).unwrap();
    let mut ref_all = Vec::new();
    let mut ref_buf = vec![0.0f32; 48000 * 2];
    let mut r = 0u64;
    while r < playable {
        let got = ref_dec.read_f32(&mut ref_buf, 48000).unwrap();
        ref_all.extend_from_slice(&ref_buf[..got as usize * 2]);
        r += got;
    }
    eprintln!("reference decode: {} samples", ref_all.len());

    for target in [playable / 2, playable * 4 / 5, playable - 48000] {
        let bytes2 = std::fs::read(&path).unwrap();
        let demux2 = Demuxed::parse(bytes2).unwrap();
        let mut dec = sena_dec::stream::StreamingDecoder::open(demux2).unwrap();
        let t0 = Instant::now();
        dec.seek(target).unwrap();
        let t1 = Instant::now();
        let mut buf = vec![0.0f32; 48000 * 2];
        let got = dec.read_f32(&mut buf, 48000).unwrap();
        let t2 = Instant::now();
        let mut maxe = 0.0f32;
        for i in 0..got as usize * 2 {
            let d = (buf[i] - ref_all[target as usize * 2 + i]).abs();
            if d > maxe {
                maxe = d;
            }
        }
        // steady-state comparison after the first 50 ms (LF resampler context)
        let mut maxe_mid = 0.0f32;
        for i in 4800 * 2..got as usize * 2 {
            let d = (buf[i] - ref_all[target as usize * 2 + i]).abs();
            if d > maxe_mid {
                maxe_mid = d;
            }
        }
        eprintln!(
            "seek {target}: {:.3}s (read {got} in {:.3}s); max diff {maxe:.4}; mid diff {maxe_mid:.6}",
            (t1 - t0).as_secs_f64(),
            (t2 - t1).as_secs_f64()
        );
    }
}
