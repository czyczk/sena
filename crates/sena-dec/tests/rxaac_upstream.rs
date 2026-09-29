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
    let asc = AudioSpecificConfig::parse(&demux.init.lf_asc).expect("asset ASC parses");

    let mut frames: Vec<Vec<u8>> = Vec::new();
    for b in &demux.blocks {
        let block = sena_core::es::decode_block(&b.data).unwrap();
        for au in block.lf {
            frames.push(au.to_vec());
        }
        if frames.len() >= 16 {
            break;
        }
    }
    assert!(frames.len() >= 3);

    // Rebuild the contiguous original AU stream, then prepend the historical
    // one-byte preamble. Chunking it with the original AU sizes reproduces
    // the old `find_aus` bug exactly: every AU starts one byte late.
    let mut original = Vec::new();
    for f in &frames {
        original.extend_from_slice(f);
    }
    let mut shifted_stream = vec![0x00u8];
    shifted_stream.extend_from_slice(&original);

    let mut shifted_aus = Vec::new();
    let mut off = 0usize;
    for f in &frames {
        shifted_aus.push(shifted_stream[off..off + f.len()].to_vec());
        off += f.len();
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
            let _ = stateful.decode_au(good, &mut pcm);
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

    // Four blocks cover LF AU 0 (warmup, block 0) and AU 1 (block 3, by-end
    // placement): playable [0, 1024) core = 3072 frames at 48 kHz.
    let mut blocks: Vec<Vec<u8>> = demux.blocks[..4].iter().map(|b| b.data.clone()).collect();
    {
        let parsed = sena_core::es::decode_block(&blocks[0]).unwrap();
        let mut au0 = parsed.lf[0].to_vec();
        assert!(au0.len() > 3);
        au0[3] = au0[3].wrapping_sub(1); // 0x22 -> 0x21 triggers the preroll reinit
        let aus: Vec<&[u8]> = vec![&au0];
        blocks[0] = sena_core::es::encode_block(&sena_core::es::BlockParts {
            mid: parsed.mid,
            lf: (parsed.lf_au_index.unwrap(), &aus),
            hf: &parsed.hf,
        });
    }
    let mut init = demux.init.clone();
    init.playable_samples = 3072;
    init.audio_sha256 = [0; 32];
    let cp = sena_core::es::build_codec_private(&init, demux.avg_bytes_per_sec);
    let sb: Vec<sena_mux::mka::SenaBlock> = blocks
        .into_iter()
        .enumerate()
        .map(|(i, data)| sena_mux::mka::SenaBlock {
            t_ns: i as u64 * sena_core::es::BLOCK_NS,
            data,
        })
        .collect();
    let file = sena_mux::mka::write_sena(
        &cp,
        &sb,
        &sena_core::es::codec_name(init.profile, init.three_track),
        1000,
        3072 * 1_000_000_000 / 48_000,
    );
    let small = Demuxed::parse(file).unwrap();

    let decoded = sena_dec::pipeline::decode(&small).unwrap();
    assert_eq!(decoded.info().playable_frames, 3072);
    assert!(
        decoded
            .warnings()
            .iter()
            .any(|w| w.contains("LF AU decode error") || w.contains("LF AU decoder panic")),
        "expected the malformed LF AU to be reported, got {:?}",
        decoded.warnings()
    );
}
