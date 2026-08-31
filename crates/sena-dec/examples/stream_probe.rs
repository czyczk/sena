use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn main() {
    let path = std::env::args().nth(1).expect("usage: stream_probe <file.sena>");
    let bytes = std::fs::read(path).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let mut dec = StreamingDecoder::open(demux).unwrap();
    let playable = dec.info().playable_frames;
    let mut total = 0u64;
    let mut bits = 0u64;
    let mut buf = vec![0.0f32; 4800 * 2];
    let mut first_bits = None;
    loop {
        let got = dec.read_f32(&mut buf, 4800).unwrap();
        if got == 0 { break; }
        let ri = dec.read_info();
        if first_bits.is_none() { first_bits = Some((ri.frames, ri.payload_bits)); }
        total += got;
        bits += ri.payload_bits;
    }
    println!("playable={playable} total={total} bits={bits} first={first_bits:?}");
    assert_eq!(total, playable);

    // seek to the final 100 frames and confirm they survive the seek
    dec.seek(playable - 100).unwrap();
    let got = dec.read_f32(&mut buf, 4800).unwrap();
    let ri = dec.read_info();
    println!("tail got={got} start={} bits={}", ri.start_frame, ri.payload_bits);
    assert_eq!(got, 100);
    assert_eq!(ri.start_frame, playable - 100);
    assert_eq!(dec.read_f32(&mut buf, 4800).unwrap(), 0);
}
