// Per-lane cost breakdown for the xfi streaming decode: rxaac mid AU decode,
// 44.1k->48k stream resample, and the LF rxaac decode, each timed alone.
use sena_dec::demux::Demuxed;
use std::time::Instant;

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
    println!("mid AUs: {}", aus.len());

    // 1) pure rxaac decode
    let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(
        &rxaac_dec_lib::asc::AudioSpecificConfig::parse(&asc).unwrap(),
    )
    .unwrap();
    let mut out = Vec::new();
    let t0 = Instant::now();
    let mut samples = 0usize;
    for au in &aus {
        out.clear();
        dec.decode_au(au, &mut out).unwrap();
        samples += out.len() / 2;
    }
    let d1 = t0.elapsed().as_secs_f64();
    println!("rxaac mid decode: {:.2}s ({:.1}x)", d1, samples as f64 / 44100.0 / d1);

    // 2) the 44.1k -> 48k stream resample of the whole mid stream (f64)
    let mut dec = rxaac_dec_lib::usac::UsacDecoder::new(
        &rxaac_dec_lib::asc::AudioSpecificConfig::parse(&asc).unwrap(),
    )
    .unwrap();
    let mut raw: Vec<f32> = Vec::new();
    for au in &aus {
        let mut out = Vec::new();
        dec.decode_au(au, &mut out).unwrap();
        raw.extend(out);
    }
    let raw64: Vec<f64> = raw.iter().map(|&v| v as f64).collect();
    let t0 = Instant::now();
    let mut rs = sena_dsp::StreamResampler::new_with_passband(44100, 48000, 13_000.0);
    let mut done = 0usize;
    for chunk in raw64.chunks(4096 * 2) {
        done += rs.push(chunk, 2).len();
    }
    done += rs.finish(2).len();
    let d2 = t0.elapsed().as_secs_f64();
    println!("stream resample 44.1->48k: {:.2}s ({:.1}x, {} out frames)", d2, done as f64 / 2.0 / 48000.0 / d2, done / 2);
}
