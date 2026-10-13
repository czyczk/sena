// Wall-time of open + seek + first 100 ms of production, per position.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_bench <file.sena>");
    let bytes = std::fs::read(&path).unwrap();
    let playable = {
        let d = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        d.info().playable_frames
    };
    for frac in [4u64, 3, 7] {
        let target = playable / frac;
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
