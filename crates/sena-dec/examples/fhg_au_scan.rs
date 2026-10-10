//! Probe an encoder-produced xHE-AAC m4a (e.g. ezcd-cli / Fraunhofer IIS):
//! extract ASC+AUs, decode AU by AU with rxaac, report per-AU output widths,
//! the independency-flag map and leading-padding total, and dump the full
//! decoded PCM as f32 WAV for alignment measurement against the encoder
//! input WAV.
//!
//! Usage: fhg_au_scan <in.m4a> [out.wav]
use rxaac_dec_lib::usac::UsacDecoder;

fn main() {
    let mut args = std::env::args().skip(1);
    let m4a = args.next().expect("in.m4a");
    let wav_out = args.next();
    let (asc_bytes, aus) = sena_enc::extract_m4a(std::path::Path::new(&m4a)).unwrap();
    let rate = sena_enc::m4a_stream_rate(std::path::Path::new(&m4a)).unwrap();
    let asc = rxaac_dec_lib::asc::AudioSpecificConfig::parse(&asc_bytes).unwrap();
    let mut dec = UsacDecoder::new(&asc).unwrap();
    println!(
        "m4a rate {rate}, ASC parsed: out_rate {} core_rate {} core_frame {} ch {}",
        dec.output_rate(),
        dec.core_output_rate(),
        dec.core_frame_len(),
        dec.num_channels()
    );
    let mut pcm: Vec<f32> = Vec::new();
    let mut widths: Vec<usize> = Vec::new();
    let mut indeps: Vec<usize> = Vec::new();
    for (i, au) in aus.iter().enumerate() {
        let mut out = Vec::new();
        dec.decode_au(au, &mut out).unwrap();
        let n = out.len() / dec.num_channels();
        widths.push(n);
        if au[0] >> 7 != 0 {
            indeps.push(i);
        }
        pcm.extend(out);
    }
    let w0 = widths[0];
    let steady: Vec<usize> = widths.iter().skip(1).copied().collect::<std::collections::BTreeSet<_>>().into_iter().collect();
    println!(
        "{} AUs; AU0 width {w0}, steady widths {steady:?}; leading_padding {} samples",
        aus.len(),
        dec.leading_padding()
    );
    let first_indeps: Vec<usize> = indeps.iter().take(12).copied().collect();
    println!(
        "indep AUs: {} total, first: {:?}",
        indeps.len(),
        first_indeps
    );
    if let Some(path) = wav_out {
        // minimal f32 WAV writer (stereo at the decoder output rate)
        let rate = dec.output_rate();
        let ch = dec.num_channels() as u16;
        let data_len = (pcm.len() * 4) as u32;
        let mut w = Vec::new();
        w.extend_from_slice(b"RIFF");
        w.extend_from_slice(&(36 + data_len).to_le_bytes());
        w.extend_from_slice(b"WAVEfmt ");
        w.extend_from_slice(&16u32.to_le_bytes());
        w.extend_from_slice(&3u16.to_le_bytes()); // IEEE float
        w.extend_from_slice(&ch.to_le_bytes());
        w.extend_from_slice(&rate.to_le_bytes());
        w.extend_from_slice(&(rate * ch as u32 * 4).to_le_bytes());
        w.extend_from_slice(&(ch * 4).to_le_bytes());
        w.extend_from_slice(&32u16.to_le_bytes());
        w.extend_from_slice(b"data");
        w.extend_from_slice(&data_len.to_le_bytes());
        for s in &pcm {
            w.extend_from_slice(&s.to_le_bytes());
        }
        std::fs::write(&path, &w).unwrap();
        println!("wrote {} ({} samples)", path, pcm.len() / ch as usize);
    }
}
