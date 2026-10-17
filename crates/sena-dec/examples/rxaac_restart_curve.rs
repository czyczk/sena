// Does a bare rxaac restart at an indep AU ever bit-converge to the
// continuous decode? corr in windows at increasing offsets from the
// restart AU, for several restart points.
use sena_dec::demux::Demuxed;
fn corr(a: &[f32], b: &[f32]) -> f64 {
    let mut num = 0.0; let mut da = 0.0; let mut db = 0.0;
    for i in 0..a.len() { num += a[i] as f64 * b[i] as f64; da += (a[i] as f64).powi(2); db += (b[i] as f64).powi(2); }
    num / (da.sqrt() * db.sqrt() + 1e-30)
}
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let d = Demuxed::parse(bytes).unwrap();
    let asc = d.init.mid_head.clone();
    let mut aus: Vec<&[u8]> = Vec::new();
    for b in &d.blocks {
        if let Ok(blk) = sena_core::es::decode_block(&b.data, d.init.mid_codec, d.init.hf_codec) {
            if let sena_core::es::MidSection::Xhe { aus: a, .. } = blk.mid { aus.extend(a); }
        }
    }
    let parse = || rxaac_dec_lib::asc::AudioSpecificConfig::parse(&asc).unwrap();
    let mut cont_dec = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
    let mut cont = Vec::new();
    for au in &aus {
        let mut out = Vec::new();
        cont_dec.decode_au(au, &mut out).unwrap();
        cont.extend_from_slice(&out);
    }
    let indep: Vec<usize> = (0..aus.len()).filter(|&i| aus[i].first().map_or(false, |b| b & 0x80 != 0)).collect();
    println!("indeps: {}", indep.len());
    for &start in indep.iter().skip(indep.len()/3).take(3) {
        let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
        let mut rst = Vec::new();
        for au in &aus[start..] {
            let mut out = Vec::new();
            dec.decode_au(au, &mut out).unwrap();
            rst.extend_from_slice(&out);
        }
        print!("restart au {start}:");
        for off_s in [0.25f64, 1.0, 2.0, 4.0, 8.0, 16.0, 32.0] {
            let off = (off_s * 44100.0) as usize;
            let n = 44100.min(rst.len()/2 - off).min(cont.len()/2 - (start*1024 + off));
            if n < 4000 { break; }
            let a = &cont[(start*1024 + off)*2..(start*1024 + off + n)*2];
            let b = &rst[off*2..(off+n)*2];
            print!("  {}s={:.5}", off_s, corr(a, b));
        }
        println!();
    }
}
