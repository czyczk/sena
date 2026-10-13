// Wall-time of a single seek + first 100 ms of production.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let playable = {
        let d = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        let n = d.info().playable_frames;
        d
    };
    let _ = playable;
    for frac in [4usize, 3, 7] {
        let target = 10156800u64 * 0 + 1; // placeholder replaced below
        let _ = target;
        let target = (10156800 / frac as u64) as u64;
        let t0 = std::time::Instant::now();
        let mut dec = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        dec.seek(target).unwrap();
        let mut buf = vec![0.0f32; 4800 * 2];
        let mut got = 0usize;
        while got < 4800 {
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got += n as usize;
        }
        println!("seek to 1/{frac}: {:?} (open+seek+first 100ms)", t0.elapsed());
    }
}
