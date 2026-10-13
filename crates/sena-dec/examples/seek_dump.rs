use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let target: u64 = std::env::args().nth(2).unwrap().parse().unwrap();
    let out = std::env::args().nth(3).unwrap();
    let naive = std::env::var_os("SENA_NAIVE_SEEK").is_some();
    eprintln!("naive={naive}");
    let d = Demuxed::parse(std::fs::read(&path).unwrap()).unwrap();
    let mut dec = StreamingDecoder::open(d).unwrap();
    dec.seek(target).unwrap();
    let mut pcm = Vec::new();
    let mut buf = vec![0.0f32; 4800 * 2];
    loop {
        let n = dec.read_f32(&mut buf, 4800).unwrap();
        if n == 0 { break; }
        pcm.extend_from_slice(&buf[..(n as usize) * 2]);
    }
    // write raw f32le
    let mut bytes = Vec::new();
    for v in pcm { bytes.extend_from_slice(&v.to_le_bytes()); }
    std::fs::write(out, bytes).unwrap();
}
