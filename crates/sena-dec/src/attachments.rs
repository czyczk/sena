//! Matroska Attachments: parse and in-place rewrite of embedded files
//! (album art lives here in Matroska, NOT in string Tags).
//!
//! Rewrites are planned as positioned writes ([`plan_attachment_rewrite`])
//! from a bounded-memory [`index_container`](crate::demux::index_container)
//! scan, like the tag rewriter; no whole-file copies.

use sena_mux::ebml;

use crate::demux::{self, Demuxed, Elem, IndexedFile, WriteOp};

const ATTACHED_FILE_ID: &[u8] = &[0x61, 0xA7];
const FILE_NAME_ID: &[u8] = &[0x46, 0x6E];
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
pub fn parse_attachments(demuxed: &Demuxed) -> Vec<Attachment> {
    let bytes = demuxed.bytes();
    let mut out = Vec::new();
    // Attachments is a top-level Segment child.
    let seg = demuxed.segment.elem;
    let _result = for_each_child(bytes, seg, |id, elem| {
        if id != demux::ATTACHMENTS_ID {
            return Ok(());
        }
        out.extend(parse_attachments_payload(bytes.get(elem.payload()).unwrap_or(&[])));
        Ok(())
    });
    // errors in the walk stop scanning (tolerated)
    let _ = _result;
    out
}

/// Parse one Attachments element payload (a series of AttachedFile children).
/// Tolerates malformed trailing data by stopping.
pub fn parse_attachments_payload(payload: &[u8]) -> Vec<Attachment> {
    let mut out = Vec::new();
    let top = Elem { start: 0, id_len: 0, size_len: 0, data_start: 0, data_end: payload.len() };
    let _ = for_each_child(payload, top, |cid, cchild| {
        if cid != ATTACHED_FILE_ID {
            return Ok(());
        }
        let mut name = None;
        let mut mime = None;
        let mut data = None;
        for_each_child(payload, cchild, |fid, ff| {
            match fid {
                FILE_NAME_ID => name = Some(String::from_utf8_lossy(payload.get(ff.payload()).unwrap_or(&[])).into_owned()),
                FILE_MIME_ID => mime = Some(String::from_utf8_lossy(payload.get(ff.payload()).unwrap_or(&[])).into_owned()),
                FILE_DATA_ID => data = Some(payload.get(ff.payload()).unwrap_or(&[]).to_vec()),
                _ => {}
            }
            Ok(())
        })?;
        if let (Some(name), Some(mime), Some(data)) = (name, mime, data) {
            out.push(Attachment { name, mime, data });
        }
        Ok(())
    });
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
    out.extend_from_slice(demux::ATTACHMENTS_ID);
    ebml::write_vint(&mut out, payload.len() as u64);
    out.extend_from_slice(&payload);
    out
}

/// Compute the positioned writes that replace the full attachment set of a
/// scanned file: void every existing Attachments element, insert the
/// replacement at the head (right after the Tracks element), re-patch the
/// Segment size. Head placement makes cover art visible as attached_pic
/// streams at probe time; a mid-file Insert op is only valid for the
/// in-memory apply path, so the io-callback path must use the buffered
/// whole-file rewrite for it.
pub fn plan_attachment_rewrite(
    scan: &IndexedFile,
    attachments: &[Attachment],
) -> Result<Vec<WriteOp>, String> {
    let mut ops = Vec::new();
    for &(start, _, end) in &scan.attachment_ranges {
        demux::push_void_ops(&mut ops, start, end)?;
    }
    if !attachments.is_empty() {
        let appended = encode_attachments(attachments);
        let new_payload_len =
            (scan.segment_payload_end - scan.segment_payload_start) + appended.len() as u64;
        let patch = demux::encode_vint_exact(new_payload_len, scan.segment_size_len)?;
        ops.push(WriteOp::Insert {
            offset: scan.tracks_end,
            data: appended,
        });
        ops.push(WriteOp::Bytes {
            offset: scan.segment_size_pos,
            data: patch,
        });
    }
    Ok(ops)
}

/// Rewrite the attachment set in a complete file buffer; the FFI write path
/// applies the same plan through io callbacks.
pub fn rewrite_attachments(original: &[u8], attachments: &[Attachment]) -> Result<Vec<u8>, String> {
    let scan = demux::index_container(
        &mut |pos: u64, len: usize| demux::mem_read_at(original, pos, len),
        false,
    )?;
    let ops = plan_attachment_rewrite(&scan, attachments)?;
    let mut out = original.to_vec();
    demux::apply_write_ops_to_vec(&mut out, &ops);

    // Verify the result still scans and the SENA-INIT track is intact.
    let check = demux::index_container(
        &mut |pos: u64, len: usize| demux::mem_read_at(&out, pos, len),
        false,
    )
    .map_err(|e| format!("rewritten file invalid: {e}"))?;
    if check.init != scan.init {
        return Err("SENA-INIT changed after rewrite".into());
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
        assert_eq!(check.init.playable_samples, 960000);
        let parsed = parse_attachments(&check);
        assert_eq!(parsed, art);

        // remove again (empty list voids the element)
        let removed = rewrite_attachments(&rewritten, &[]).unwrap();
        let check2 = Demuxed::parse(removed.clone()).unwrap();
        assert!(parse_attachments(&check2).is_empty());
        assert_eq!(check2.init.playable_samples, 960000);
        // Head-positioned Attachments shift the clusters as a block by the
        // inserted element's length; the removal (void, no insert) leaves
        // the shift in place, and the file still decodes.
        let shift = rewritten.len() - original.len();
        let want: Vec<usize> = crate::demux::read_cluster_starts(&original)
            .unwrap()
            .iter()
            .map(|p| p + shift)
            .collect();
        assert_eq!(
            crate::demux::read_cluster_starts(&rewritten).unwrap(),
            want
        );
        assert_eq!(
            crate::demux::read_cluster_starts(&removed).unwrap(),
            want
        );
        let dec = crate::pipeline::Decoder::open(&check2).unwrap();
        assert_eq!(dec.info().playable_frames, 960000);
    }
}
