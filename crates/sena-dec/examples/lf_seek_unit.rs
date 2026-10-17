// Same validation as mx_seek_unit but for the LF (exhale) lane.
use sena_dec::demux::Demuxed;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let d = Demuxed::parse(bytes).unwrap();
    let asc = d.init.lf_asc.clone();
    let rate = d.init.lf_rate as u64;
    let mut aus: Vec<&[u8]> = Vec::new();
    for b in &d.blocks {
        if let Ok(blk) = sena_core::es::decode_block(&b.data, d.init.mid_codec, d.init.hf_codec) {
            aus.extend(blk.lf);
        }
    }
    let parse = || rxaac_dec_lib::asc::AudioSpecificConfig::parse(&asc).unwrap();
    let cont = {
        let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
        let mut out_all = Vec::new();
        for au in &aus {
            let mut out = Vec::new();
            dec.decode_au(au, &mut out).unwrap();
            out_all.extend_from_slice(&out);
        }
        out_all[1024 * 2..].to_vec()
    };
    let indep = |i: usize| aus[i].first().map_or(false, |b| b & 0x80 != 0);
    println!("  au | start | first-len | corr(current) | corr(class-adj)");
    for frac in [0.049f64, 0.05, 0.051] {
        let playable = d.init.playable_samples;
        let target48 = (playable as f64 * frac) as u64;
        let c = target48 * rate / 48000;
        let target_raw = c + 1024;
        let au = target_raw / 1024;
        let mut start = au;
        while start > 0 && !indep(start as usize) { start -= 1; }
        let trim = (target_raw - start * 1024) as usize;
        let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
        let mut out_all = Vec::new();
        for au2 in &aus[start as usize..] {
            let mut out = Vec::new();
            dec.decode_au(au2, &mut out).unwrap();
            out_all.extend_from_slice(&out);
        }
        let _first_len = out_all.len() / 2; // includes the restart AU's emission
        let fl0 = {
            let mut d2 = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
            let mut o = Vec::new();
            d2.decode_au(aus[start as usize], &mut o).unwrap();
            o.len() / 2
        };
        let corr_at = |adj: isize| -> f64 {
            let skip = (trim as isize + adj).max(0) as usize;
            let off = 11025usize.min(c as usize / 4);
            let n = 11025usize;
            let ci = c as usize + off;
            let si = skip + off;
            if ci + n > cont.len() / 2 || si + n > out_all.len() / 2 { return f64::NAN; }
            let mut num = 0.0; let mut da = 0.0; let mut db = 0.0;
            for i in 0..n {
                let a = cont[(ci + i) * 2] as f64;
                let b = out_all[(si + i) * 2] as f64;
                num += a * b; da += a * a; db += b * b;
            }
            num / (da.sqrt() * db.sqrt() + 1e-30)
        };
        println!("{:5} | {:5} | {:9} | {:.5} | {:.5}",
            au, start, fl0, corr_at(0), corr_at(-1024));
    }
}
