// 44.1 kHz-output stream acceptance: streaming == batch bit-exact, plus
// seek-vs-continuous residuals at several positions.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).expect("usage: check_44k <file.sena>");
    let bytes = std::fs::read(&path).unwrap();
    let whole = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let info = whole.info();
    println!("rate {} playable {}", info.sample_rate, info.playable_frames);
    assert_eq!(info.sample_rate, 44100, "not a 44.1k-output stream");
    let wr: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    // streaming full decode vs batch
    let mut d = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let mut sp = Vec::new();
    let mut buf = vec![0.0f32; 8192];
    loop {
        let n = d.read_f32(&mut buf, 4096).unwrap();
        if n == 0 { break; }
        sp.extend_from_slice(&buf[..n as usize * 2]);
    }
    let mut mx = 0.0f32;
    for (a, b) in wr.iter().zip(sp.iter()) { mx = mx.max((a - b).abs()); }
    println!("stream vs batch: len {} vs {}, maxdiff {mx}", wr.len(), sp.len());
    // seeks
    let playable = info.playable_frames;
    for frac in [4u64, 3, 2, 5, 7] {
        let target = playable / frac;
        let mut d = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        d.seek(target).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 8192];
        loop {
            let n = d.read_f32(&mut buf, 4096).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..n as usize * 2]);
        }
        let mut d0 = 0.0f32;
        let mut d1 = 0.0f32;
        for (i, &v) in got.iter().enumerate() {
            let r = wr[target as usize * 2 + i];
            let diff = (v - r).abs();
            if i < 4410 * 2 { d0 = d0.max(diff); }
            d1 = d1.max(diff);
        }
        println!("seek 1/{frac} (frame {target}): first-100ms {d0:.5}, total {d1:.5}");
    }
}
