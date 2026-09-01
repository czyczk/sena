//! Reproduce foobar tag flows at the Rust level: write entries through
//! rewrite_user_tags (what sena_file_write_tags does), then re-parse/probe.
use sena_dec::demux::Demuxed;
use sena_dec::tags::{rewrite_user_tags, scan_user_tags};

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let original = std::fs::read(&path).unwrap();

    // 1. RG write (plain strings)
    let mut entries: Vec<(String, String)> = vec![
        ("REPLAYGAIN_TRACK_GAIN".into(), "-6.53 dB".into()),
        ("REPLAYGAIN_TRACK_PEAK".into(), "0.988250".into()),
        ("TITLE".into(), "Some Song".into()),
    ];
    // 2. foobar attached-picture meta (binary-ish value with embedded NUL)
    let mut pic = Vec::new();
    pic.extend_from_slice(&0u8.to_le_bytes()); // type id
    pic.extend_from_slice(b"image/jpeg|");
    pic.push(b'\0'); // embedded NUL like foobar's PICTURE meta
    pic.extend_from_slice(b"\xff\xd8\xff\xe0binary-jpeg-data");
    entries.push(("PICTURE".into(), String::from_utf8_lossy(&pic).into_owned()));

    let rewritten = rewrite_user_tags(&original, &entries).unwrap();
    let demux = Demuxed::parse(rewritten.clone()).unwrap();
    assert_eq!(demux.immutable_tag("SENA_PROFILE"), Some("300"));
    let (info, _w) = sena_dec::pipeline::probe(&demux).unwrap();
    eprintln!("after rewrite: probe OK playable={}", info.playable_frames);
    let mut served = 0usize;
    let bytes = rewritten.clone();
    let mut read_at = |pos: u64, len: usize| -> Result<Vec<u8>, String> {
        served += len;
        bytes.get(pos as usize..pos as usize + len).map(|s| s.to_vec()).ok_or_else(|| "range".into())
    };
    let back = scan_user_tags(&mut read_at).unwrap();
    eprintln!("read back {} entries: {:?}", back.len(), back.iter().map(|(k, _)| k.as_str()).collect::<Vec<_>>());
    // decode must still work
    match sena_dec::pipeline::decode(&demux) {
        Ok(d) => eprintln!("decode OK, playable {}", d.info().playable_frames),
        Err(e) => eprintln!("decode ERR: {e}"),
    }
}
