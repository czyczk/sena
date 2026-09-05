//! In-place Matroska user-tag rewriting (void old tail Tags, append new one,
//! patch the Segment size). Immutable Sena tags stay in the first Tags
//! element and are never modified.
//!
//! The rewrite is planned as a handful of positioned writes
//! ([`plan_user_tag_rewrite`]) computed from a bounded-memory
//! [`index_container`](crate::demux::index_container) scan, so neither the
//! in-buffer helper nor the FFI path materializes a rewritten copy of the
//! whole file (32-bit hosts).

use sena_mux::ebml;

use crate::demux::{self, IndexedFile, WriteOp};

const TAGS_ID: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
const TAG_ID: &[u8] = &[0x73, 0x73];
const SIMPLE_TAG_ID: &[u8] = &[0x67, 0xC8];
const TAG_NAME_ID: &[u8] = &[0x45, 0xA3];
const TAG_STRING_ID: &[u8] = &[0x44, 0x87];

fn build_user_tags(entries: &[(String, String)]) -> Vec<u8> {
    let mut tag = Vec::new();
    for (k, v) in entries {
        let mut simple = Vec::new();
        ebml::str(&mut simple, TAG_NAME_ID, k);
        ebml::str(&mut simple, TAG_STRING_ID, v);
        ebml::write_element(&mut tag, SIMPLE_TAG_ID, &simple);
    }
    let mut tags_payload = Vec::new();
    if !tag.is_empty() {
        ebml::write_element(&mut tags_payload, TAG_ID, &tag);
    }
    let mut top = Vec::new();
    top.extend_from_slice(TAGS_ID);
    ebml::write_vint(&mut top, tags_payload.len() as u64);
    top.extend_from_slice(&tags_payload);
    top
}

/// Scan only the top-level EBML element headers (plus the small user `Tags`
/// payloads) and return the user metadata. This is the fast path used by
/// `sena_file_read_tags`: cluster payloads are skipped via the read-at
/// callback and never copied.
pub fn scan_user_tags<R>(read_at: &mut R) -> Result<Vec<(String, String)>, String>
where
    R: FnMut(u64, usize) -> Result<Vec<u8>, String>,
{
    fn id_len(first: u8) -> Result<usize, String> {
        let mut len = 0usize;
        let mut mask = 0x80u8;
        while mask != 0 && (first & mask) == 0 {
            mask >>= 1;
            len += 1;
        }
        if mask == 0 {
            return Err("invalid EBML element id".into());
        }
        Ok(len + 1)
    }

    fn vint_len(first: u8) -> Result<usize, String> {
        id_len(first)
    }

    // `buf` holds exactly one element header; `base` is its absolute offset.
    fn elem_at(buf: &[u8], base: u64) -> Result<(Vec<u8>, crate::demux::Elem), String> {
        let idl = id_len(*buf.first().ok_or("empty element header")?)?;
        let (size, sizel) = crate::demux::read_vint(buf, idl).map_err(|e| e.to_string())?;
        let size = usize::try_from(size).map_err(|_| "element too large".to_string())?;
        let data_start = base + (idl + sizel) as u64;
        let data_end = data_start
            .checked_add(size as u64)
            .ok_or("element size overflow".to_string())?;
        if data_end < data_start {
            return Err("element size overflow".into());
        }
        Ok((
            buf[..idl].to_vec(),
            crate::demux::Elem {
                start: base as usize,
                id_len: idl,
                size_len: sizel,
                data_start: data_start as usize,
                data_end: data_end as usize,
            },
        ))
    }

    fn header_at<R2>(
        read_at: &mut R2,
        pos: u64,
    ) -> Result<(Vec<u8>, crate::demux::Elem), String>
    where
        R2: FnMut(u64, usize) -> Result<Vec<u8>, String>,
    {
        // Read exactly the id + size vint bytes. Never over-read past the
        // element header: the last top-level element can sit at EOF and a
        // fixed 12-byte probe would otherwise fail.
        let first = read_at(pos, 1)?;
        let first = *first.first().ok_or("unexpected end of file in EBML header")?;
        let idl = id_len(first)?;
        let mut buf = vec![first];
        if idl > 1 {
            buf.extend(read_at(pos + 1, idl - 1)?);
        }
        let sizefirst = *read_at(pos + idl as u64, 1)?
            .first()
            .ok_or("unexpected end of file in EBML size")?;
        let sizel = vint_len(sizefirst)?;
        buf.push(sizefirst);
        if sizel > 1 {
            buf.extend(read_at(pos + idl as u64 + 1, sizel - 1)?);
        }
        elem_at(&buf, pos)
    }

    let (id0, header0) = header_at(read_at, 0)?;
    if id0 != crate::demux::SEGMENT_ID {
        // Standard files start with an EBML header (always small); skip it.
        if id0 != b"\x1a\x45\xdf\xa3" {
            return Err("not a Matroska file (missing EBML/Segment header)".into());
        }
    }
    let segment = if id0 == crate::demux::SEGMENT_ID {
        header0
    } else {
        let (id1, header1) = header_at(read_at, header0.data_end as u64)?;
        if id1 != crate::demux::SEGMENT_ID {
            return Err("missing Matroska Segment element".into());
        }
        header1
    };

    let mut pos = segment.data_start as u64;
    let end = segment.data_end as u64;
    let mut tag_index = 0usize;
    let mut entries = Vec::new();
    while pos < end {
        let (id, elem) = header_at(read_at, pos)?;
        if elem.data_end as u64 > end {
            return Err("top-level element overruns the Segment".into());
        }
        if id == TAGS_ID {
            if tag_index > 0 {
                let payload = read_at(elem.data_start as u64, elem.data_end - elem.data_start)?;
                let local = crate::demux::Elem {
                    start: 0,
                    id_len: elem.id_len,
                    size_len: elem.size_len,
                    data_start: 0,
                    data_end: payload.len(),
                };
                let parsed = crate::demux::parse_tags_payload(&payload, local)
                    .map_err(|e| e.to_string())?;
                entries.extend(parsed);
            }
            tag_index += 1;
        }
        pos = elem.data_end as u64;
    }
    Ok(entries)
}

