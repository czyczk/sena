// Wall time: batch decode + streaming decode of a .sena file.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
use std::time::Instant;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let secs = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap())
        .unwrap().info().playable_frames as f64
        / sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap().info().sample_rate as f64;
    let t0 = Instant::now();
    let d = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let tb = t0.elapsed().as_secs_f64();
    println!("batch:    {tb:.2}s ({:.1}x)", secs / tb);
    drop(d);
    let t0 = Instant::now();
    let mut dec = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let mut n_total = 0u64;
    let mut buf = vec![0.0f32; 8192];
    loop {
        let n = dec.read_f32(&mut buf, 4096).unwrap();
        if n == 0 { break; }
        n_total += n as u64;
    }
    let ts = t0.elapsed().as_secs_f64();
    println!("streaming: {ts:.2}s ({:.1}x, {} frames)", secs / ts, n_total);
}
