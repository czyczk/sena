use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
use std::time::Instant;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let d = Demuxed::parse(std::fs::read(&path).unwrap()).unwrap();
    let playable = d.init.playable_samples;
    let target = playable / 2;
    let mut dec = StreamingDecoder::open(d).unwrap();
    let t0 = Instant::now();
    dec.seek(target).unwrap();
    let mut buf = vec![0.0f32; 9600 * 2];
    let n = dec.read_f32(&mut buf, 9600).unwrap();
    let ri = dec.read_info();
    println!("seek+first-read wall: {:.3}s, got {} frames starting at {} (target {})",
        t0.elapsed().as_secs_f64(), n, ri.start_frame, target);
}
