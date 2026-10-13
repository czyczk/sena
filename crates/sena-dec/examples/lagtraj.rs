// lag trajectory after a seek: best lag + corr in windows over 40s.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn main() {
    let path = std::env::args().nth(1).unwrap();
    let target: u64 = std::env::args().nth(2).unwrap().parse().unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let whole = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    drop(whole);
    let mut dec = StreamingDecoder::open(Demuxed::parse(bytes).unwrap()).unwrap();
    dec.seek(target).unwrap();
    let mut got = Vec::new();
    let mut buf = vec![0.0f32; 4800 * 2];
    while got.len() < 48000 * 42 * 2 {
        let n = dec.read_f32(&mut buf, 4800).unwrap();
        if n == 0 { break; }
        got.extend_from_slice(&buf[..(n as usize) * 2]);
    }
    println!("win_start_s | best_lag | corr@best | corr@lag0");
    let mut w = 0usize;
    while (w + 1) * 48000 <= got.len() / 2 {
        let a = w * 48000;
        let n = 48000usize;
        if target as usize + a + n > whole_pcm.len() / 2 { break; }
        let s: Vec<f64> = (0..n).step_by(4).map(|i| (got[(a + i) * 2] as f64 + got[(a + i) * 2 + 1] as f64) * 0.5).collect();
        let r: Vec<f64> = (0..n).step_by(4).map(|i| { let j = target as usize + a + i; (whole_pcm[j * 2] as f64 + whole_pcm[j * 2 + 1] as f64) * 0.5 }).collect();
        let nn = s.len() as f64;
        let mut best = (0i64, f64::MIN);
        let mut corr0 = 0.0f64;
        for lag in -2400i64..=2400 {
            let (mut sab, mut sa, mut sb, mut saa, mut sbb) = (0.0, 0.0, 0.0, 0.0, 0.0);
            let mut cnt = 0usize;
            for i in 0..s.len() {
                let j = i as i64 + lag;
                if j < 0 || j >= s.len() as i64 { continue; }
                let (x, y) = (s[i], r[j as usize]);
                sab += x * y; sa += x; sb += y; saa += x * x; sbb += y * y; cnt += 1;
            }
            let c = (sab - sa * sb / cnt as f64) / ((saa - sa * sa / cnt as f64) * (sbb - sb * sb / cnt as f64)).sqrt().max(1e-12);
            if lag == 0 { corr0 = c; }
            if c > best.1 { best = (lag, c); }
        }
        let _ = nn;
        println!("{:>11} | {:>8} | {:>9.5} | {:>9.5}", w, best.0, best.1, corr0);
        w += 1;
    }
}
