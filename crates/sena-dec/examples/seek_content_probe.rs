use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn decode_all(path: &str) -> Vec<f32> {
    let d = Demuxed::parse(std::fs::read(path).unwrap()).unwrap();
    let mut dec = StreamingDecoder::open(d).unwrap();
    let mut out = Vec::new();
    let mut buf = vec![0.0f32; 4800 * 2];
    loop {
        let n = dec.read_f32(&mut buf, 4800).unwrap();
        if n == 0 { break; }
        out.extend_from_slice(&buf[..(n as usize) * 2]);
    }
    out
}
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let full = decode_all(&path);
    let playable = (full.len() / 2) as u64;
    for &p in &[0.5f64, 0.9, 0.99] {
        let target = (playable as f64 * p) as u64;
        let d = Demuxed::parse(std::fs::read(&path).unwrap()).unwrap();
        let mut dec = StreamingDecoder::open(d).unwrap();
        dec.seek(target).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 4800 * 2];
        loop {
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..(n as usize) * 2]);
        }
        // skip the 1 s ramp/settle region, then align the next 0.4 s
        let seg = &got[(192000 * 2).min(got.len())..got.len().min((192000 + 48000) * 2)];
        let mut best = (0i64, -1.0f64);
        for shift in -4800i64..4800 {
            let mut num = 0.0; let mut da = 0.0; let mut db = 0.0;
            let base = target as i64 + shift;
            if base < 0 { continue; }
            let base = (base + 192000) * 2; // interleaved: frame -> sample index
            let n = seg.len().min((full.len() as i64 - base).max(0) as usize);
            if n < 960 { continue; }
            for i in 0..n {
                let a = full[base as usize + i] as f64;
                let b = seg[i] as f64;
                num += a * b; da += a * a; db += b * b;
            }
            let c = num / (da.sqrt() * db.sqrt() + 1e-30);
            if c > best.1 { best = (shift, c); }
        }
        println!("seek {:.2}%: best shift {} corr {:.5} (got {} frames)", p * 100.0, best.0, best.1, got.len() / 2);
    }
}
