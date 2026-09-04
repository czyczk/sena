//! Acceptance probe for the upstream rxaac-dec fix tracked in
//! `notes/rxaac-dec.md`: malformed / bit-shifted LF AUs must make
//! `UsacDecoder::decode_au` return an error, never panic.
//!
//! The exact historical failure was the old sena-enc `mdat + 2` extraction:
//! a 3-byte exhale mdat preamble made every LF AU start one byte late. We
//! reconstruct that byte-exact shifted AU stream from a real Sena asset and
//! feed it directly to rxaac-dec-lib (without sena-dec's catch_unwind, so a
//! panic here fails the test).

use rxaac_dec_lib::asc::AudioSpecificConfig;
use rxaac_dec_lib::usac::UsacDecoder;
use sena_dec::demux::Demuxed;

#[test]
fn one_byte_shifted_lf_au_stream_never_panics() {
    let asset = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/e2e/01__p300__lfa__hf144.sena"
    );
    let bytes = std::fs::read(asset).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let lf = demux.track("A_SENALF").unwrap();
    let asc = AudioSpecificConfig::parse(&lf.codec_private).expect("asset ASC parses");

    let frames: Vec<_> = demux.frames_for(lf.number).take(16).cloned().collect();
    assert!(frames.len() >= 3);

    // Rebuild the contiguous original AU stream, then prepend the historical
    // one-byte preamble. Chunking it with the original AU sizes reproduces
    // the old `find_aus` bug exactly: every AU starts one byte late.
    let mut original = Vec::new();
    for f in &frames {
        original.extend_from_slice(&f.data);
    }
    let mut shifted_stream = vec![0x00u8];
    shifted_stream.extend_from_slice(&original);

    let mut shifted_aus = Vec::new();
    let mut off = 0usize;
    for f in &frames {
        shifted_aus.push(shifted_stream[off..off + f.data.len()].to_vec());
        off += f.data.len();
    }

    // Exercise both fresh decoders (no valid state yet) and stateful decoders
    // that have already decoded the valid AUs, matching sena-dec's use.
    for au in &shifted_aus {
        let mut fresh = UsacDecoder::new(&asc).expect("fresh decoder");
        let mut pcm = Vec::new();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            fresh.decode_au(au, &mut pcm)
        }));
        assert!(
            result.is_ok(),
            "fresh decode_au must not panic on shifted AU"
        );

        let mut stateful = UsacDecoder::new(&asc).expect("stateful decoder");
        let mut pcm = Vec::new();
        for good in &frames[..3] {
            let _ = stateful.decode_au(&good.data, &mut pcm);
            pcm.clear();
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            stateful.decode_au(au, &mut pcm)
        }));
        assert!(
            result.is_ok(),
            "stateful decode_au must not panic on shifted AU"
        );
    }
}

#[test]
fn malformed_preroll_config_is_contained_by_sena_decode() {
    // A single-byte corruption in the AudioPreroll config area makes rxaac
    // re-initialise itself with a channel layout that is inconsistent with
    // the decoded element (`num_out_channels = 1` but a 2-channel CPE). This
    // used to panic inside rxaac (`usac.rs` channel indexing); rxaac now
    // rejects the config and returns Err. This test verifies sena's normal
    // Result-based recovery (silence + decoder rebuild) still produces the
    // full playable timeline.
    let asset = concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../../assets/e2e/01__p300__lfa__hf144.sena"
    );
    let bytes = std::fs::read(asset).unwrap();
    let demux = Demuxed::parse(bytes).unwrap();
    let lf = demux.track("A_SENALF").unwrap();
    let hf = demux.track("A_OPUS").unwrap();
    let lf_frames: Vec<_> = demux.frames_for(lf.number).take(3).cloned().collect();
    let hf_frames: Vec<_> = demux.frames_for(hf.number).take(3).cloned().collect();

    let tracks = vec![
        sena_mux::mka::Track {
            codec_id: "A_OPUS".into(),
            codec_private: hf.codec_private.clone(),
            sample_rate: 48000.0,
            channels: 2,
            codec_delay_ns: hf.codec_delay_ns.unwrap_or(0),
            bit_depth: Some(32),
        },
        sena_mux::mka::Track {
            codec_id: "A_SENALF".into(),
            codec_private: lf.codec_private.clone(),
            sample_rate: lf.sample_rate,
            channels: 2,
            codec_delay_ns: lf.codec_delay_ns.unwrap_or(0),
            bit_depth: None,
        },
    ];
    let mut frames = Vec::new();
    for (i, f) in hf_frames.iter().enumerate() {
        frames.push(sena_mux::mka::Frame {
            track: 0,
            t_ns: i as u64 * 20_000_000,
            data: f.data.clone(),
        });
    }
    for (i, f) in lf_frames.iter().enumerate() {
        let mut data = f.data.clone();
        if i == 0 {
            assert!(data.len() > 3);
            data[3] = data[3].wrapping_sub(1); // 0x22 -> 0x21 triggers the preroll reinit
        }
        frames.push(sena_mux::mka::Frame {
            track: 1,
            t_ns: i as u64 * 64_000_000,
            data,
        });
    }
    let tags = [
        ("SENA_PROFILE", "300"),
        ("SENA_VERSION", "1"),
        ("SENA_PLAYABLE_SAMPLES", "2568"),
    ];
    let playable_ns = 2568u64 * 1_000_000_000 / 48_000;
    let file = sena_mux::mka::write_mka(&tracks, frames, &tags, 1000, playable_ns);
    let small = Demuxed::parse(file).unwrap();

    let decoded = sena_dec::pipeline::decode(&small).unwrap();
    assert_eq!(decoded.info().playable_frames, 2568);
    assert!(
        decoded
            .warnings()
            .iter()
            .any(|w| w.contains("LF AU decode error") || w.contains("LF AU decoder panic")),
        "expected the malformed LF AU to be reported, got {:?}",
        decoded.warnings()
    );
}
