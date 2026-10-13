// Long-horizon: corr(seek output, continuous output) in windows at
// increasing offsets after the seek target.
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
    let path = std::env::args().nth(1).expect("usage: seek_converge <file.sena> [frac_num/8 ...]");
    let bytes = std::fs::read(&path).unwrap();
    let whole = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    let playable = whole.info().playable_frames as usize;
    drop(whole);
    let nums: Vec<usize> = std::env::args().skip(2).map(|a| a.parse().unwrap()).collect();
    let nums = if nums.is_empty() { vec![1, 2, 3, 4, 5, 6, 7] } else { nums };
    // windows: [start_s, len_s]
    let wins = [(0.0, 0.5), (0.5, 0.5), (1.0, 1.0), (2.0, 1.0), (4.0, 2.0), (8.0, 4.0), (16.0, 8.0), (32.0, 8.0)];
    println!("tgt | 0-0.5s | 0.5-1s | 1-2s | 2-3s | 4-6s | 8-12s | 16-24s | 32-40s");
    for num in nums {
        let target = playable * num / 8;
        let mut dec = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        dec.seek(target as u64).unwrap();
        let need = 40 * 48000usize;
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 4800 * 2];
        while got.len() < need * 2 {
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..(n as usize) * 2]);
        }
        let mut row = format!(" {num}/8 |");
        for (st, ln) in wins {
            let a = (st * 48000.0) as usize;
            let n = (ln * 48000.0) as usize;
            if a + n > got.len() / 2 || target + a + n > whole_pcm.len() / 2 { row.push_str("    - |"); continue; }
            let s: Vec<f64> = (0..n).map(|i| (got[(a + i) * 2] as f64 + got[(a + i) * 2 + 1] as f64) * 0.5).collect();
            let r: Vec<f64> = (0..n).map(|i| { let j = target + a + i; (whole_pcm[j * 2] as f64 + whole_pcm[j * 2 + 1] as f64) * 0.5 }).collect();
            row.push_str(&format!(" {:>7.5} |", corr(&s, &r)));
        }
        println!("{row}");
    }
}
