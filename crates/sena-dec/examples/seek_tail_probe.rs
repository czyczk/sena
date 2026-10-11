use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let d = Demuxed::parse(bytes).unwrap();
    let playable = d.init.playable_samples;
    for frac in [0.5, 0.9, 0.99] {
        let t = (playable as f64 * frac) as u64;
        let mut dec = StreamingDecoder::open(Demuxed::parse(std::fs::read(&path).unwrap()).unwrap()).unwrap();
        dec.seek(t).unwrap();
        let mut buf = vec![0.0f32; 9600 * 2];
        let got = dec.read_f32(&mut buf, 9600).unwrap();
        println!("seek {:.2}% ({}): got {}", frac * 100.0, t, got);
    }
    for back in [960u64, 480, 100, 10] {
        let t = playable - back;
        let mut dec = StreamingDecoder::open(Demuxed::parse(std::fs::read(&path).unwrap()).unwrap()).unwrap();
        dec.seek(t).unwrap();
        let mut buf = vec![0.0f32; 9600 * 2];
        let mut total = 0u64;
        let mut first_start = u64::MAX;
        loop {
            let got = dec.read_f32(&mut buf, 9600).unwrap();
            if got > 0 && first_start == u64::MAX { first_start = dec.read_info().start_frame; }
            total += got;
            if got == 0 { break; }
        }
        println!("seek end-{}: total {} start {} (expected {})", back, total, first_start, t);
    }
}
