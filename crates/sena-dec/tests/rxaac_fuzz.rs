//! Deterministic malformed-AU fuzz harness for the rxaac-dec-lib integration.
//!
//! This is the Sena-side half of the methodology recorded in
//! `notes/rxaac-dec.md`: take the first LF AUs of a real `.sena` asset and
//! apply deterministic byte/bit/truncation/shift/random mutations. Every
//! variant is decoded with a fresh decoder and (for a quarter of the random
//! variants) a stateful decoder that has already seen valid AUs. rxaac must
//! return `Result` and never panic.
//!
//! `#[ignore]` because the full corpus takes tens of seconds in debug; run it
//! explicitly:
//!   cargo test -p sena-dec --test rxaac_fuzz -- --ignored --nocapture

use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_dec::demux::Demuxed;

fn try_decode(
    asc: &AudioSpecificConfig,
    good: &[Vec<u8>],
    au: &[u8],
    stateful: bool,
) -> Result<(), String> {
    let mut dec = UsacDecoder::new(asc).map_err(|e| e.to_string())?;
    let mut pcm = Vec::new();
    if stateful {
        for g in good.iter().take(3) {
            dec.decode_au(g, &mut pcm).map_err(|e| e.to_string())?;
        }
        pcm.clear();
    }
    dec.decode_au(au, &mut pcm).map_err(|e| e.to_string())?;
    Ok(())
}

#[test]
#[ignore]
fn malformed_lf_aus_never_panic() {
    let asset = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/e2e/01__p300__lfa__hf144.sena"
    );
    let bytes = std::fs::read(asset).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let lf = demux.track("A_SENALF").unwrap();
    let asc = AudioSpecificConfig::parse(&lf.codec_private).expect("asset ASC parses");
    let frames: Vec<_> = demux.frames_for(lf.number).take(8).cloned().collect();
    let good: Vec<Vec<u8>> = frames.iter().map(|f| f.data.clone()).collect();

    let mut calls = 0usize;
    let mut panics: Vec<String> = Vec::new();

    let mut run = |idx: usize, name: &str, au: &[u8], stateful: bool| {
        calls += 1;
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            try_decode(&asc, &good, au, stateful)
        }));
        if let Err(_) = result {
            panics.push(format!("au={idx} variant={name} stateful={stateful}"));
        }
    };

    // Byte substitutions across every AU byte.
    for (idx, f) in frames.iter().enumerate() {
        for pos in 0..f.data.len() {
            for val in [0x00u8, 0x01, 0x02, 0x21, 0x7f, 0x80, 0xfe, 0xff] {
                let mut v = f.data.clone();
                v[pos] = val;
                run(idx, &format!("set[{pos}]={val:#04x}"), &v, false);
            }
        }
    }

    // Single-bit flips across the AU head (where parser headers live).
    for (idx, f) in frames.iter().enumerate() {
        for pos in 0..f.data.len().min(48) {
            for bit in 0..8u8 {
                let mut v = f.data.clone();
                v[pos] ^= 1u8 << bit;
                run(idx, &format!("flip[{pos}]^{bit}"), &v, false);
            }
        }
    }

    // Truncations, leading bytes and boundary shifts.
    for (idx, f) in frames.iter().enumerate() {
        for cut in [0usize, 1, 2, 3, 4, 5, f.data.len() / 2, f.data.len() - 1] {
            run(idx, &format!("truncate@{cut}"), &f.data[..cut], false);
        }
        for lead in [0x00u8, 0xff] {
            let mut v = vec![lead];
            v.extend_from_slice(&f.data);
            run(idx, &format!("lead{lead:#04x}"), &v, false);
        }
    }
    for start_idx in 0..frames.len().min(3) {
        let mut stream = Vec::new();
        for f in frames.iter().skip(start_idx).take(6) {
            stream.extend_from_slice(&f.data);
        }
        for lead in [0x00u8, 0x01, 0xff] {
            let mut s = vec![lead];
            s.extend_from_slice(&stream);
            let mut off = 0usize;
            for (j, f) in frames.iter().skip(start_idx).take(6).enumerate() {
                let end = (off + f.data.len()).min(s.len());
                run(start_idx + j, &format!("shift{lead:#04x}"), &s[off..end], false);
                off += f.data.len();
            }
        }
    }

    // Deterministic pseudo-random mutations concentrated in the parser head.
    let mut state = 0xa5a5_1234_5a5a_6789u64;
    for i in 0..20_000usize {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        let idx = (state as usize >> 17) % frames.len();
        let au = &frames[idx].data;
        let mut v = au.to_vec();
        let nops = 1 + ((state >> 32) % 8) as usize;
        for _ in 0..nops {
            state = state
                .wrapping_mul(6364136223846793005)
                .wrapping_add(1442695040888963407);
            let pos = (state as usize >> 9) % au.len().min(80).max(1);
            match (state >> 40) % 4 {
                0 => v[pos] ^= ((state >> 48) as u8) | 1,
                1 => v[pos] = (state >> 48) as u8,
                2 => v[pos] = v[pos].wrapping_add(1),
                _ => v[pos] = v[pos].wrapping_sub(1),
            }
        }
        run(idx, &format!("random{i}"), &v, i % 4 == 0);
    }

    assert!(
        panics.is_empty(),
        "rxaac decode_au panicked on {} / {calls} malformed LF AUs:\n{}",
        panics.len(),
        panics.join("\n")
    );
    println!("malformed LF AU fuzz clean: {calls} decode_au calls, 0 panics");
}
