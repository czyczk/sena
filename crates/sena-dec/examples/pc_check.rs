// Seek the 48k xHE-mid asset at many positions; SENA_DEBUG_PC prints each
// phase-lock correction: post-correction residue must be 0.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let playable = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap())
        .unwrap().info().playable_frames;
    for i in 1..24 {
        let target = playable * i / 24;
        let mut d = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        d.seek(target).unwrap();
        let mut buf = vec![0.0f32; 8192];
        let _ = d.read_f32(&mut buf, 4096).unwrap(); // one chunk is enough
    }
}
