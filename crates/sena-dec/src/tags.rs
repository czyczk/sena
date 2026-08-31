//! In-place Matroska user-tag rewriting (void old tail Tags, append new one,
//! patch the Segment size). Immutable Sena tags stay in the first Tags
//! element and are never modified.

use sena_mux::ebml;

use crate::demux::Demuxed;

const TAGS_ID: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
const TAG_ID: &[u8] = &[0x73, 0x73];
const SIMPLE_TAG_ID: &[u8] = &[0x67, 0xC8];
const TAG_NAME_ID: &[u8] = &[0x45, 0xA3];
const TAG_STRING_ID: &[u8] = &[0x44, 0x87];
const VOID_ID: &[u8] = &[0xEC];

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
            out.extend_from_slice(VOID_ID);
            out.extend_from_slice(&encode_vint_exact(payload as u64, size_len)?);
            out.resize(total_len, 0);
            return Ok(out);
        }
    }
    Err(format!("cannot encode {total_len}-byte Void element"))
}

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

/// Rewrite user metadata in a complete Sena file buffer.
///
/// The first top-level `Tags` element is treated as immutable and is left
/// untouched. Every later top-level `Tags` element is voided in place with an
/// EBML `Void` of exactly the same encoded length. A replacement user `Tags`
/// element is appended at the Segment tail (or omitted when `entries` is
/// empty), and the Segment size is re-patched with its original size-vint
/// width. Cluster and Cue bytes are never moved.
pub fn rewrite_user_tags(original: &[u8], entries: &[(String, String)]) -> Result<Vec<u8>, String> {
    let demux = Demuxed::parse(original.to_vec()).map_err(|e| e.to_string())?;
    let seg = demux.segment.clone();
    let mut out = original.to_vec();

    let user_tags: Vec<_> = demux.tags.iter().skip(1).collect();
    for tag in user_tags {
        let start = tag.elem.start;
        let end = tag.elem.data_end;
        let void = make_void(end - start)?;
        out.splice(start..end, void);
    }

    let old_payload_len = seg.payload_end - seg.payload_start;
    let appended = if entries.is_empty() { Vec::new() } else { build_user_tags(entries) };
    let new_payload_len = old_payload_len + appended.len();
    let insert_at = seg.payload_end;
    out.splice(insert_at..insert_at, appended.iter().copied());

    let size_pos = seg.elem.start + seg.elem.id_len;
    let new_size = encode_vint_exact(new_payload_len as u64, seg.elem.size_len)?;
    out.splice(size_pos..size_pos + seg.elem.size_len, new_size.iter().copied());

    // Verify the result still parses and immutable tags remain at the head.
    let check = Demuxed::parse(out.clone()).map_err(|e| format!("rewritten file invalid: {e}"))?;
    for key in ["SENA_PROFILE", "SENA_VERSION", "SENA_PLAYABLE_SAMPLES"] {
        if check.immutable_tag(key).is_none() {
            return Err(format!("immutable tag {key} lost after rewrite"));
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::pipeline::Decoder;

    #[test]
    fn void_and_vint_exact() {
        let v = encode_vint_exact(0x15b, 2).unwrap();
        assert_eq!(v, vec![0x41, 0x5b]);
        let void = make_void(3).unwrap();
        assert_eq!(void, vec![0xEC, 0x81, 0x00]);
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
