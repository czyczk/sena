//! Matroska writer for Sena v2: one composite audio track + Cues.

use crate::ebml;
use sena_core::es;

pub const TIMECODE_SCALE: u64 = 1_000_000; // 1 ms ticks

/// One composite ES block (see sena_core::es) with its playable-timeline
/// timestamp.
pub struct SenaBlock {
    /// presentation timestamp in nanoseconds (multiple of es::BLOCK_NS)
    pub t_ns: u64,
    pub data: Vec<u8>,
}

/// Write a Sena file: exactly one `A_MS/ACM` track carrying composite ES
/// blocks, 1 s-ish clusters, and a Cues element (one CuePoint per cluster)
/// after the last cluster. `playable_ns` is the playable length; the
/// Segment Info Duration is set to exactly this value (no padding).
pub fn write_sena(
    codec_private: &[u8],
    blocks: &[SenaBlock],
    codec_name: &str,
    cluster_ms: u64,
    playable_ns: u64,
) -> Vec<u8> {
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
    // exact playable duration (ms); no padding convention
    ebml::float(&mut info, &[0x44, 0x89], playable_ns as f64 / 1e6);
    ebml::write_element(&mut out, &[0x15, 0x49, 0xA9, 0x66], &info);

    // Tracks: the single composite Sena track
    let mut te = vec![];
    ebml::uint(&mut te, &[0xD7], 1); // TrackNumber
    ebml::uint(&mut te, &[0x73, 0xC5], 1); // TrackUID
    ebml::uint(&mut te, &[0x83], 2); // audio
    ebml::str(&mut te, &[0x86], es::SENA_CODEC_ID);
    ebml::str(&mut te, &[0x25, 0x86, 0x88], codec_name);
    ebml::bin(&mut te, &[0x63, 0xA2], codec_private);
    // CodecDelay is 0 (default; trims are internal to the ES) — omitted.
    ebml::uint(&mut te, &[0x23, 0xE3, 0x83], es::BLOCK_NS); // DefaultDuration (ns)
    ebml::uint(&mut te, &[0x56, 0xBB], es::SEEK_PREROLL_NS); // SeekPreRoll (ns, informational)
    let mut audio = vec![];
    ebml::float(&mut audio, &[0xB5], sena_core::SAMPLE_RATE as f64);
    ebml::uint(&mut audio, &[0x9F], es::CHANNELS as u64);
    ebml::write_element(&mut te, &[0xE1], &audio);
    let mut tracks_el = vec![];
    ebml::write_element(&mut tracks_el, &[0xAE], &te);
    ebml::write_element(&mut out, &[0x16, 0x54, 0xAE, 0x6B], &tracks_el);

    // Clusters: pack blocks into ~cluster_ms windows, recording cue points
    let mut cues: Vec<(u64, u64)> = vec![]; // (time ms, cluster pos rel. to segment payload)
    let mut idx = 0;
    while idx < blocks.len() {
        let start = blocks[idx].t_ns;
        let end = start + cluster_ms * 1_000_000;
        let cluster_pos = (out.len() - seg_payload_start) as u64;
        cues.push((start / TIMECODE_SCALE, cluster_pos));
        let mut cluster = vec![];
        ebml::uint(&mut cluster, &[0xE7], start / TIMECODE_SCALE);
        while idx < blocks.len() && blocks[idx].t_ns < end {
            let f = &blocks[idx];
            let mut sb = vec![];
            ebml::write_vint(&mut sb, 1); // track 1
            // SimpleBlock relative timestamp is signed 16-bit in
            // TimestampScale ticks; with 1 ms ticks and <= 1 s clusters
            // this is always within i16 range for Sena block timing.
            let rel = ((f.t_ns - start) / TIMECODE_SCALE) as i16;
            sb.extend_from_slice(&rel.to_be_bytes());
            sb.push(0x80); // keyframe, no lacing
            sb.extend_from_slice(&f.data);
            ebml::write_element(&mut cluster, &[0xA3], &sb);
            idx += 1;
        }
        ebml::write_element(&mut out, &[0x1F, 0x43, 0xB6, 0x75], &cluster);
    }

    // Cues: one CuePoint per cluster
    let mut cues_el = vec![];
    for (time_ms, cluster_pos) in cues {
        let mut point = vec![];
        ebml::uint(&mut point, &[0xB3], time_ms); // CueTime (ticks)
        let mut pos = vec![];
        ebml::uint(&mut pos, &[0xF7], 1); // CueTrack
        ebml::uint(&mut pos, &[0xF1], cluster_pos); // CueClusterPosition
        ebml::uint(&mut pos, &[0x53, 0x78], 1); // CueBlockNumber
        ebml::write_element(&mut point, &[0xB7], &pos);
        ebml::write_element(&mut cues_el, &[0xBB], &point);
    }
    ebml::write_element(&mut out, &[0x1C, 0x53, 0xBB, 0x6B], &cues_el);

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn codec_name_is_embedded() {
        let cp = vec![0u8; 40];
        let blocks = vec![
            SenaBlock { t_ns: 0, data: vec![0, 3, 1, b'x'] },
            SenaBlock { t_ns: 20_000_000, data: vec![0, 2, 1, b'y'] },
        ];
        let name = "Sena xAAC-Opus@600-Opus@15600";
        let file = write_sena(&cp, &blocks, name, 1000, 40_000_000);
        let needle = b"Sena xAAC-Opus@600-Opus@15600";
        assert!(
            file.windows(needle.len()).any(|w| w == needle),
            "CodecName not found in container"
        );
    }
}
