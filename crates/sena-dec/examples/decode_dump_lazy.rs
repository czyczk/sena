//! decode_dump variant driving the bounded-memory lazy path (the same store
//! the C ABI uses for seekable input), for isolating store-dependent decode
//! differences in tests.
//!
//! usage: decode_dump_lazy <file.sena> [block_frames] [--seek <frame>]
use std::io::Write;
use sena_dec::stream::{FrameStore, StreamingDecoder};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let path = args.first().expect("usage: decode_dump_lazy <file.sena> [block_frames] [--seek <frame>] [--pread <frames>]").clone();
    let block: usize = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(1024);
    let seek: Option<u64> = args
        .iter()
        .position(|s| s == "--seek")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse().unwrap());
    // --pread N: read N frames before seeking (reproduces ffmpeg's
    // find_stream_info probe-then-seek sequence).
    let pread: u64 = args
        .iter()
        .position(|s| s == "--pread")
        .and_then(|i| args.get(i + 1))
        .map(|s| s.parse().unwrap())
        .unwrap_or(0);
    let bytes = std::fs::read(path).unwrap();
    let mut indexed = sena_dec::demux::index_container(
        &mut |pos: u64, len: usize| sena_dec::demux::mem_read_at(&bytes, pos, len),
        true,
    )
    .unwrap();
    let frames = std::mem::take(&mut indexed.frames);
    let read_at = Box::new(move |pos: u64, len: usize| sena_dec::demux::mem_read_at(&bytes, pos, len));
    let store = FrameStore::Lazy { frames, read_at, window: (0, Vec::new()) };
    let mut dec = StreamingDecoder::open_indexed(indexed, store).unwrap();
    if pread > 0 {
        let mut scratch = vec![0.0f32; pread as usize * 2];
        dec.read_f32(&mut scratch, pread).unwrap();
    }
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
