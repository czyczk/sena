// replicate cold_start_ramp's push feeding with LFAU dump
use sena_dec::es::{EsDecoder, BlockStore};
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let indexed = sena_dec::demux::index_container(&mut |pos, len| sena_dec::demux::mem_read_at(&bytes, pos, len), true).unwrap();
    let params = sena_dec::es::parse_params(indexed.init).unwrap();
    let blocks = indexed.blocks;
    let mut d = EsDecoder::open_push(params).unwrap();
    for b in &blocks[500..] {
        let data = &bytes[b.offset as usize..b.offset as usize + b.len as usize];
        d.push(b.t_ns, data).unwrap();
    }
    d.finish();
    let mut buf = vec![0.0f32; 8192];
    loop { if d.pull(&mut buf, 4096).unwrap() == 0 { break; } }
    println!("done");
}
