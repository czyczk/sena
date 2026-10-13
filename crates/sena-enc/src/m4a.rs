//! Minimal MP4/m4a demuxer: just enough box walking to pull the ASC (esds
//! DecSpecificInfo), the raw AUs (mdat, stsz order) and the mdhd timescale
//! out of an exhale / ezcd-cli output.

use std::path::Path;

use crate::Error;

/// MPEG-4 descriptor tags walked by [`walk_descs`].
mod desc {
    pub const ES: u8 = 0x03;
    pub const DECODER_CONFIG: u8 = 0x04;
    pub const DEC_SPECIFIC_INFO: u8 = 0x05;
}

/// Big-endian u32 at `off`.
fn be32(f: &[u8], off: usize) -> u32 {
    u32::from_be_bytes([f[off], f[off + 1], f[off + 2], f[off + 3]])
}

/// Read the stream's actual sample rate from mdhd timescale.
pub fn m4a_stream_rate(path: &Path) -> Result<u32, Error> {
    let f = std::fs::read(path)?;
    for (tag, s, e) in boxes(&f, 0, f.len()) {
        if tag != b"moov" {
            continue;
        }
        for (tag2, s2, e2) in boxes(&f, s, e) {
            if tag2 != b"trak" {
                continue;
            }
            for (tag3, s3, e3) in boxes(&f, s2, e2) {
                if tag3 == b"mdia" {
                    for (tag4, s4, _e4) in boxes(&f, s3, e3) {
                        if tag4 == b"mdhd" {
                            let ver = f[s4];
                            // version(1) flags(3), then v1: 8+8 creation/mod
                            // times, v0: 4+4; timescale follows.
                            let off = if ver == 1 { 16 } else { 12 };
                            return Ok(be32(&f, s4 + off));
                        }
                    }
                }
            }
        }
    }
    Err(Error::Format("mdhd not found".into()))
}

/// Extract ASC (esds desc 0x05) and raw AUs (mdat, stsz order) from an exhale m4a.
pub fn extract_m4a(path: &Path) -> Result<(Vec<u8>, Vec<Vec<u8>>), Error> {
    let f = std::fs::read(path)?;
    let asc = find_asc(&f)?;
    let aus = find_aus(&f)?;
    Ok((asc, aus))
}

fn boxes(f: &[u8], start: usize, end: usize) -> Vec<(&[u8], usize, usize)> {
    let mut out = vec![];
    let mut p = start;
    while p + 8 <= end {
        let sz = be32(f, p) as usize;
        let tag = &f[p + 4..p + 8];
        if sz < 8 || p + sz > end {
            break;
        }
        out.push((tag, p + 8, p + sz));
        p += sz;
    }
    out
}

