//! Diagnostic: print payload bits reported for 100 ms read blocks.
use sena_dec::demux::Demuxed;
use sena_dec::pipeline::Decoder;

fn main() {
    let path = std::env::args().nth(1).expect("usage: bitrate_probe <file.sena>");
    let bytes = std::fs::read(path).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let mut dec = Decoder::open(&demux).unwrap();
    let frames_per_block = 4800u64; // 100 ms @48k
    let mut buf = vec![0.0f32; (frames_per_block * 2) as usize];
    let mut idx = 0u64;
    while let Ok(n) = dec.read_f32(&mut buf, frames_per_block) {
        if n == 0 { break; }
        let ri = dec.read_info();
        let kbps = ri.payload_bits as f64 / (ri.frames as f64 / 48000.0) / 1000.0;
        println!("block {} start {} frames {} bits {} kbps {:.1}", idx, ri.start_frame, ri.frames, ri.payload_bits, kbps);
        idx += 1;
    }
}
