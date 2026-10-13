// indep AU spacing stats for the LF and mid lanes of a .sena file
use sena_dec::demux;
fn main() {
    let path = std::env::args().nth(1).unwrap();
    let bytes = std::fs::read(&path).unwrap();
    let indexed = demux::index_container(&mut |pos, len| demux::mem_read_at(&bytes, pos, len), true).unwrap();
    let mut lf_indep: Vec<u64> = Vec::new();
    let mut mx_indep: Vec<u64> = Vec::new();
    for b in &indexed.blocks {
        if let Some(ai) = b.au_index {
            for (i, &first) in b.lf_first.iter().take(b.lf_count as usize).enumerate() {
                if first & 0x80 != 0 { lf_indep.push(ai as u64 + i as u64); }
            }
        }
        if let Some(ai) = b.mx_au_index {
            for (i, &first) in b.mx_first.iter().take(b.mx_count as usize).enumerate() {
                if first & 0x80 != 0 { mx_indep.push(ai as u64 + i as u64); }
            }
        }
    }
    for (name, v) in [("LF", &lf_indep), ("MX", &mx_indep)] {
        let gaps: Vec<u64> = v.windows(2).map(|w| w[1] - w[0]).collect();
        if gaps.is_empty() { println!("{name}: {} indeps", v.len()); continue; }
        println!("{name}: {} indeps, gap min {} max {} mean {:.1} (AU units)",
            v.len(), gaps.iter().min().unwrap(), gaps.iter().max().unwrap(),
            gaps.iter().sum::<u64>() as f64 / gaps.len() as f64);
    }
}
