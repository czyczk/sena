// rxaac-only seek-formula validator: decode the xHE mid stream from
// prev_indep(au) as one continuous restart stream, skip `skip` core samples
// into it, and compare against the continuous decode at the same playable
// position. Validates the trim formula and the 1024/2048 restart-class
// adjustment.
use sena_dec::demux::Demuxed;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(path).unwrap();
    let d = Demuxed::parse(bytes).unwrap();
    let asc = d.init.mid_head.clone();
    let mut aus: Vec<&[u8]> = Vec::new();
    for b in &d.blocks {
        if let Ok(blk) = sena_core::es::decode_block(&b.data, d.init.mid_codec, d.init.hf_codec) {
            if let sena_core::es::MidSection::Xhe { aus: a, .. } = blk.mid {
                aus.extend(a);
            }
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
        out_all[1024 * 2..].to_vec() // drop the stream-start warmup
    };
    let indep = |i: usize| aus[i].first().map_or(false, |b| b & 0x80 != 0);
    let rate = 44100u64;
    println!("  au | start | first-len | corr(current) | corr(class-adj)");
    for frac in [0.25f64, 0.5, 0.75] {
        let target48 = (d.init.playable_samples as f64 * frac) as u64;
        let c = target48 * rate / 48000;
        let target_raw = c + 1024;
        let au = target_raw / 1024;
        let mut start = au;
        while start > 0 && !indep(start as usize) { start -= 1; }
        let trim = (target_raw - start * 1024) as usize;
        // one continuous restart stream from `start`
        let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&parse()).unwrap();
        let mut out_all = Vec::new();
        for au2 in &aus[start as usize..] {
            let mut out = Vec::new();
            dec.decode_au(au2, &mut out).unwrap();
            out_all.extend_from_slice(&out);
        }
        let corr_at = |adj: isize| -> f64 {
            let skip = (trim as isize + adj).max(0) as usize;
            let off = 11025usize;
            let n = 11025;
            let ci = c as usize + off;
            let si = skip + off;
            let mut num = 0.0; let mut da = 0.0; let mut db = 0.0;
            for i in 0..n {
                let a = cont[(ci + i) * 2] as f64;
                let b = out_all[(si + i) * 2] as f64;
                num += a * b; da += a * a; db += b * b;
            }
            num / (da.sqrt() * db.sqrt() + 1e-30)
        };
        println!("{:5} | {:5} | {:9} | {:.5} | {:.5}",
            au, start, 0, corr_at(0), corr_at(-1024));
    }
}