/// Filter out entries that cannot be represented as Matroska string tags:
/// attached pictures (binary payload, would need Attachments) and values
/// with NUL bytes or invalid UTF-8 (the C ABI cannot carry them correctly).
fn sanitize_entries(entries: &[(String, String)]) -> Vec<(String, String)> {
    entries
        .iter()
        .filter(|(k, v)| {
            !k.eq_ignore_ascii_case("PICTURE")
                && !v.as_bytes().contains(&0)
                && std::str::from_utf8(v.as_bytes()).is_ok()
        })
        .cloned()
        .collect()
}

/// Compute the positioned writes that replace the user tags of a scanned
/// file with `entries`: void every Tags block after the immutable first one,
/// append the replacement at the Segment tail, re-patch the Segment size.
pub fn plan_user_tag_rewrite(
    scan: &IndexedFile,
    entries: &[(String, String)],
) -> Result<Vec<WriteOp>, String> {
    let entries = sanitize_entries(entries);
    let mut ops = Vec::new();
    for &(start, end) in scan.tag_ranges.iter().skip(1) {
        demux::push_void_ops(&mut ops, start, end)?;
    }
    let appended = if entries.is_empty() { Vec::new() } else { build_user_tags(&entries) };
    demux::push_append_ops(&mut ops, scan, appended)?;
    Ok(ops)
}

