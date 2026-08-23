//! Matroska writer for Sena: two audio tracks, interleaved clusters.

use crate::ebml;

pub const TIMECODE_SCALE: u64 = 1_000_000; // microseconds

pub struct Track {
    pub codec_id: String,
    pub codec_private: Vec<u8>,
    pub sample_rate: f64,
    pub channels: u64,
    pub codec_delay_ns: u64,
    pub bit_depth: Option<u64>,
}

pub struct Frame {
    pub track: usize,
    /// presentation timestamp in nanoseconds
    pub t_ns: u64,
    pub data: Vec<u8>,
}

/// Write a Sena mka file. Frames must be sorted by time.
pub fn write_mka(tracks: &[Track], mut frames: Vec<Frame>, tags: &[(&str, &str)], cluster_ms: u64) -> Vec<u8> {
    let mut out = vec![];

    // EBML header
    let mut hdr = vec![];
    ebml::str(&mut hdr, &[0x42, 0x82], "matroska");
    ebml::uint(&mut hdr, &[0x42, 0x87], 4);
    ebml::uint(&mut hdr, &[0x42, 0x85], 2);
    ebml::write_element(&mut out, &[0x1A, 0x45, 0xDF, 0xA3], &hdr);

    // Segment (size patched at the end)
    let seg_pos = out.len();
    ebml::write_element_sized(&mut out, &[0x18, 0x53, 0x80, 0x67], 8, &[]);
    let seg_payload_start = out.len();

    // Info
    let mut info = vec![];
    ebml::uint(&mut info, &[0x2A, 0xD7, 0xB1], TIMECODE_SCALE);
    ebml::str(&mut info, &[0x4D, 0x80], "senaenc");
    ebml::str(&mut info, &[0x57, 0x41], "senaenc");
    if let Some(last) = frames.last() {
        let dur = (last.t_ns as f64 / 1e3) + 20_000.0; // ms, padded
        ebml::float(&mut info, &[0x44, 0x89], dur);
    }
    ebml::write_element(&mut out, &[0x15, 0x49, 0xA9, 0x66], &info);

    // Tracks
    let mut tracks_el = vec![];
    for (i, t) in tracks.iter().enumerate() {
        let mut te = vec![];
        ebml::uint(&mut te, &[0xD7], (i + 1) as u64);
        ebml::uint(&mut te, &[0x73, 0xC5], (i + 1) as u64);
        ebml::uint(&mut te, &[0x83], 2); // audio
        ebml::str(&mut te, &[0x86], &t.codec_id);
        if !t.codec_private.is_empty() {
            ebml::bin(&mut te, &[0x63, 0xA2], &t.codec_private);
        }
        ebml::uint(&mut te, &[0x56, 0xAA], t.codec_delay_ns);
        let mut audio = vec![];
        ebml::float(&mut audio, &[0xB5], t.sample_rate);
        ebml::uint(&mut audio, &[0x9F], t.channels);
        if let Some(bd) = t.bit_depth {
            ebml::uint(&mut audio, &[0x62, 0x64], bd);
        }
        ebml::write_element(&mut te, &[0xE1], &audio);
        ebml::write_element(&mut tracks_el, &[0xAE], &te);
    }
    ebml::write_element(&mut out, &[0x16, 0x54, 0xAE, 0x6B], &tracks_el);

    // Tags (container level)
    if !tags.is_empty() {
        let mut tags_el = vec![];
        let mut tag = vec![];
        for (k, v) in tags {
            let mut st = vec![];
            ebml::str(&mut st, &[0x45, 0xA3], k);
            ebml::str(&mut st, &[0x44, 0x87], v);
            ebml::write_element(&mut tag, &[0x67, 0xC8], &st);
        }
        ebml::write_element(&mut tags_el, &[0x73, 0x73], &tag);
        ebml::write_element(&mut out, &[0x12, 0x54, 0xC3, 0x67], &tags_el);
    }

    // Clusters: pack frames into ~cluster_ms windows
    frames.sort_by_key(|f| f.t_ns);
    let mut idx = 0;
    while idx < frames.len() {
        let start = frames[idx].t_ns;
        let end = start + cluster_ms * 1_000_000;
        let mut cluster = vec![];
        ebml::uint(&mut cluster, &[0xE7], start / TIMECODE_SCALE);
        while idx < frames.len() && frames[idx].t_ns < end {
            let f = &frames[idx];
            let mut sb = vec![];
            ebml::write_vint(&mut sb, (f.track + 1) as u64);
            let rel = ((f.t_ns - start) * TIMECODE_SCALE as u64 / 1_000_000_000) as u16;
            sb.extend_from_slice(&rel.to_be_bytes());
            sb.push(0x80); // keyframe, no lacing
            sb.extend_from_slice(&f.data);
            ebml::write_element(&mut cluster, &[0xA3], &sb);
            idx += 1;
        }
        ebml::write_element(&mut out, &[0x1F, 0x43, 0xB6, 0x75], &cluster);
    }

    // Patch segment size
    let seg_payload_len = out.len() - seg_payload_start;
    let size_len = 8;
    let first_marker = 1 << (8 - size_len);
    let mut b = [0u8; 8];
    let mut t = seg_payload_len as u64;
    for i in (0..size_len).rev() {
        b[i] = (t & 0xFF) as u8;
        t >>= 8;
    }
    b[0] |= first_marker;
    out.splice(seg_pos + 4..seg_pos + 4 + size_len, b.iter().copied());

    out
}
