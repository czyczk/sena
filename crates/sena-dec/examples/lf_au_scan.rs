//! Scan LF AUs of a .sena file: report any AU whose decoded length differs
//! from the plain 1024 core samples (a signal of embedded AudioPreroll),
//! plus the independency-flag map around it.
use sena_dec::demux::Demuxed;
fn main() {
    let path = std::env::args().nth(1).expect("file.sena");
    let demux = Demuxed::parse(std::fs::read(&path).unwrap()).unwrap();
    let asc = rxaac_dec_lib::asc::AudioSpecificConfig::parse(&demux.init.lf_asc).unwrap();
    let mut aus: Vec<&[u8]> = Vec::new();
    for b in &demux.blocks {
        let block = sena_core::es::decode_block(&b.data, sena_core::MidCodec::Opus, sena_core::HfCodec::Opus).unwrap();
        aus.extend(block.lf);
    }
    let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&asc).unwrap();
    let mut odd = 0usize;
    let mut indep_run = 0usize;
    let mut max_indep_run = 0usize;
    for (i, au) in aus.iter().enumerate() {
        let mut out = Vec::new();
        dec.decode_au(au, &mut out).unwrap();
        let n = out.len() / 2;
        let indep = au[0] >> 7 != 0;
        if indep { indep_run += 1; max_indep_run = max_indep_run.max(indep_run); } else { indep_run = 0; }
        if n != 1024 {
            odd += 1;
            println!("AU {i}: {n} core samples (indep={indep})");
        }
    }
    println!("{}: {} AUs, {} odd-length AUs, longest indep-AU run {max_indep_run}", path, aus.len(), odd);
}
