// Post-seek transient profile: for several targets, diff the seek decode
// against the continuous decode in windows for the first 2 seconds.
use sena_dec::demux::Demuxed;
use sena_dec::stream::StreamingDecoder;

fn main() {
    let path = std::env::args().nth(1).expect("usage: seek_transient <file.sena>");
    let bytes = std::fs::read(&path).unwrap();
    let demuxed = Demuxed::parse(bytes).unwrap();
    let whole = sena_dec::pipeline::decode(&demuxed).unwrap();
    let whole_pcm: Vec<f32> = whole.pcm_f64().iter().map(|&v| v as f32).collect();
    let playable = whole.info().playable_frames as usize;
    drop(whole);

    let fracs: Vec<(String, usize)> = [("1/4", playable / 4), ("1/3", playable / 3), ("1/2", playable / 2),
        ("2/3", playable * 2 / 3), ("3/4", playable * 3 / 4)].into_iter()
        .map(|(n, t)| (n.to_string(), t)).collect();
    let windows = [0usize, 2400, 4800, 9600, 19200, 38400, 76800, 96000];
    println!("target | 0-50ms | 50-100 | 100-200 | 200-400 | 400-800 | 0.8-1.6 | 1.6-2.0");
    for (name, target) in fracs {
        let mut dec = StreamingDecoder::open(Demuxed::parse(std::fs::read(&path).unwrap()).unwrap()).unwrap();
        dec.seek(target as u64).unwrap();
        let mut got = Vec::new();
        let mut buf = vec![0.0f32; 4800 * 2];
        while got.len() < 96000 * 2 {
            let n = dec.read_f32(&mut buf, 4800).unwrap();
            if n == 0 { break; }
            got.extend_from_slice(&buf[..(n as usize) * 2]);
        }
        let mut row = format!("{name:>6} |");
        for w in 0..windows.len() - 1 {
            let (a, b) = (windows[w], windows[w + 1]);
            if b > got.len() / 2 || target + b > whole_pcm.len() / 2 {
                row.push_str("      - |");
                continue;
            }
            let mut max = 0.0f32;
            for i in a..b {
                for c in 0..2 {
                    let diff = (got[i * 2 + c] - whole_pcm[(target + i) * 2 + c]).abs();
                    max = max.max(diff);
                }
            }
            row.push_str(&format!(" {:>7.5} |", max));
        }
        println!("{row}");
    }
}
