// Dump an m4a's ASC + AU payloads to a flat probe file:
// [asc_len u16 LE][asc][au_count u32 LE][au_len u32 LE][au]...
use std::io::Write;
fn main() {
    let inp = std::env::args().nth(1).expect("usage: dump_m4a <in.m4a> <out.bin>");
    let out = std::env::args().nth(2).unwrap();
    let (asc, aus) = sena_enc::extract_m4a(std::path::Path::new(&inp)).unwrap();
    let mut b = Vec::new();
    b.extend_from_slice(&(asc.len() as u16).to_le_bytes());
    b.extend_from_slice(&asc);
    b.extend_from_slice(&(aus.len() as u32).to_le_bytes());
    for au in &aus {
        b.extend_from_slice(&(au.len() as u32).to_le_bytes());
        b.extend_from_slice(au);
    }
    std::fs::File::create(&out).unwrap().write_all(&b).unwrap();
    println!("{} AUs, asc {}B -> {}", aus.len(), asc.len(), out);
}
