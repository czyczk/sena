// For each seek target, estimate the time misalignment between the seek
// decode and the continuous decode: best lag (frames) by cross-correlation
// on a 1s slice taken 2s after the target, plus corr at lag 0 and best lag.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_lag <file.sena>");
    let bytes = std::fs::read(&path).unwrap();
    let whole = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    let playable = whole.info().playable_frames as usize;
    drop(whole);

    println!("target | corr@lag0 | best_lag | corr@best");
    for num in 1..8usize {
        let target = playable * num / 8;
        let mut dec = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        dec.seek(target as u64).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 4800 * 2];
        while got.len() < 144000 * 2 {  // 3s
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..(n as usize) * 2]);
        }
        // slice: 2s..3s after target, mono mean
        let a = 96000usize;
        let n = 48000usize;
        if got.len() < (a + n) * 2 || target + a + n > whole_pcm.len() / 2 { continue; }
        let s: Vec<f64> = (0..n).map(|i| (got[(a + i) * 2] as f64 + got[(a + i) * 2 + 1] as f64) * 0.5).collect();
        let r: Vec<f64> = (0..n).map(|i| {
            let j = target + a + i;
            (whole_pcm[j * 2] as f64 + whole_pcm[j * 2 + 1] as f64) * 0.5
        }).collect();
        let mut best = (0i64, f64::MIN);
        let mut corr0 = 0.0f64;
        for lag in -4800i64..=4800 {
            let mut num_ = 0.0;
            let (mut sa, mut sb, mut saa, mut sbb) = (0.0, 0.0, 0.0, 0.0);
            for i in 0..n {
                let j = i as i64 + lag;
                if j < 0 || j >= n as i64 { continue; }
                let (x, y) = (s[i], r[j as usize]);
                num_ += x * y; sa += x; sb += y; saa += x * x; sbb += y * y;
            }
            let denom = ((saa - sa * sa / n as f64) * (sbb - sb * sb / n as f64)).sqrt();
            let c = if denom > 1e-12 { num_ / denom } else { 0.0 };
            if lag == 0 { corr0 = c; }
            if c > best.1 { best = (lag, c); }
        }
        println!("  {num}/8 | {corr0:>9.5} | {:>8} | {:>9.5}", best.0, best.1);
    }
}
