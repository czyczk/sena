use sena_dec::demux::Demuxed;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let seek_frame: usize = std::env::args().nth(2).unwrap().parse().unwrap();
    let demux = Demuxed::parse(std::fs::read(path).unwrap()).unwrap();
    let lf = demux.tracks.iter().find(|t| t.codec_id == "A_SENALF").unwrap();
    let lf_rate = lf.sample_rate.round() as usize;
    let asc = rxaac_dec_lib::asc::AudioSpecificConfig::parse(&lf.codec_private).unwrap();
    let aus: Vec<&[u8]> = demux.frames.iter().filter(|f| f.track == lf.number).map(|f| f.data.as_slice()).collect();
    let target_core = seek_frame * lf_rate / 48_000;
    let au = target_core / 1024;
    let mut indep = au;
    while indep > 0 && aus[indep][0] >> 7 == 0 { indep -= 1; }
    let decode_from = |from: usize| -> Vec<f32> {
        let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(&asc).unwrap();
        let mut pcm = Vec::new();
        for a in aus.iter().skip(from) {
            let mut out = Vec::new();
            if dec.decode_au(a, &mut out).is_err() { out.clear(); out.resize(2048, 0.0); }
            pcm.extend_from_slice(&out);
        }
        pcm[(au - from) * 2048..].to_vec()
    };
    let continuous = decode_from(0);
    let win = lf_rate / 50 * 2;
    for back in [0usize, 4, 8, 9, 10, 11, 12, 14, 16, 24, 32] {
        let from = indep.saturating_sub(back);
        let jumped = decode_from(from);
        let n = continuous.len().min(jumped.len());
        let s = 0;
        let e = (16 * win).min(n);
        let max_early = continuous[s..e].iter().zip(&jumped[s..e]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max);
        let s2 = 48 * win;
        let tail = if s2 < n { continuous[s2..n].iter().zip(&jumped[s2..n]).map(|(a, b)| (a - b).abs()).fold(0.0f32, f32::max) } else { 0.0 };
        println!("back={back:2} (from au {from}): first-320ms max {max_early:.1e}, tail {tail:.1e}");
    }
}
