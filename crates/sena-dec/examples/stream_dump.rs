// Dump the streaming decode to a wav (f32le) for offline comparison.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let inp = std::env::args().nth(1).unwrap();
    let out = std::env::args().nth(2).unwrap();
    let bytes = std::fs::read(&inp).unwrap();
    let mut d = StreamingDecoder::open(Demuxed::parse(bytes).unwrap()).unwrap();
    let rate = d.info().sample_rate;
    let mut pcm: Vec<f32> = Vec::new();
    let mut buf = vec![0.0f32; 8192];
    loop {
        let n = d.read_f32(&mut buf, 4096).unwrap();
        if n == 0 { break; }
        pcm.extend_from_slice(&buf[..n as usize * 2]);
    }
    let mut w = Vec::with_capacity(44 + pcm.len() * 4);
    w.extend_from_slice(b"RIFF");
    w.extend_from_slice(&(36 + pcm.len() as u32 * 4).to_le_bytes());
    w.extend_from_slice(b"WAVEfmt ");
    w.extend_from_slice(&16u32.to_le_bytes());
    w.extend_from_slice(&3u16.to_le_bytes());
    w.extend_from_slice(&2u16.to_le_bytes());
    w.extend_from_slice(&rate.to_le_bytes());
    w.extend_from_slice(&(rate * 8).to_le_bytes());
    w.extend_from_slice(&8u16.to_le_bytes());
    w.extend_from_slice(&32u16.to_le_bytes());
    w.extend_from_slice(b"data");
    w.extend_from_slice(&(pcm.len() as u32 * 4).to_le_bytes());
    for v in pcm { w.extend_from_slice(&v.to_le_bytes()); }
    std::fs::write(&out, &w).unwrap();
    println!("wrote {out}");
}
