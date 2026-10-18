// A/B the mx phase-correction direction on seek output: diff each variant's
// post-seek decode against the batch reference in the 600-12k mid band.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let whole = sena_dec::pipeline::decode(&Demuxed::parse(bytes.clone()).unwrap()).unwrap();
    let wr: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    let playable = whole.info().playable_frames as usize;
    // 只打 pc 修正的位置（SENA_DEBUG_PC 打印过的）：扫 1..24
    for i in 1..24u64 {
        let target = (playable as u64 * i / 24) as usize;
        let mut d = StreamingDecoder::open(Demuxed::parse(bytes.clone()).unwrap()).unwrap();
        d.seek(target as u64).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 8192];
        while got.len() < 96000 * 2 {
            let n = d.read_f32(&mut buf, 4096).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..n as usize * 2]);
        }
        // 跳过 1s settle，看随后 1s 的最大差
        let mut max = 0.0f32;
        for k in 48000..got.len() / 2 {
            for c in 0..2 {
                let d0 = (got[k * 2 + c] - wr[(target + k) * 2 + c]).abs();
                max = max.max(d0);
            }
        }
        println!("{i}/24 target={target}: post-1s max {max:.5}");
    }
}
