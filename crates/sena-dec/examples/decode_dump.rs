//! Dump the streaming-decoder output as interleaved f32 LE on stdout.
//! Used by tests/ffmpeg to produce the deterministic reference PCM that the
//! ffmpeg demuxer (which drives the same StreamingDecoder through the C ABI)
//! must reproduce bit-exactly.
//!
//! usage: decode_dump <file.sena> [block_frames] [--seek <frame>]
use std::io::Write;
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("usage: decode_dump <file.sena> [block_frames] [--seek <frame>]").clone();
    let block: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let seek: Option<u64> = args
        .iter()
        .position(|s| s == "--seek")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse().unwrap());
    let bytes = std::fs::read(path).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let mut dec = StreamingDecoder::open(demux).unwrap();
    if let Some(frame) = seek {
        dec.seek(frame).unwrap();
    }
    let stdout = std::io::stdout();
    let mut out = stdout.lock();
    let mut buf = vec![0.0f32; block * 2];
    loop {
        let got = dec.read_f32(&mut buf, block as u64).unwrap();
        if got == 0 {
            break;
        }
        let bytes: &[u8] = unsafe {
            std::slice::from_raw_parts(buf.as_ptr().cast::<u8>(), got as usize * 2 * 4)
        };
        out.write_all(bytes).unwrap();
    }
}

