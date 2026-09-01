//! Matroska Attachments: parse and in-place rewrite of embedded files
//! (album art lives here in Matroska, NOT in string Tags).

use sena_mux::ebml;

use crate::demux::{Demuxed, Elem};

const ATTACHMENTS_ID: &[u8] = &[0x19, 0x41, 0xA4, 0x69];
const ATTACHED_FILE_ID: &[u8] = &[0x61, 0xA7];
const FILE_NAME_ID: &[u8] = &[0x45, 0xE0];
const FILE_MIME_ID: &[u8] = &[0x46, 0x60];
const FILE_DATA_ID: &[u8] = &[0x46, 0x5C];
const FILE_UID_ID: &[u8] = &[0x46, 0xAE];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Attachment {
    pub name: String,
    pub mime: String,
    pub data: Vec<u8>,
}

impl Attachment {
    pub fn new(name: &str, mime: &str, data: Vec<u8>) -> Self {
        Self { name: name.to_string(), mime: mime.to_string(), data }
    }
}

/// Parse the top-level Attachments element(s) of a Demuxed file.
pub fn parse_attachments(demux: &Demuxed) -> Vec<Attachment> {
    let bytes = demux.bytes();
    let mut out = Vec::new();
    // Attachments is a top-level Segment child.
    let seg = demux.segment.elem.clone();
    let _result = for_each_child(bytes, seg, |id, elem| {
        if id != ATTACHMENTS_ID {
            return Ok(());
        }
        for_each_child(bytes, elem, |cid, cchild| {
            if cid != ATTACHED_FILE_ID {
                return Ok(());
            }
            let mut name = None;
            let mut mime = None;
            let mut data = None;
            for_each_child(bytes, cchild, |fid, ff| {
                match fid {
                    FILE_NAME_ID => name = Some(String::from_utf8_lossy(bytes.get(ff.payload()).unwrap_or(&[])).into_owned()),
                    FILE_MIME_ID => mime = Some(String::from_utf8_lossy(bytes.get(ff.payload()).unwrap_or(&[])).into_owned()),
                    FILE_DATA_ID => data = Some(bytes.get(ff.payload()).unwrap_or(&[]).to_vec()),
                    _ => {}
                }
                Ok(())
            })?;
            if let (Some(name), Some(mime), Some(data)) = (name, mime, data) {
                out.push(Attachment { name, mime, data });
            }
            Ok(())
        })?;
        Ok(())
    });
    // errors in the walk stop scanning (tolerated)
    let _ = _result;
    out
}

