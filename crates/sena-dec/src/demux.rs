//! Minimal Matroska/EBML reader for the Sena container subset.

use std::fmt;

pub const SEGMENT_ID: &[u8] = &[0x18, 0x53, 0x80, 0x67];
const INFO_ID: &[u8] = &[0x15, 0x49, 0xA9, 0x66];
const TRACKS_ID: &[u8] = &[0x16, 0x54, 0xAE, 0x6B];
const TAGS_ID: &[u8] = &[0x12, 0x54, 0xC3, 0x67];
const CLUSTER_ID: &[u8] = &[0x1F, 0x43, 0xB6, 0x75];
const TRACK_ENTRY_ID: &[u8] = &[0xAE];
const TRACK_NUMBER_ID: &[u8] = &[0xD7];
const TRACK_TYPE_ID: &[u8] = &[0x83];
const CODEC_ID_ID: &[u8] = &[0x86];
const CODEC_PRIVATE_ID: &[u8] = &[0x63, 0xA2];
const CODEC_DELAY_ID: &[u8] = &[0x56, 0xAA];
const AUDIO_ID: &[u8] = &[0xE1];
const SAMPLING_FREQ_ID: &[u8] = &[0xB5];
const CHANNELS_ID: &[u8] = &[0x9F];
const TAG_ID: &[u8] = &[0x73, 0x73];
const SIMPLE_TAG_ID: &[u8] = &[0x67, 0xC8];
const TAG_NAME_ID: &[u8] = &[0x45, 0xA3];
const TAG_STRING_ID: &[u8] = &[0x44, 0x87];
const CLUSTER_TIMESTAMP_ID: &[u8] = &[0x07, 0xE7];
const SIMPLE_BLOCK_ID: &[u8] = &[0xA3];
const TIMECODE_SCALE_NS: u64 = 1_000_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Elem {
    pub start: usize,
    pub id_len: usize,
    pub size_len: usize,
    pub data_start: usize,
    pub data_end: usize,
}

impl Elem {
    pub fn len_encoded(&self) -> usize {
        self.data_end - self.start
    }
    pub fn payload(&self) -> std::ops::Range<usize> {
        self.data_start..self.data_end
    }
}