/// Rewrite user metadata in a complete Sena file buffer. Equivalent to
/// planning the rewrite on the buffer and applying it in place; kept for the
/// CLI/tests, the FFI write path applies the same plan through io callbacks.
pub fn rewrite_user_tags(original: &[u8], entries: &[(String, String)]) -> Result<Vec<u8>, String> {
    let scan = demux::index_container(
        &mut |pos: u64, len: usize| demux::mem_read_at(original, pos, len),
        false,
    )?;
    let ops = plan_user_tag_rewrite(&scan, entries)?;
    let mut out = original.to_vec();
    demux::apply_write_ops_to_vec(&mut out, &ops);

    // Verify the result still scans and the immutable Sena tags (including
    // the encoded-audio stream hash) remain at the head.
    let check = demux::index_container(
        &mut |pos: u64, len: usize| demux::mem_read_at(&out, pos, len),
        false,
    )
    .map_err(|e| format!("rewritten file invalid: {e}"))?;
    demux::check_immutable_survived(&scan, &check)?;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::demux::Demuxed;
    use crate::pipeline::Decoder;

    #[test]
    fn void_and_vint_exact() {
        let v = demux::encode_vint_exact(0x15b, 2).unwrap();
        assert_eq!(v, vec![0x41, 0x5b]);
        let header = demux::void_header(3).unwrap();
        assert_eq!(header, vec![0xEC, 0x81]);
        // The header plus the zeroed payload is exactly the old full Void.
        let mut ops = Vec::new();
        demux::push_void_ops(&mut ops, 10, 13).unwrap();
        let mut buf = vec![0xAA; 20];
        demux::apply_write_ops_to_vec(&mut buf, &ops);
        assert_eq!(&buf[10..13], &[0xEC, 0x81, 0x00]);
    }

    #[test]
    fn scan_user_tags_skips_cluster_payloads() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let original = std::fs::read(path).unwrap();
        let demux = Demuxed::parse(original.clone()).unwrap();
        let expected: Vec<(String, String)> = demux
            .tags
            .iter()
            .skip(1)
            .flat_map(|t| t.entries.clone())
            .collect();

        let mut bytes_served = 0usize;
        let mut read_at = |pos: u64, len: usize| -> Result<Vec<u8>, String> {
            bytes_served += len;
            let start = pos as usize;
            let end = start.checked_add(len).ok_or("range overflow")?;
            original
                .get(start..end)
                .map(|s| s.to_vec())
                .ok_or_else(|| "range outside file".to_string())
        };
        let got = scan_user_tags(&mut read_at).unwrap();
        assert_eq!(got, expected);
        // The whole point of the fast path: only EBML headers and the small
        // user-Tags payloads are fetched; the multi-MB cluster payloads are not.
        assert!(bytes_served < original.len() / 10, "tag scan read {} of {}", bytes_served, original.len());

        // A freshly rewritten tail Tags element is found by the scan as well.
        let tagged = rewrite_user_tags(
            &original,
            &[("TITLE".to_string(), "scan test".to_string()), ("ARTIST".to_string(), "zenas".to_string())],
        ).unwrap();
        let mut served2 = 0usize;
        let mut read_at2 = |pos: u64, len: usize| -> Result<Vec<u8>, String> {
            served2 += len;
            let start = pos as usize;
            tagged.get(start..start + len).map(|s| s.to_vec()).ok_or_else(|| "range outside file".to_string())
        };
        let got2 = scan_user_tags(&mut read_at2).unwrap();
        assert_eq!(got2, vec![
            ("TITLE".to_string(), "scan test".to_string()),
            ("ARTIST".to_string(), "zenas".to_string()),
        ]);
        assert!(served2 < tagged.len() / 10);
    }

    #[test]
    fn picture_and_binary_entries_are_skipped() {
        // foobar's attached-picture meta and any value with embedded NULs
        // must not reach the Matroska tag strings; the write must succeed
        // and the file must stay valid.
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let original = std::fs::read(path).unwrap();
        let mut pic = vec![0u8];
        pic.extend_from_slice(b"image/jpeg\0");
        pic.extend_from_slice(&[0xff, 0xd8, 0xff, 0xe0]);
        let entries = vec![
            ("TITLE".to_string(), "ok".to_string()),
            ("PICTURE".to_string(), String::from_utf8_lossy(&pic).into_owned()),
            ("BAD".to_string(), "a\0b".to_string()),
        ];
        let rewritten = rewrite_user_tags(&original, &entries).unwrap();
        let check = Demuxed::parse(rewritten.clone()).unwrap();
        assert_eq!(check.immutable_tag("SENA_PROFILE"), Some("300"));
        assert_eq!(check.immutable_tag("SENA_PLAYABLE_SAMPLES"), Some("960000"));
        let user: Vec<(String, String)> = check.tags.iter().skip(1).flat_map(|t| t.entries.clone()).collect();
        assert_eq!(user, vec![("TITLE".to_string(), "ok".to_string())]);
        // read-back via the scan path too
        let mut served = 0usize;
        let bytes = rewritten.clone();
        let mut read_at = |pos: u64, len: usize| -> Result<Vec<u8>, String> {
            served += len;
            bytes.get(pos as usize..pos as usize + len).map(|s| s.to_vec()).ok_or_else(|| "range".into())
        };
        let back = scan_user_tags(&mut read_at).unwrap();
        assert_eq!(back, vec![("TITLE".to_string(), "ok".to_string())]);
        // the container must still decode
        let dec = crate::pipeline::Decoder::open(&check).unwrap();
        assert_eq!(dec.info().playable_frames, 960000);
    }

    #[test]
    fn retag_preserves_clusters_and_immutable_tags() {
        let path = concat!(env!("CARGO_MANIFEST_DIR"), "/../../assets/e2e/01__p300__lfa__hf144.sena");
        let original = std::fs::read(path).unwrap();
        let cluster_positions: Vec<usize> = crate::demux::read_cluster_starts(&original).unwrap();

        let once = rewrite_user_tags(
            &original,
            &[("TITLE".to_string(), "first".to_string()), ("ARTIST".to_string(), "zenas".to_string())],
        ).unwrap();
        let check1 = Demuxed::parse(once.clone()).unwrap();
        assert_eq!(check1.immutable_tag("SENA_PROFILE"), Some("300"));
        assert_eq!(check1.immutable_tag("SENA_PLAYABLE_SAMPLES"), Some("960000"));
        assert_eq!(check1.tags.last().unwrap().get("TITLE"), Some("first"));
        assert_eq!(check1.tags.last().unwrap().get("ARTIST"), Some("zenas"));
        assert_eq!(crate::demux::read_cluster_starts(&once).unwrap(), cluster_positions);

        let twice = rewrite_user_tags(&once, &[("TITLE".to_string(), "second".to_string())]).unwrap();
        let check2 = Demuxed::parse(twice.clone()).unwrap();
        assert_eq!(check2.immutable_tag("SENA_PROFILE"), Some("300"));
        assert_eq!(check2.tags.last().unwrap().get("TITLE"), Some("second"));
        assert!(check2.tags.last().unwrap().get("ARTIST").is_none());
        assert_eq!(crate::demux::read_cluster_starts(&twice).unwrap(), cluster_positions);

        // The second file must still decode to the same playable length.
        let dec = Decoder::open(&check2).unwrap();
        assert_eq!(dec.info().playable_frames, 960000);
    }
}