/// Encode one AttachedFile element.
fn encode_attached_file(a: &Attachment) -> Vec<u8> {
    let mut body = Vec::new();
    ebml::str(&mut body, FILE_NAME_ID, &a.name);
    ebml::str(&mut body, FILE_MIME_ID, &a.mime);
    ebml::write_element(&mut body, FILE_DATA_ID, &a.data);
    // FileUID: unique per file in the segment; the muxer has no counter, but
    // a deterministic hash of name+data is fine for our own files.
    let mut h: u64 = 0x9E37_79B9_7F4A_7C15;
    for b in a.name.bytes().chain(a.data.iter().copied()) {
        h = (h ^ u64::from(b)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    }
    ebml::uint(&mut body, FILE_UID_ID, h & 0x7FFF_FFFF_FFFF_FFFF);
    let mut file = Vec::new();
    ebml::write_element(&mut file, ATTACHED_FILE_ID, &body);
    file
}

/// Build a complete Attachments element for a list of files.
pub fn encode_attachments(attachments: &[Attachment]) -> Vec<u8> {
    let mut payload = Vec::new();
    for a in attachments {
        payload.extend(encode_attached_file(a));
    }
    let mut out = Vec::new();
    out.extend_from_slice(ATTACHMENTS_ID);
    ebml::write_vint(&mut out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out
}

/// In-place rewrite: void existing top-level Attachments element(s) (Void of
/// the same encoded length), append the replacement at the Segment tail and
/// patch the Segment size. Clusters/Cues/Tags are never moved.
pub fn rewrite_attachments(original: &[u8], attachments: &[Attachment]) -> Result<Vec<u8>, String> {
    let demux = Demuxed::parse(original.to_vec()).map_err(|e| e.to_string())?;
    let seg = demux.segment.clone();
    let mut out = original.to_vec();

    // Void every existing top-level Attachments element in place.
    let mut pos = seg.payload_start;
    while pos < seg.payload_end {
        let elem = crate::demux::read_elem_public(out.as_slice(), pos).map_err(|e| e.to_string())?;
        let id = out[elem.start..elem.start + elem.id_len].to_vec();
        if id == ATTACHMENTS_ID {
            let void = make_void(elem.data_end - elem.start)?;
            out.splice(elem.start..elem.data_end, void);
        }
        pos = elem.data_end;
    }

    let appended = if attachments.is_empty() { Vec::new() } else { encode_attachments(attachments) };
    let insert_at = seg.payload_end;
    out.splice(insert_at..insert_at, appended.iter().copied());

    let size_pos = seg.elem.start + seg.elem.id_len;
    let new_size = encode_vint_exact((out.len() - seg.payload_start) as u64, seg.elem.size_len)?;
    out.splice(size_pos..size_pos + seg.elem.size_len, new_size.iter().copied());

    // Verify the result still parses and the immutable Sena tags (including
    // the pure-audio content hash) survive.
    let check = Demuxed::parse(out.clone()).map_err(|e| format!("rewritten file invalid: {e}"))?;
    for key in ["SENA_PROFILE", "SENA_VERSION", "SENA_PLAYABLE_SAMPLES", "SENA_AUDIO_SHA256"] {
        let before = Demuxed::parse(original.to_vec())
            .map_err(|e| e.to_string())?
            .immutable_tag(key)
            .map(|s| s.to_string());
        if let Some(v) = before {
            if check.immutable_tag(key) != Some(v.as_str()) {
                return Err(format!("immutable tag {key} changed after attachment rewrite"));
            }
        }
    }
    Ok(out)
}

/// Walk top-level children of an element with a callback; tolerates unknown
/// IDs and malformed trailing data by stopping.
fn for_each_child(
    bytes: &[u8],
    elem: Elem,
    mut f: impl FnMut(&[u8], Elem) -> Result<(), crate::demux::DemuxError>,
) -> Result<(), crate::demux::DemuxError> {
    let mut p = elem.data_start;
    while p < elem.data_end {
        let child = crate::demux::read_elem_public(bytes, p)?;
        let id = bytes[child.start..child.start + child.id_len].to_vec();
        f(&id, child)?;
        p = child.data_end;
    }
    Ok(())
}

fn vint_size(v: u64) -> usize {
    ebml::vint_size(v)
}

fn encode_vint_exact(value: u64, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 || len > 8 {
        return Err("bad vint length".into());
    }
    let max = (1u64 << (7 * len)) - 1;
    if value >= max {
        return Err(format!("value {value} does not fit {len}-byte vint"));
    }
    let mut b = [0u8; 8];
    let mut t = value;
    for i in (0..len).rev() {
        b[i] = (t & 0xFF) as u8;
        t >>= 8;
    }
    b[0] |= 1u8 << (8 - len);
    Ok(b[..len].to_vec())
}

fn make_void(total_len: usize) -> Result<Vec<u8>, String> {
    for size_len in 1..=8usize {
        if total_len < 1 + size_len {
            continue;
        }
        let payload = total_len - 1 - size_len;
        if vint_size(payload as u64) == size_len {
            let mut out = Vec::with_capacity(total_len);
            out.extend_from_slice(&[0xEC]);
            out.extend_from_slice(&encode_vint_exact(payload as u64, size_len)?);
            out.resize(total_len, 0);
            return Ok(out);
        }
    }
    Err(format!("cannot encode {total_len}-byte Void element"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attachment_roundtrip_and_rewrite() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let original = std::fs::read(path).unwrap();
        let demux = Demuxed::parse(original.clone()).unwrap();
        assert!(parse_attachments(&demux).is_empty());

        let art = vec![Attachment::new("cover_front.jpg", "image/jpeg", vec![0xff, 0xd8, 0xff, 0xe0, 1, 2, 3])];
        let rewritten = rewrite_attachments(&original, &art).unwrap();
        let check = Demuxed::parse(rewritten.clone()).unwrap();
        assert_eq!(check.immutable_tag("SENA_PROFILE"), Some("300"));
        let parsed = parse_attachments(&check);
        assert_eq!(parsed, art);

        // remove again (empty list voids the element)
        let removed = rewrite_attachments(&rewritten, &[]).unwrap();
        let check2 = Demuxed::parse(removed).unwrap();
        assert!(parse_attachments(&check2).is_empty());
        assert_eq!(check2.immutable_tag("SENA_PROFILE"), Some("300"));
        // clusters unchanged by both rewrites
        assert_eq!(
            crate::demux::read_cluster_starts(&rewritten).unwrap(),
            crate::demux::read_cluster_starts(&original).unwrap()
        );
    }
}
