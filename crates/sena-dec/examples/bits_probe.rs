use sena_dec::demux::Demuxed;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let d = Demuxed::parse(bytes).unwrap();
    let mut lf = 0usize; let mut mid_xhe = 0usize; let mut mid_opus = 0usize; let mut top: usize = 0;
    for b in &d.blocks {
        let blk = sena_core::es::decode_block(&b.data, d.init.mid_codec).unwrap();
        lf += blk.lf.iter().map(|a| a.len()).sum::<usize>();
        match blk.mid {
            sena_core::es::MidSection::Opus(p) => mid_opus += p.len(),
            sena_core::es::MidSection::Xhe { aus, .. } => mid_xhe += aus.iter().map(|a| a.len()).sum::<usize>(),
        }
        top += blk.hf.iter().map(|a: &&[u8]| a.len()).sum::<usize>();
    }
    let secs = d.init.playable_samples as f64 / 48000.0;
    let k = |v: usize| v as f64 * 8.0 / secs / 1000.0;
    println!("LF {:.1}k | mid_xhe {:.1}k | mid_opus {:.1}k | top {:.1}k | payload {:.1}k over {:.2}s",
        k(lf), k(mid_xhe), k(mid_opus), k(top), k(lf+mid_xhe+mid_opus+top), secs);
}
