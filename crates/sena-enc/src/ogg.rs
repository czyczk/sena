//! Minimal Ogg demuxer for the Opus track: extract OpusHead, pre-skip and
//! audio packets, discarding the OpusTags comment packet (tag stripping).

use super::Error;

/// Returns (OpusHead packet, pre_skip, audio packets).
pub fn extract_opus(ogg: &[u8]) -> Result<(Vec<u8>, u16, Vec<Vec<u8>>), Error> {
    let mut packets: Vec<Vec<u8>> = vec![];
    let mut pos = 0usize;
    let mut cur = Vec::new();
    let mut segs_left = 0usize;
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
            segs_left += 1;
            body += l;
            if l < 255 {
                // packet complete
                let p = std::mem::take(&mut cur);
                packets.push(p);
                segs_left = 0;
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
