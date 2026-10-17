// Measure the AU emission model of a dumped m4a stream (dump_m4a output):
// AU0/steady widths, restart classes from fresh starts at indep AUs,
// indep spacing stats. Used to verify the FhG CBR128@44.1k HF stream
// matches the mid lane's measured model.
use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;

fn main() {
    let path = std::env::args().nth(1).expect("usage: au_model_probe <dump.bin>");
    let b = std::fs::read(&path).unwrap();
    let asc_len = u16::from_le_bytes(b[0..2].try_into().unwrap()) as usize;
    let asc = &b[2..2 + asc_len];
    let mut p = 2 + asc_len;
    let n = u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as usize;
    p += 4;
    let mut aus = Vec::new();
    for _ in 0..n {
        let l = u32::from_le_bytes(b[p..p + 4].try_into().unwrap()) as usize;
        p += 4;
        aus.push(&b[p..p + l]);
        p += l;
    }
    let parsed = AudioSpecificConfig::parse(asc).unwrap();
    println!("AUs: {n}");
    // full-run widths
    let mut dec = UsacDecoder::new(&parsed).unwrap();
    let mut out = Vec::new();
    let mut widths = Vec::new();
    for au in &aus {
        out.clear();
        dec.decode_au(au, &mut out).unwrap();
        widths.push(out.len() / 2);
    }
    println!("widths[0..6]: {:?}", &widths[..6]);
    let steady: std::collections::BTreeMap<usize, usize> =
        widths[1..].iter().fold(Default::default(), |mut m, &w| { *m.entry(w).or_insert(0) += 1; m });
    println!("steady width histogram: {:?}", steady);
    // indep positions
    let indeps: Vec<usize> = aus.iter().enumerate()
        .filter(|(_, au)| au[0] & 0x80 != 0).map(|(i, _)| i).collect();
    let gaps: Vec<usize> = indeps.windows(2).map(|w| w[1] - w[0]).collect();
    println!("indep count {} first {:?} gap min/max/mean: {}/{}/{:.1}",
        indeps.len(), &indeps[..indeps.len().min(8)],
        gaps.iter().min().unwrap_or(&0), gaps.iter().max().unwrap_or(&0),
        gaps.iter().sum::<usize>() as f64 / gaps.len().max(1) as f64);
    // fresh starts at a few indep AUs: width of first three AUs + total span
    for &k in indeps.iter().take(6) {
        let mut d2 = UsacDecoder::new(&parsed).unwrap();
        let mut ws = Vec::new();
        for au in &aus[k..(k + 3).min(aus.len())] {
            out.clear();
            d2.decode_au(au, &mut out).unwrap();
            ws.push(out.len() / 2);
        }
        println!("fresh from AU{k}: widths {ws:?}");
    }
}