fn find_asc(f: &[u8]) -> Result<Vec<u8>, Error> {
    // walk: moov > trak > mdia > minf > stbl > stsd > mp4a > esds > desc 0x05
    for (tag, s, e) in boxes(f, 0, f.len()) {
        if tag != b"moov" {
            continue;
        }
        for (tag2, s2, e2) in boxes(f, s, e) {
            if tag2 != b"trak" {
                continue;
            }
            for (tag3, s3, e3) in boxes(f, s2, e2) {
                if tag3 != b"mdia" {
                    continue;
                }
                for (tag4, s4, e4) in boxes(f, s3, e3) {
                    if tag4 != b"minf" {
                        continue;
                    }
                    for (tag5, s5, e5) in boxes(f, s4, e4) {
                        if tag5 != b"stbl" {
                            continue;
                        }
                        for (tag6, s6, e6) in boxes(f, s5, e5) {
                            if tag6 != b"stsd" {
                                continue;
                            }
                            // entry at s6+4 (version/flags) + 4 (count)
                            let (_, es, ee) = boxes(f, s6 + 8, e6)[0];
                            // mp4a entry: 8 + 28 audio header
                            for (tag7, s7, e7) in boxes(f, es + 28, ee) {
                                if tag7 == b"esds" {
                                    if let Some(asc) = walk_descs(f, s7 + 4, e7) {
                                        return Ok(asc);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    Err(Error::Format("ASC not found in m4a".into()))
}

/// Recursive MPEG-4 descriptor walker: find the 0x05 DecSpecificInfo payload.
fn walk_descs(f: &[u8], start: usize, end: usize) -> Option<Vec<u8>> {
    let mut p = start;
    while p + 2 <= end {
        let t = f[p];
        p += 1;
        let mut len = 0u32;
        for _ in 0..4 {
            if p >= end {
                return None;
            }
            let b = f[p];
            p += 1;
            len = (len << 7) | (b & 0x7F) as u32;
            if b & 0x80 == 0 {
                break;
            }
        }
        let c_end = p + len as usize;
        if c_end > end {
            return None;
        }
        match t {
            desc::DEC_SPECIFIC_INFO => return Some(f[p..c_end].to_vec()),
            desc::ES => {
                // ES_ID(2) + flags(1), then children
                if let Some(v) = walk_descs(f, p + 3, c_end) {
                    return Some(v);
                }
            }
            desc::DECODER_CONFIG => {
                // objectType(1) streamType(1) bufferSize(3) maxRate(4) avgRate(4)
                if let Some(v) = walk_descs(f, p + 13, c_end) {
                    return Some(v);
                }
            }
            _ => {}
        }
        p = c_end;
    }
    None
}

fn find_aus(f: &[u8]) -> Result<Vec<Vec<u8>>, Error> {
    let mut sizes: Option<Vec<u32>> = None;
    let mut mdat_start = None;
    let mut first_chunk_offset = None;
    for (tag, s, e) in boxes(f, 0, f.len()) {
        if tag == b"mdat" {
            mdat_start = Some(s);
        }
        if tag == b"moov" {
            for (t2, s2, e2) in boxes(f, s, e) {
                if t2 != b"trak" {
                    continue;
                }
                for (t3, s3, e3) in boxes(f, s2, e2) {
                    if t3 != b"mdia" {
                        continue;
                    }
                    for (t4, s4, e4) in boxes(f, s3, e3) {
                        if t4 != b"minf" {
                            continue;
                        }
                        for (t5, s5, e5) in boxes(f, s4, e4) {
                            if t5 != b"stbl" {
                                continue;
                            }
                            for (t6, s6, _e6) in boxes(f, s5, e5) {
                                if t6 == b"stsz" {
                                    let count = be32(f, s6 + 8) as usize;
                                    let mut v = Vec::with_capacity(count);
                                    for i in 0..count {
                                        v.push(be32(f, s6 + 12 + 4 * i));
                                    }
                                    sizes = Some(v);
                                }
                                if t6 == b"stco" {
                                    // stco payload: version/flags(4) + count(4)
                                    // + chunk offsets; the first offset is the
                                    // authoritative start of AU #0. exhale's
                                    // mdat preamble length can vary, so the
                                    // old hard-coded `mdat + 2` is unreliable.
                                    let count = be32(f, s6 + 4) as usize;
                                    if count > 0 {
                                        first_chunk_offset = Some(be32(f, s6 + 8) as usize);
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }
    let sizes = sizes.ok_or_else(|| Error::Format("stsz not found".into()))?;
    let start = mdat_start.ok_or_else(|| Error::Format("mdat not found".into()))?;
    // exhale writes "unused but informative" bytes at the start of the mdat
    // payload before the first AU. Their length can vary, so trust the MP4
    // stco chunk table; fall back to the historical two-byte preamble only
    // when stco is absent.
    let mut off = first_chunk_offset.unwrap_or(start + 2);
    let mut aus = vec![];
    for sz in sizes {
        let sz = sz as usize;
        if off + sz > f.len() {
            return Err(Error::Format("AU out of range".into()));
        }
        aus.push(f[off..off + sz].to_vec());
        off += sz;
    }
    Ok(aus)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a minimal m4a: moov(trak(mdia(mdhd, minf(stbl(stsd(mp4a(esds)),
    /// stsz, stco))))), mdat. Returns (bytes, asc, aus).
    fn synth_m4a(asc: &[u8], aus: &[&[u8]], rate: u32, preamble: &[u8]) -> Vec<u8> {
        fn bx(tag: &[u8; 4], payload: &[u8]) -> Vec<u8> {
            let mut out = Vec::new();
            out.extend_from_slice(&((8 + payload.len()) as u32).to_be_bytes());
            out.extend_from_slice(tag);
            out.extend_from_slice(payload);
            out
        }
        // MPEG-4 descriptors: ES (0x03) -> DecoderConfig (0x04) -> DecSpecific (0x05).
        let mut desc = vec![desc::DEC_SPECIFIC_INFO, asc.len() as u8];
        desc.extend_from_slice(asc);
        let mut dcfg = vec![desc::DECODER_CONFIG, (13 + desc.len()) as u8];
        dcfg.extend_from_slice(&[0x40, 0x15, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]); // 13 fixed
        dcfg.extend_from_slice(&desc);
        let mut es = vec![desc::ES, 3 + dcfg.len() as u8];
        es.extend_from_slice(&[0, 1, 0]); // ES_ID + flags
        es.extend_from_slice(&dcfg);
        let mut esds_payload = vec![0u8; 4]; // version/flags
        esds_payload.extend_from_slice(&es);
        let esds = bx(b"esds", &esds_payload);

        let mut mp4a_entry = Vec::new();
        mp4a_entry.extend_from_slice(&[0u8; 6]); // reserved
        mp4a_entry.extend_from_slice(&1u16.to_be_bytes()); // data reference index
        mp4a_entry.extend_from_slice(&[0u8; 8]); // reserved
        mp4a_entry.extend_from_slice(&2u16.to_be_bytes()); // channels
        mp4a_entry.extend_from_slice(&16u16.to_be_bytes()); // sample size
        mp4a_entry.extend_from_slice(&[0u8; 4]); // pre-defined + reserved
        mp4a_entry.extend_from_slice(&(rate << 16).to_be_bytes()); // sample rate 16.16
        let mut mp4a_full_payload = mp4a_entry.clone();
        mp4a_full_payload.extend_from_slice(&esds);
        let mp4a = bx(b"mp4a", &mp4a_full_payload);

        let mut stsd_payload = vec![0u8; 4]; // version/flags
        stsd_payload.extend_from_slice(&1u32.to_be_bytes()); // entry count
        stsd_payload.extend_from_slice(&mp4a);
        let stsd = bx(b"stsd", &stsd_payload);

        let mut stsz_payload = vec![0u8; 4];
        stsz_payload.extend_from_slice(&0u32.to_be_bytes()); // sample_size (variable)
        stsz_payload.extend_from_slice(&(aus.len() as u32).to_be_bytes());
        for au in aus {
            stsz_payload.extend_from_slice(&(au.len() as u32).to_be_bytes());
        }
        let stsz = bx(b"stsz", &stsz_payload);

        // stco is patched below once the mdat offset is known; reserve a
        // placeholder chunk offset, then fix it up after assembling the file.
        let stco_offset_pos;
        let stco = {
            let mut p = vec![0u8; 4];
            p.extend_from_slice(&1u32.to_be_bytes()); // entry count
            stco_offset_pos = 8 + 4 + 4; // header + verflags + count
            p.extend_from_slice(&0u32.to_be_bytes());
            bx(b"stco", &p)
        };

        let mut mdhd_payload = vec![0u8; 4]; // version 0 + flags
        mdhd_payload.extend_from_slice(&[0u8; 8]); // creation + modification
        mdhd_payload.extend_from_slice(&rate.to_be_bytes()); // timescale
        mdhd_payload.extend_from_slice(&0u32.to_be_bytes()); // duration
        mdhd_payload.extend_from_slice(&[0u8; 4]); // language + quality
        let mdhd = bx(b"mdhd", &mdhd_payload);

        let mut stbl = Vec::new();
        stbl.extend_from_slice(&stsd);
        stbl.extend_from_slice(&stsz);
        stbl.extend_from_slice(&stco);
        let minf = bx(b"minf", &bx(b"stbl", &stbl));
        let mut mdia = Vec::new();
        mdia.extend_from_slice(&mdhd);
        mdia.extend_from_slice(&minf);
        let trak = bx(b"trak", &bx(b"mdia", &mdia));
        let moov = bx(b"moov", &trak);

        let mut mdat_payload = preamble.to_vec();
        let au0_file_offset = moov.len() + 8 + preamble.len();
        for au in aus {
            mdat_payload.extend_from_slice(au);
        }
        let mdat = bx(b"mdat", &mdat_payload);

        let mut file = moov;
        file.extend_from_slice(&mdat);
        // Locate the stco tag anywhere in the file and patch its first entry.
        let tag = b"stco";
        for pos in 0..file.len().saturating_sub(4) {
            if &file[pos..pos + 4] == tag {
                let entry_pos = pos - 4 + stco_offset_pos;
                file[entry_pos..entry_pos + 4]
                    .copy_from_slice(&(au0_file_offset as u32).to_be_bytes());
                break;
            }
        }
        file
    }

    #[test]
    fn m4a_extract_asc_and_aus() {
        let asc = vec![0x2B, 0x92, 0x08];
        let au0 = vec![1u8, 2, 3];
        let au1 = vec![4u8, 5];
        let bytes = synth_m4a(&asc, &[&au0, &au1], 16_000, &[0, 0]);
        let dir = std::env::temp_dir().join(format!("sena-m4a-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.m4a");
        std::fs::write(&path, &bytes).unwrap();
        let (got_asc, got_aus) = extract_m4a(&path).unwrap();
        assert_eq!(got_asc, asc);
        assert_eq!(got_aus, vec![au0, au1]);
        assert_eq!(m4a_stream_rate(&path).unwrap(), 16_000);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn m4a_extract_falls_back_to_mdat_preamble_without_stco() {
        // Same file, but strip the stco box: extraction must fall back to the
        // historical "two-byte preamble after the mdat header" assumption.
        let asc = vec![0x2B];
        let au = vec![9u8, 9, 9, 9];
        let bytes = synth_m4a(&asc, &[&au], 16_000, &[0, 0]);
        // Zero out the stco tag (leave the box in place but unrecognized).
        let mut bytes = bytes;
        let mut pos = 0usize;
        while pos + 8 <= bytes.len() {
            if &bytes[pos + 4..pos + 8] == b"stco" {
                bytes[pos + 4..pos + 8].copy_from_slice(b"free");
            }
            let sz = be32(&bytes, pos) as usize;
            pos += sz.max(8);
        }
        let dir = std::env::temp_dir().join(format!("sena-m4a-test2-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("t.m4a");
        std::fs::write(&path, &bytes).unwrap();
        let (got_asc, got_aus) = extract_m4a(&path).unwrap();
        assert_eq!(got_asc, asc);
        assert_eq!(got_aus, vec![au]);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn m4a_boxes_iterator_bounds() {
        // A truncated box (size larger than the buffer) must terminate the walk.
        let mut f = Vec::new();
        f.extend_from_slice(&100u32.to_be_bytes());
        f.extend_from_slice(b"moov");
        f.extend_from_slice(&[0u8; 4]); // short
        let out = boxes(&f, 0, f.len());
        assert!(out.is_empty());
        // A zero/too-small size must terminate the walk (no infinite loop).
        let mut f = Vec::new();
        f.extend_from_slice(&4u32.to_be_bytes());
        f.extend_from_slice(b"moov");
        let out = boxes(&f, 0, f.len());
        assert!(out.is_empty());
    }

    #[test]
    fn m4a_walk_descs_finds_specific_info() {
        // 0x03 (ES) wrapping 0x04 (DecoderConfig) wrapping 0x05 (DecSpecific).
        let payload = [0xAA, 0xBB];
        let mut d = vec![desc::DEC_SPECIFIC_INFO, payload.len() as u8];
        d.extend_from_slice(&payload);
        let mut dcfg = vec![desc::DECODER_CONFIG, 13 + d.len() as u8];
        dcfg.extend_from_slice(&[0u8; 13]);
        dcfg.extend_from_slice(&d);
        let mut es = vec![desc::ES, 3 + dcfg.len() as u8];
        es.extend_from_slice(&[0, 1, 0]);
        es.extend_from_slice(&dcfg);
        assert_eq!(walk_descs(&es, 0, es.len()), Some(payload.to_vec()));
        // Truncated length must not over-read.
        let short = vec![desc::DEC_SPECIFIC_INFO, 10u8, 1];
        assert_eq!(walk_descs(&short, 0, short.len()), None);
    }
}
