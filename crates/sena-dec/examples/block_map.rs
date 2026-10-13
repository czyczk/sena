// Print container block boundaries (t_ns -> seconds) and the divergence
// onset after each n/8 seek anchor: first 0.5s window where corr < 0.95.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn corr(a: &[f64], b: &[f64]) -> f64 {
    let n = a.len() as f64;
    let (mut sab, mut sa, mut sb, mut saa, mut sbb) = (0.0, 0.0, 0.0, 0.0, 0.0);
    for i in 0..a.len() {
        sab += a[i] * b[i]; sa += a[i]; sb += b[i]; saa += a[i] * a[i]; sbb += b[i] * b[i];
    }
    (sab - sa * sb / n) / ((saa - sa * sa / n) * (sbb - sb * sb / n)).sqrt().max(1e-12)
}

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let d = Demuxed::parse(bytes.clone()).unwrap();
    let whole = sena_dec::pipeline::decode(&d).unwrap();
    let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    let playable = whole.info().playable_frames as usize;
    drop(whole);
    // block boundaries via the demuxed block list
    let mut bnds: Vec<f64> = d.blocks.iter().map(|b| b.t_ns as f64 / 1e9).collect();
    bnds.sort_by(|a, b| a.partial_cmp(b).unwrap());
    println!("blocks: {} first 10: {:?}", bnds.len(), &bnds[..bnds.len().min(10)].iter().map(|v| format!("{v:.2}")).collect::<Vec<_>>());
    for num in 1..8usize {
        let target = playable * num / 8;
        let t_s = target as f64 / 48000.0;
        let next_b = bnds.iter().filter(|&&b| b > t_s + 0.001).next().copied();
        let mut dec = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        dec.seek(target as u64).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 4800 * 2];
        while got.len() < 24000 * 40 * 2 / 10 * 10 {
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..(n as usize) * 2]);
            if got.len() >= 48000 * 20 * 2 { break; }
        }
        // scan 0.5s windows for first sustained corr drop
        let mut onset = f64::NAN;
        let mut w = 0usize;
        loop {
            let a = w * 24000;
            let n = 24000usize;
            if a + n > got.len() / 2 || target + a + n > whole_pcm.len() / 2 { break; }
            let s: Vec<f64> = (0..n).map(|i| (got[(a + i) * 2] as f64 + got[(a + i) * 2 + 1] as f64) * 0.5).collect();
            let r: Vec<f64> = (0..n).map(|i| { let j = target + a + i; (whole_pcm[j * 2] as f64 + whole_pcm[j * 2 + 1] as f64) * 0.5 }).collect();
            let c = corr(&s, &r);
            if c < 0.95 {
                // confirm sustained: next window also < 0.95?
                let a2 = a + n;
                if a2 + n <= got.len() / 2 && target + a2 + n <= whole_pcm.len() / 2 {
                    let s2: Vec<f64> = (0..n).map(|i| (got[(a2 + i) * 2] as f64 + got[(a2 + i) * 2 + 1] as f64) * 0.5).collect();
                    let r2: Vec<f64> = (0..n).map(|i| { let j = target + a2 + i; (whole_pcm[j * 2] as f64 + whole_pcm[j * 2 + 1] as f64) * 0.5 }).collect();
                    if corr(&s2, &r2) < 0.95 { onset = a as f64 / 48000.0; break; }
                }
            }
            w += 1;
        }
        println!("anchor {num}/8 t={t_s:.2}s | next block at {:?} (+{:.2}s) | divergence onset: {:.2}s after anchor",
            next_b, next_b.map(|b| b - t_s).unwrap_or(f64::NAN), onset);
    }
}