#[derive(Debug, Clone)]
pub struct Track {
    pub number: u64,
    pub codec_id: String,
    pub codec_private: Vec<u8>,
    pub sample_rate: f64,
    pub channels: u64,
    pub codec_delay_ns: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct TagBlock {
    pub elem: Elem,
    pub entries: Vec<(String, String)>,
}

impl TagBlock {
    pub fn get(&self, key: &str) -> Option<&str> {
        self.entries
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct Frame {
    pub track: u64,
    pub t_ns: u64,
    pub data: Vec<u8>,
}

#[derive(Debug, Clone)]
pub struct SegmentInfo {
    pub elem: Elem,
    pub payload_start: usize,
    pub payload_end: usize,
}

#[derive(Debug)]
pub struct Demuxed {
    bytes: Vec<u8>,
    pub segment: SegmentInfo,
    pub tracks: Vec<Track>,
    pub tags: Vec<TagBlock>,
    pub frames: Vec<Frame>,
    pub warnings: Vec<String>,
}

#[derive(Debug)]
pub enum DemuxError {
    Truncated { context: &'static str },
    Malformed(String),
    Unsupported(String),
}

impl fmt::Display for DemuxError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            DemuxError::Truncated { context } => write!(f, "truncated EBML: {context}"),
            DemuxError::Malformed(s) => write!(f, "malformed container: {s}"),
            DemuxError::Unsupported(s) => write!(f, "unsupported container feature: {s}"),
        }
    }
}

impl std::error::Error for DemuxError {}

pub fn read_uint(data: &[u8], ctx: &'static str) -> Result<u64, DemuxError> {
    if data.is_empty() || data.len() > 8 {
        return Err(DemuxError::Malformed(format!("{ctx}: bad uint length")));
    }
    let mut v = 0u64;
    for &b in data {
        v = (v << 8) | u64::from(b);
    }
    Ok(v)
}

pub fn read_vint(data: &[u8], pos: usize) -> Result<(u64, usize), DemuxError> {
    let first = *data
        .get(pos)
        .ok_or(DemuxError::Truncated { context: "vint" })?;
    let mut len = 0usize;
    let mut mask = 0x80u8;
    while mask != 0 && (first & mask) == 0 {
        mask >>= 1;
        len += 1;
    }
    if mask == 0 {
        return Err(DemuxError::Malformed(format!("invalid vint at byte {pos}")));
    }
    len += 1;
    let bytes = data
        .get(pos..pos + len)
        .ok_or(DemuxError::Truncated { context: "vint bytes" })?;
    let mut v = 0u64;
    for &b in bytes {
        v = (v << 8) | u64::from(b);
    }
    v &= (1u64 << (7 * len)) - 1;
    Ok((v, len))
}

fn read_id(data: &[u8], pos: usize) -> Result<(&[u8], usize), DemuxError> {
    let first = *data
        .get(pos)
        .ok_or(DemuxError::Truncated { context: "element id" })?;
    if first & 0x80 != 0 {
        return Ok((&data[pos..pos + 1], 1));
    }
    let mut len = 0usize;
    let mut mask = 0x80u8;
    while mask != 0 && (first & mask) == 0 {
        mask >>= 1;
        len += 1;
    }
    if mask == 0 {
        return Err(DemuxError::Malformed("invalid element id".into()));
    }
    len += 1;
    let end = pos
        .checked_add(len)
        .ok_or(DemuxError::Malformed("id overflow".into()))?;
    Ok((data.get(pos..end).ok_or(DemuxError::Truncated { context: "id" })?, len))
}

/// Public wrapper over the internal EBML element reader (used by the
/// attachments/tags scanner helpers).
pub fn read_elem_public(data: &[u8], pos: usize) -> Result<Elem, DemuxError> {
    read_elem(data, pos)
}

fn read_elem(data: &[u8], pos: usize) -> Result<Elem, DemuxError> {
    let (_, id_len) = read_id(data, pos)?;
    let (size, size_len) = read_vint(data, pos + id_len)?;
    let size = usize::try_from(size).map_err(|_| DemuxError::Unsupported("element too large".into()))?;
    let data_start = pos + id_len + size_len;
    let data_end = data_start
        .checked_add(size)
        .ok_or(DemuxError::Malformed("element size overflow".into()))?;
    if data_end > data.len() {
        return Err(DemuxError::Malformed(format!("element at byte {pos} overruns file: payload ends {data_end}, file {}", data.len())));
    }
    Ok(Elem { start: pos, id_len, size_len, data_start, data_end })
}

fn for_each_child(data: &[u8], elem: Elem, mut f: impl FnMut(&[u8], Elem) -> Result<(), DemuxError>) -> Result<(), DemuxError> {
    let mut p = elem.data_start;
    while p < elem.data_end {
        let child = read_elem(data, p)?;
        let id = data[child.start..child.start + child.id_len].to_vec();
        f(&id, child)?;
        p = child.data_end;
    }
    Ok(())
}

fn parse_float_payload(data: &[u8], elem: Elem) -> Result<f64, DemuxError> {
    let p = elem.payload();
    let bytes = data.get(p.clone()).ok_or(DemuxError::Truncated { context: "float payload" })?;
    match bytes.len() {
        4 => {
            let b: [u8; 4] = bytes.try_into().unwrap();
            Ok(f64::from(f32::from_be_bytes(b)))
        }
        8 => {
            let b: [u8; 8] = bytes.try_into().unwrap();
            Ok(f64::from_be_bytes(b))
        }
        n => Err(DemuxError::Malformed(format!("bad float length {n}"))),
    }
}

pub(crate) fn parse_tags_payload(data: &[u8], elem: Elem) -> Result<Vec<(String, String)>, DemuxError> {
    let mut entries = Vec::new();
    for_each_child(data, elem, |id, child| {
        if id != TAG_ID {
            return Ok(());
        }
        for_each_child(data, child, |id2, simple| {
            if id2 != SIMPLE_TAG_ID {
                return Ok(());
            }
            let mut name = None;
            let mut value = None;
            for_each_child(data, simple, |id3, field| {
                if id3 == TAG_NAME_ID {
                    name = Some(
                        String::from_utf8_lossy(data.get(field.payload()).unwrap_or(&[])).into_owned(),
                    );
                } else if id3 == TAG_STRING_ID {
                    value = Some(
                        String::from_utf8_lossy(data.get(field.payload()).unwrap_or(&[])).into_owned(),
                    );
                }
                Ok(())
            })?;
            if let (Some(k), Some(v)) = (name, value) {
                entries.push((k, v));
            }
            Ok(())
        })
    })?;
    Ok(entries)
}

fn parse_track(data: &[u8], elem: Elem) -> Result<Track, DemuxError> {
    let mut number = 0u64;
    let mut codec_id = String::new();
    let mut codec_private = Vec::new();
    let mut sample_rate = 0.0f64;
    let mut channels = 0u64;
    let mut codec_delay_ns = None;
    for_each_child(data, elem, |id, child| {
        match id {
            TRACK_NUMBER_ID => number = read_uint(data.get(child.payload()).unwrap_or(&[]), "track number")?,
            TRACK_TYPE_ID => {
                let t = read_uint(data.get(child.payload()).unwrap_or(&[]), "track type")?;
                if t != 2 {
                    return Err(DemuxError::Unsupported(format!("track type {t}, expected audio")));
                }
            }
            CODEC_ID_ID => {
                codec_id = String::from_utf8_lossy(data.get(child.payload()).unwrap_or(&[])).into_owned();
            }
            CODEC_PRIVATE_ID => {
                codec_private = data.get(child.payload()).unwrap_or(&[]).to_vec();
            }
            CODEC_DELAY_ID => {
                codec_delay_ns = Some(read_uint(data.get(child.payload()).unwrap_or(&[]), "CodecDelay")?);
            }
            AUDIO_ID => {
                for_each_child(data, child, |aid, achild| {
                    if aid == SAMPLING_FREQ_ID {
                        sample_rate = parse_float_payload(data, achild)?;
                    } else if aid == CHANNELS_ID {
                        channels = read_uint(data.get(achild.payload()).unwrap_or(&[]), "channels")?;
                    }
                    Ok(())
                })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if number == 0 || codec_id.is_empty() || sample_rate <= 0.0 || channels == 0 {
        return Err(DemuxError::Malformed("incomplete track entry".into()));
    }
    Ok(Track { number, codec_id, codec_private, sample_rate, channels, codec_delay_ns })
}

fn parse_simple_block(data: &[u8], payload: std::ops::Range<usize>) -> Result<(u64, u64, Vec<u8>), DemuxError> {
    let p = payload.start;
    let (track, tn_len) = read_vint(data, p)?;
    let rel_pos = p + tn_len;
    let rel = i16::from_be_bytes(
        data.get(rel_pos..rel_pos + 2)
            .ok_or(DemuxError::Truncated { context: "simpleblock relative timestamp" })?
            .try_into()
            .unwrap(),
    );
    let flags = *data
        .get(rel_pos + 2)
        .ok_or(DemuxError::Truncated { context: "simpleblock flags" })?;
    if flags & 0x06 != 0 {
        return Err(DemuxError::Unsupported("laced frames".into()));
    }
    let body_start = rel_pos + 3;
    let body = data
        .get(body_start..payload.end)
        .ok_or(DemuxError::Truncated { context: "simpleblock body" })?;
    Ok((track, rel as u64 as i128 as u64, body.to_vec()))
}

/// Return the byte offsets of every top-level Cluster element (test/diagnostic
/// aid for the in-place tag writer's "clusters never move" guarantee).
pub fn read_cluster_starts(data: &[u8]) -> Result<Vec<usize>, DemuxError> {
    let mut p = 0usize;
    let mut segment = None;
    while p < data.len() {
        let elem = read_elem(data, p)?;
        let id = &data[elem.start..elem.start + elem.id_len];
        if id == SEGMENT_ID {
            segment = Some(elem);
            break;
        }
        p = elem.data_end;
    }
    let segment = segment.ok_or_else(|| DemuxError::Malformed("missing Segment".into()))?;
    let mut out = Vec::new();
    for_each_child(data, segment, |id, child| {
        if id == CLUSTER_ID {
            out.push(child.start);
        }
        Ok(())
    })?;
    Ok(out)
}

impl Demuxed {
    pub fn parse(bytes: Vec<u8>) -> Result<Self, DemuxError> {
        if bytes.len() < 4 || &bytes[0..4] != b"\x1A\x45\xDF\xA3" {
            return Err(DemuxError::Malformed("missing EBML header".into()));
        }
        let mut segment = None;
        let mut p = 0;
        while p < bytes.len() {
            let elem = read_elem(&bytes, p)?;
            let id = &bytes[elem.start..elem.start + elem.id_len];
            if id == SEGMENT_ID {
                segment = Some(elem);
                break;
            }
            p = elem.data_end;
        }
        let segment = segment.ok_or_else(|| DemuxError::Malformed("missing Segment".into()))?;
        let (seg_size, _) = read_vint(&bytes, segment.start + segment.id_len)?;
        let max_seg = (1u128 << (7 * segment.size_len)) - 1;
        if u128::from(seg_size) == max_seg {
            return Err(DemuxError::Unsupported("unknown-size Segment".into()));
        }
        let payload_start = segment.data_start;
        let payload_end = segment.data_end;
        let mut tracks = Vec::new();
        let mut tags = Vec::new();
        let mut frames = Vec::new();
        for_each_child(&bytes, segment, |id, child| {
            if id == TRACKS_ID {
                for_each_child(&bytes, child, |tid, tentry| {
                    if tid == TRACK_ENTRY_ID {
                        tracks.push(parse_track(&bytes, tentry)?);
                    }
                    Ok(())
                })?;
            } else if id == TAGS_ID {
                let entries = parse_tags_payload(&bytes, child)?;
                tags.push(TagBlock { elem: child, entries });
            } else if id == CLUSTER_ID {
                let mut cluster_t = 0u64;
                for_each_child(&bytes, child, |cid, cchild| {
                    if cid == CLUSTER_TIMESTAMP_ID {
                        cluster_t = read_uint(&bytes[cchild.payload()], "cluster timestamp")?;
                    } else if cid == SIMPLE_BLOCK_ID {
                        let (track, rel, data) = parse_simple_block(&bytes, cchild.payload())?;
                        let t_ns = cluster_t
                            .saturating_mul(TIMECODE_SCALE_NS)
                            .saturating_add(rel.saturating_mul(TIMECODE_SCALE_NS));
                        frames.push(Frame { track, t_ns, data });
                    }
                    Ok(())
                })?;
            } else if id == INFO_ID {
                // TimestampScale is not needed here: the muxer fixes 1 ms.
            }
            Ok(())
        })?;
        // Cluster/block order is authoritative for whole-file decode: the
        // muxer packs frames in presentation order, while 16-bit relative
        // timestamps can wrap negative inside a 1 s cluster.
        // `t_ns` remains available for diagnostics and bitrate mapping.
        let mut out = Self {
            bytes,
            segment: SegmentInfo { elem: segment, payload_start, payload_end },
            tracks,
            tags,
            frames,
            warnings: Vec::new(),
        };
        out.validate_layout();
        Ok(out)
    }

    fn validate_layout(&mut self) {
        if self.tracks.iter().filter(|t| t.codec_id == "A_OPUS").count() != 1 {
            self.warnings.push("expected exactly one A_OPUS track".into());
        }
        if self.tracks.iter().filter(|t| t.codec_id == "A_SENALF").count() != 1 {
            self.warnings.push("expected exactly one A_SENALF track".into());
        }
    }

    pub fn bytes(&self) -> &[u8] {
        &self.bytes
    }

    pub fn into_bytes(self) -> Vec<u8> {
        self.bytes
    }

    pub fn tag(&self, key: &str) -> Option<&str> {
        self.tags.iter().find_map(|t| t.get(key))
    }

    pub fn immutable_tag(&self, key: &str) -> Option<&str> {
        self.tags.first().and_then(|t| t.get(key))
    }

    pub fn track(&self, codec_id: &str) -> Option<&Track> {
        self.tracks.iter().find(|t| t.codec_id == codec_id)
    }

    pub fn frames_for(&self, track_number: u64) -> impl Iterator<Item = &Frame> {
        self.frames.iter().filter(move |f| f.track == track_number)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_roundtrip() {
        let mut buf = Vec::new();
        sena_mux::ebml::write_vint(&mut buf, 0);
        assert_eq!(read_vint(&buf, 0).unwrap(), (0, 1));
        let mut buf = Vec::new();
        sena_mux::ebml::write_vint(&mut buf, 127);
        assert_eq!(read_vint(&buf, 0).unwrap(), (127, 2));
        let mut buf = Vec::new();
        sena_mux::ebml::write_vint(&mut buf, 128);
        assert_eq!(read_vint(&buf, 0).unwrap(), (128, 2));
    }
}
