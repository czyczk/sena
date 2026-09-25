//! Build a plugin test fixture: copy a .sena adding user tags and/or cover
//! art (Matroska Attachments) through the same rewrite paths the C ABI uses.
//!
//! usage: make_fixture <in.sena> <out.sena> [--art <name> <mime> <file>] [KEY=VALUE ...]
use sena_dec::attachments::{Attachment, rewrite_attachments};
use sena_dec::tags::rewrite_user_tags;

fn main() {
    let mut args = std::env::args().skip(1);
    let input = args.next().expect("input .sena");
    let output = args.next().expect("output .sena");
    let mut entries: Vec<(String, String)> = Vec::new();
    let mut arts: Vec<Attachment> = Vec::new();
    let mut rest: Vec<String> = args.collect();
    if let Some(i) = rest.iter().position(|a| a == "--art") {
        let tail = rest.split_off(i);
        let mut it = tail.into_iter().skip(1);
        while let (Some(name), Some(mime), Some(file)) = (it.next(), it.next(), it.next()) {
            let data = std::fs::read(&file).unwrap();
            arts.push(Attachment::new(&name, &mime, data));
        }
    }
    for a in rest.drain(..) {
        let (k, v) = a.split_once('=').expect("KEY=VALUE");
        entries.push((k.to_string(), v.to_string()));
    }

    let mut bytes = std::fs::read(&input).unwrap();
    if !entries.is_empty() {
        bytes = rewrite_user_tags(&bytes, &entries).unwrap();
    }
    if !arts.is_empty() {
        bytes = rewrite_attachments(&bytes, &arts).unwrap();
    }
    // The fixture must stay fully decodable.
    let demux = sena_dec::demux::Demuxed::parse(bytes.clone()).unwrap();
    let (info, _) = sena_dec::pipeline::probe(&demux).unwrap();
    std::fs::write(&output, &bytes).unwrap();
    eprintln!(
        "wrote {output}: playable={} tags={} art={}",
        info.playable_frames,
        entries.len(),
        arts.len()
    );
}
