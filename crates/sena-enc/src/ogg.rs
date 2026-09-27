//! Minimal Ogg demuxer for the Opus track: extract OpusHead, pre-skip and
//! audio packets, discarding the OpusTags comment packet (tag stripping).

use super::Error;

/// Returns (OpusHead packet, pre_skip, audio packets).
pub fn extract_opus(ogg: &[u8]) -> Result<(Vec<u8>, u16, Vec<Vec<u8>>), Error> {
    let mut packets: Vec<Vec<u8>> = vec![];
    let mut pos = 0usize;
    let mut cur = Vec::new();
    while pos + 27 <= ogg.len() {
        if &ogg[pos..pos + 4] != b"OggS" {
            return Err(Error::Format("bad ogg page".into()));
        }
        let hdr_type = ogg[pos + 5];
        let nsegs = ogg[pos + 26] as usize;
        let segtab = &ogg[pos + 27..pos + 27 + nsegs];
        let mut body = pos + 27 + nsegs;
        for &l in segtab {
            let l = l as usize;
            if body + l > ogg.len() {
                return Err(Error::Format("ogg page truncated".into()));
            }
            cur.extend_from_slice(&ogg[body..body + l]);
            body += l;
            if l < 255 {
                // packet complete
                packets.push(std::mem::take(&mut cur));
            }
        }
        if hdr_type & 0x04 != 0 {
            // end of stream
            break;
        }
        pos = body;
    }
    if packets.len() < 2 {
        return Err(Error::Format("ogg has too few packets".into()));
    }
    let head = packets[0].clone();
    if head.len() < 19 || &head[0..8] != b"OpusHead" {
        return Err(Error::Format("first packet is not OpusHead".into()));
    }
    let preskip = u16::from_le_bytes([head[10], head[11]]);
    // packet 1 is OpusTags -> discard (tag stripping policy)
    let audio: Vec<Vec<u8>> = packets.into_iter().skip(2).collect();
    Ok((head, preskip, audio))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build one Ogg page carrying the given packets (no continuation).
    fn page(hdr_type: u8, packets: &[&[u8]]) -> Vec<u8> {
        let mut segtab = Vec::new();
        let mut body = Vec::new();
        for p in packets {
            // Lacing: full 255-octet runs then a final short segment.
            let mut left = p.len();
            while left >= 255 {
                segtab.push(255u8);
                left -= 255;
            }
            segtab.push(left as u8);
            body.extend_from_slice(p);
        }
        let mut out = Vec::new();
        out.extend_from_slice(b"OggS");
        out.push(0); // version
        out.push(hdr_type);
        out.extend_from_slice(&[0u8; 20]); // granule + serial + seq + crc (unused)
        out.push(segtab.len() as u8);
        out.extend_from_slice(&segtab);
        out.extend_from_slice(&body);
        out
    }

    fn opus_head(pre_skip: u16) -> Vec<u8> {
        let mut h = Vec::new();
        h.extend_from_slice(b"OpusHead");
        h.push(1); // version
        h.push(2); // channels
        h.extend_from_slice(&pre_skip.to_le_bytes());
        h.extend_from_slice(&48_000u32.to_le_bytes());
        h.extend_from_slice(&0i16.to_le_bytes());
        h.push(0); // mapping family
        h
    }

    fn ogg_file(packets: &[&[u8]]) -> Vec<u8> {
        // One page with BOS for the head, one EOS page with the rest.
        let mut out = page(0x02, &packets[..1]);
        if packets.len() > 1 {
            out.extend(page(0x04, &packets[1..]));
        }
        out
    }

    #[test]
    fn extract_basic_layout() {
        let head = opus_head(312);
        let tags = b"OpusTagsxxxx";
        let a1 = vec![0x11u8; 100];
        let a2 = vec![0x22u8; 50];
        let ogg = ogg_file(&[&head, tags, &a1, &a2]);
        let (h, pre, audio) = extract_opus(&ogg).unwrap();
        assert_eq!(h, head);
        assert_eq!(pre, 312);
        assert_eq!(audio, vec![a1, a2]);
    }

    #[test]
    fn packet_continuation_across_segments() {
        // A packet of 600 bytes laces as 255 + 255 + 90; the decoder must
        // reassemble it into one packet.
        let head = opus_head(0);
        let big = vec![0xABu8; 600];
        let ogg = ogg_file(&[&head, b"OpusTags", &big]);
        let (_, _, audio) = extract_opus(&ogg).unwrap();
        assert_eq!(audio.len(), 1);
        assert_eq!(audio[0], big);
    }

    #[test]
    fn stops_at_end_of_stream_page() {
        let head = opus_head(0);
        let mut ogg = ogg_file(&[&head, b"OpusTags", b"a"]);
        // Trailing garbage after the EOS page must be ignored.
        ogg.extend_from_slice(b"OggS trailing junk that is not a page");
        let (_, _, audio) = extract_opus(&ogg).unwrap();
        assert_eq!(audio.len(), 1);
    }

    #[test]
    fn rejects_bad_capture() {
        let head = opus_head(0);
        let mut ogg = ogg_file(&[&head, b"OpusTags", b"a"]);
        ogg[0] = b'X';
        assert!(extract_opus(&ogg).is_err());
    }

    #[test]
    fn rejects_truncated_page_body() {
        let head = opus_head(0);
        let ogg = ogg_file(&[&head, b"OpusTags", b"audio"]);
        let truncated = ogg[..ogg.len() - 3].to_vec();
        assert!(extract_opus(&truncated).is_err());
    }

    #[test]
    fn rejects_non_opus_head_first_packet() {
        let ogg = ogg_file(&[b"NotAnOpusHead........", b"OpusTags", b"a"]);
        assert!(extract_opus(&ogg).is_err());
    }

    #[test]
    fn rejects_too_few_packets() {
        // Head alone: no OpusTags and no audio.
        let ogg = ogg_file(&[&opus_head(0)]);
        assert!(extract_opus(&ogg).is_err());
    }

    #[test]
    fn tags_packet_is_dropped() {
        // Whatever the OpusTags packet carries (even vendor strings that look
        // like audio), it must never reach the audio packet list.
        let head = opus_head(7);
        let tags = vec![0xEEu8; 400];
        let audio1 = vec![0x01u8; 3];
        let ogg = ogg_file(&[&head, &tags, &audio1]);
        let (_, pre, audio) = extract_opus(&ogg).unwrap();
        assert_eq!(pre, 7);
        assert_eq!(audio, vec![audio1]);
    }
}
