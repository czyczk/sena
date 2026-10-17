//! Minimal Matroska/EBML reader for the Sena v2 container subset:
//! exactly one `A_MS/ACM` audio track carrying composite ES blocks.

use std::fmt;

use sena_core::es;

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
const AUDIO_ID: &[u8] = &[0xE1];
const SAMPLING_FREQ_ID: &[u8] = &[0xB5];
const CHANNELS_ID: &[u8] = &[0x9F];
const TAG_ID: &[u8] = &[0x73, 0x73];
const SIMPLE_TAG_ID: &[u8] = &[0x67, 0xC8];
const TAG_NAME_ID: &[u8] = &[0x45, 0xA3];
const TAG_STRING_ID: &[u8] = &[0x44, 0x87];
const CLUSTER_TIMESTAMP_ID: &[u8] = &[0xE7];
const SIMPLE_BLOCK_ID: &[u8] = &[0xA3];
pub(crate) const ATTACHMENTS_ID: &[u8] = &[0x19, 0x41, 0xA4, 0x69];
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
    /// TrackType (1 = video, 2 = audio, ...); non-audio tracks are ignored
    /// by the Sena reader (a remuxed video MKV stays readable).
    pub track_type: u64,
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

/// One composite ES block (see sena_core::es) with its playable-timeline
/// timestamp.
#[derive(Debug, Clone)]
pub struct Block {
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
    /// Validated SENA-INIT blob from the track's CodecPrivate.
    pub init: es::Init,
    /// Declared nAvgBytesPerSec of the WAVEFORMATEX wrapper.
    pub avg_bytes_per_sec: u32,
    pub tags: Vec<TagBlock>,
    pub blocks: Vec<Block>,
    /// Encoded size of all Cluster elements (codec payloads plus block
    /// framing); the audio-only size an average-bitrate display should use.
    pub audio_span_bytes: u64,
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
            DemuxError::Unsupported(s) => write!(f, "unsupported: {s}"),
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
    let bytes = data.get(pos..pos + len).ok_or(DemuxError::Truncated {
        context: "vint bytes",
    })?;
    let mut v = 0u64;
    for &b in bytes {
        v = (v << 8) | u64::from(b);
    }
    v &= (1u64 << (7 * len)) - 1;
    Ok((v, len))
}

fn read_id(data: &[u8], pos: usize) -> Result<(&[u8], usize), DemuxError> {
    let first = *data.get(pos).ok_or(DemuxError::Truncated {
        context: "element id",
    })?;
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
    Ok((
        data.get(pos..end)
            .ok_or(DemuxError::Truncated { context: "id" })?,
        len,
    ))
}

/// Public wrapper over the internal EBML element reader (used by the
/// attachments/tags scanner helpers).
pub fn read_elem_public(data: &[u8], pos: usize) -> Result<Elem, DemuxError> {
    read_elem(data, pos)
}

fn read_elem(data: &[u8], pos: usize) -> Result<Elem, DemuxError> {
    let (_, id_len) = read_id(data, pos)?;
    let (size, size_len) = read_vint(data, pos + id_len)?;
    let size =
        usize::try_from(size).map_err(|_| DemuxError::Unsupported("element too large".into()))?;
    let data_start = pos + id_len + size_len;
    let data_end = data_start
        .checked_add(size)
        .ok_or(DemuxError::Malformed("element size overflow".into()))?;
    if data_end > data.len() {
        return Err(DemuxError::Malformed(format!(
            "element at byte {pos} overruns file: payload ends {data_end}, file {}",
            data.len()
        )));
    }
    Ok(Elem {
        start: pos,
        id_len,
        size_len,
        data_start,
        data_end,
    })
}

fn for_each_child(
    data: &[u8],
    elem: Elem,
    mut f: impl FnMut(&[u8], Elem) -> Result<(), DemuxError>,
) -> Result<(), DemuxError> {
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
    let bytes = data.get(p.clone()).ok_or(DemuxError::Truncated {
        context: "float payload",
    })?;
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

pub(crate) fn parse_tags_payload(
    data: &[u8],
    elem: Elem,
) -> Result<Vec<(String, String)>, DemuxError> {
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
                        String::from_utf8_lossy(data.get(field.payload()).unwrap_or(&[]))
                            .into_owned(),
                    );
                } else if id3 == TAG_STRING_ID {
                    value = Some(
                        String::from_utf8_lossy(data.get(field.payload()).unwrap_or(&[]))
                            .into_owned(),
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
    let mut track_type = 0u64;
    let mut codec_id = String::new();
    let mut codec_private = Vec::new();
    let mut sample_rate = 0.0f64;
    let mut channels = 0u64;
    for_each_child(data, elem, |id, child| {
        match id {
            TRACK_NUMBER_ID => {
                number = read_uint(data.get(child.payload()).unwrap_or(&[]), "track number")?
            }
            TRACK_TYPE_ID => {
                track_type = read_uint(data.get(child.payload()).unwrap_or(&[]), "track type")?;
            }
            CODEC_ID_ID => {
                codec_id =
                    String::from_utf8_lossy(data.get(child.payload()).unwrap_or(&[])).into_owned();
            }
            CODEC_PRIVATE_ID => {
                codec_private = data.get(child.payload()).unwrap_or(&[]).to_vec();
            }
            AUDIO_ID => {
                for_each_child(data, child, |aid, achild| {
                    if aid == SAMPLING_FREQ_ID {
                        sample_rate = parse_float_payload(data, achild)?;
                    } else if aid == CHANNELS_ID {
                        channels =
                            read_uint(data.get(achild.payload()).unwrap_or(&[]), "channels")?;
                    }
                    Ok(())
                })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if number == 0 || codec_id.is_empty() {
        return Err(DemuxError::Malformed("incomplete track entry".into()));
    }
    Ok(Track {
        number,
        codec_id,
        codec_private,
        sample_rate,
        channels,
        track_type,
    })
}

fn parse_simple_block_header(
    data: &[u8],
    payload: std::ops::Range<usize>,
) -> Result<(u64, i16, usize), DemuxError> {
    let p = payload.start;
    let (track, tn_len) = read_vint(data, p)?;
    let rel_pos = p + tn_len;
    let rel = i16::from_be_bytes(
        data.get(rel_pos..rel_pos + 2)
            .ok_or(DemuxError::Truncated {
                context: "simpleblock relative timestamp",
            })?
            .try_into()
            .unwrap(),
    );
    let flags = *data.get(rel_pos + 2).ok_or(DemuxError::Truncated {
        context: "simpleblock flags",
    })?;
    if flags & 0x06 != 0 {
        return Err(DemuxError::Unsupported("laced frames".into()));
    }
    Ok((track, rel, tn_len + 3))
}

fn parse_simple_block(
    data: &[u8],
    payload: std::ops::Range<usize>,
) -> Result<(u64, u64, Vec<u8>), DemuxError> {
    let (track, rel, header_len) = parse_simple_block_header(data, payload.clone())?;
    let body = data
        .get(payload.start + header_len..payload.end)
        .ok_or(DemuxError::Truncated {
            context: "simpleblock body",
        })?;
    // Sign-extending cast: negative relative timestamps wrap into large u64
    // values, and the caller's saturating_mul/add keeps them in range.
    Ok((track, rel as u64, body.to_vec()))
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

/// Validate the track layout of a Sena v2 file: exactly one `A_MS/ACM`
/// audio track whose CodecPrivate holds a valid SENA-INIT blob. v1
/// multi-track files are recognized and rejected with a clear error.
/// Returns the INIT blob and the declared nAvgBytesPerSec.
fn sena_track_init(tracks: &[Track]) -> Result<(es::Init, u32, u64), DemuxError> {
    if tracks
        .iter()
        .any(|t| matches!(t.codec_id.as_str(), "A_OPUS" | "A_SENALF" | "A_OPUSHF"))
    {
        return Err(DemuxError::Unsupported(
            "v1 multi-track Sena file; re-encode to the v2 single-track format".into(),
        ));
    }
    let sena: Vec<&Track> = tracks
        .iter()
        .filter(|t| t.track_type == 2 && t.codec_id == es::SENA_CODEC_ID)
        .collect();
    if sena.len() != 1 {
        return Err(DemuxError::Unsupported(format!(
            "expected exactly one {} audio track, found {}",
            es::SENA_CODEC_ID,
            sena.len()
        )));
    }
    let t = sena[0];
    let (init, avg) =
        es::parse_codec_private(&t.codec_private).map_err(|e| DemuxError::Unsupported(format!("SENA-INIT: {e}")))?;
    if t.sample_rate.round() as u32 != sena_core::SAMPLE_RATE || t.channels != 2 {
        return Err(DemuxError::Unsupported(
            "Sena track rate/channels do not match 48000 Hz stereo".into(),
        ));
    }
    Ok((init, avg, t.number))
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
        let mut blocks = Vec::new();
        let mut audio_span_bytes = 0u64;
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
                tags.push(TagBlock {
                    elem: child,
                    entries,
                });
            } else if id == CLUSTER_ID {
                audio_span_bytes += (child.data_end - child.start) as u64;
                let mut cluster_t = 0u64;
                for_each_child(&bytes, child, |cid, cchild| {
                    if cid == CLUSTER_TIMESTAMP_ID {
                        cluster_t = read_uint(&bytes[cchild.payload()], "cluster timestamp")?;
                    } else if cid == SIMPLE_BLOCK_ID {
                        let (track, rel, data) = parse_simple_block(&bytes, cchild.payload())?;
                        if track != 1 {
                            return Err(DemuxError::Malformed(format!(
                                "SimpleBlock for track {track} in a single-track Sena file"
                            )));
                        }
                        let t_ns = cluster_t
                            .saturating_mul(TIMECODE_SCALE_NS)
                            .saturating_add(rel.saturating_mul(TIMECODE_SCALE_NS));
                        blocks.push(Block { t_ns, data });
                    }
                    Ok(())
                })?;
            } else if id == INFO_ID {
                // TimestampScale is not needed here: the muxer fixes 1 ms.
            }
            Ok(())
        })?;
        // Cluster/block order is authoritative for whole-file decode: the
        // muxer packs blocks in presentation order. `t_ns` remains available
        // for diagnostics and bitrate mapping.
        let (init, avg_bytes_per_sec, _track_number) = sena_track_init(&tracks)?;
        Ok(Self {
            bytes,
            segment: SegmentInfo {
                elem: segment,
                payload_start,
                payload_end,
            },
            init,
            avg_bytes_per_sec,
            tags,
            blocks,
            audio_span_bytes,
            warnings: Vec::new(),
        })
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
}

/* ------------------------------------------------------------ indexed scan
   Bounded-memory container view: the walker below reads only EBML element
   headers plus the small head payloads (Tracks/Tags), skipping cluster
   payloads via the positioned-read callback. Blocks enter as index entries
   (absolute byte range + ES-level metadata for the seek scan), never as
   payload copies. This keeps every plugin instance at O(index) memory
   regardless of file size, which matters on 32-bit hosts (foobar2000 x86:
   2 GiB address space) where the previous slurp-and-copy design needed
   >= 2x the file size per decoder and OOM aborts were a real risk under
   concurrent ReplayGain scans.
*/

/// One composite block is a few KiB; anything beyond this is container
/// corruption.
pub const MAX_FRAME_BYTES: u64 = 16 * 1024 * 1024;
/// 50 blocks/s: 5M entries cover a 27 h file. Beyond this the file is
/// treated as corrupt (a hostile file could otherwise grow the index
/// without bound).
pub const MAX_INDEX_FRAMES: usize = 5_000_000;
/// Master-element payloads (Tracks/Tags) we read fully are small in practice.
const MAX_MASTER_PAYLOAD: u64 = 16 * 1024 * 1024;
/// Window size for the sequential header walk; a refill starts at the next
/// requested header, so skipped payloads are never read at all.
const INDEX_WINDOW: usize = 256 * 1024;

/// Absolute byte range of one composite block payload in the file, plus the
/// ES metadata the seek path needs without reading the full payload.
#[derive(Debug, Clone, Copy)]
pub struct BlockRef {
    pub t_ns: u64,
    pub offset: u64,
    pub len: u32,
    /// Absolute LF AU index of the first AU in the block (None when the
    /// block carries no LF section).
    pub au_index: Option<u32>,
    /// Number of LF AUs in the block (0..=4).
    pub lf_count: u8,
    /// First payload byte of each LF AU (the USAC independency flag lives
    /// in bit 7); the first `lf_count` entries are valid.
    pub lf_first: [u8; 8],
    /// xHE mid (v3): absolute AU index of the first mid AU in the block.
    pub mx_au_index: Option<u32>,
    /// Number of mid AUs in the block (0..=8).
    pub mx_count: u8,
    /// First payload byte of each mid AU (independency flags).
    pub mx_first: [u8; 8],
    /// xHE top band (v4): absolute AU index of the first top AU.
    pub hf_au_index: Option<u32>,
    /// Number of top-band AUs in the block (0..=2 at 44.1 kHz).
    pub hf_count: u8,
    /// First payload byte of each top-band AU (independency flags).
    pub hf_first: [u8; 8],
}

/// Container metadata + block index produced by [`index_container`].
#[derive(Debug)]
pub struct IndexedFile {
    /// Validated SENA-INIT blob.
    pub init: es::Init,
    /// Declared nAvgBytesPerSec of the WAVEFORMATEX wrapper.
    pub avg_bytes_per_sec: u32,
    /// All top-level Tags blocks in file order.
    pub tags: Vec<Vec<(String, String)>>,
    /// Absolute byte ranges (element start..end) of the Tags blocks.
    pub tag_ranges: Vec<(u64, u64)>,
    /// Absolute ranges of Attachments elements: (element start, payload
    /// start, element end).
    pub attachment_ranges: Vec<(u64, u64, u64)>,
    pub blocks: Vec<BlockRef>,
    pub warnings: Vec<String>,
    /// Encoded size of all Cluster elements (codec payloads plus block
    /// framing); the audio-only size an average-bitrate display should use.
    pub audio_span_bytes: u64,
    pub segment_payload_start: u64,
    /// Absolute offset one past the Segment payload (= file end in practice).
    pub segment_payload_end: u64,
    /// Absolute offset one past the Tracks element (the insertion point
    /// for head-positioned Tags/Attachments).
    pub tracks_end: u64,
    /// Absolute offset of the Segment size vint.
    pub segment_size_pos: u64,
    pub segment_size_len: usize,
}

struct Window<'a, R: FnMut(u64, usize) -> Result<Vec<u8>, String>> {
    read_at: &'a mut R,
    base: u64,
    buf: Vec<u8>,
}

impl<R: FnMut(u64, usize) -> Result<Vec<u8>, String>> Window<'_, R> {
    /// Read `len` bytes at absolute `pos`. Small reads come from a sliding
    /// window; a refill starts at `pos`, so skipped payload bytes between
    /// headers are never read. Reads larger than the window go direct.
    fn get(&mut self, pos: u64, len: usize) -> Result<Vec<u8>, String> {
        if len > INDEX_WINDOW {
            return (self.read_at)(pos, len);
        }
        let end = pos
            .checked_add(len as u64)
            .ok_or("element range overflow")?;
        if pos < self.base || end > self.base + self.buf.len() as u64 {
            self.buf = (self.read_at)(pos, INDEX_WINDOW)?;
            self.base = pos;
            if end > self.base + self.buf.len() as u64 {
                return Err("truncated EBML: unexpected end of file".into());
            }
        }
        let off = (pos - self.base) as usize;
        Ok(self.buf[off..off + len].to_vec())
    }
}

struct RawElem {
    id: Vec<u8>,
    size: u64,
    size_len: usize,
    start: u64,
    data_start: u64,
    data_end: u64,
}

fn vint_width(first: u8) -> Option<usize> {
    if first == 0 {
        None
    } else {
        Some(1 + first.leading_zeros() as usize)
    }
}

fn read_elem_w<R: FnMut(u64, usize) -> Result<Vec<u8>, String>>(
    w: &mut Window<R>,
    pos: u64,
) -> Result<RawElem, String> {
    let first = *w.get(pos, 1)?.first().ok_or("truncated EBML: element id")?;
    let id_len = vint_width(first).ok_or("malformed container: invalid element id")?;
    let id = w.get(pos, id_len)?;
    let size_first = *w
        .get(pos + id_len as u64, 1)?
        .first()
        .ok_or("truncated EBML: element size")?;
    let size_len = vint_width(size_first).ok_or("malformed container: invalid vint")?;
    let size_bytes = w.get(pos + id_len as u64, size_len)?;
    let (size, _) = read_vint(&size_bytes, 0).map_err(|e| e.to_string())?;
    let data_start = pos
        .checked_add((id_len + size_len) as u64)
        .ok_or("element size overflow")?;
    let data_end = data_start
        .checked_add(size)
        .ok_or("element size overflow")?;
    Ok(RawElem {
        id,
        size,
        size_len,
        start: pos,
        data_start,
        data_end,
    })
}

/// ES block header metadata for the index: read the composite block header
/// just far enough to locate the LF section, without copying payload bodies.
/// `get` reads at absolute file offsets. Layout: flags(1) mid_len(2) mid,
/// lf_count(1) au_index(3), then per AU: len(3) + payload. xHE mid (v3):
/// flags(1) count(1) au_index(3) [len(3) + payload]*, same as the LF form.
#[allow(clippy::too_many_arguments)]
fn es_block_meta<R: FnMut(u64, usize) -> Result<Vec<u8>, String>>(
    get: &mut R,
    offset: u64,
    len: u64,
    xhe_mid: bool,
    xhe_hf: bool,
) -> Result<BlockMeta, String> {
    let head = get(offset, len.min(3) as usize)?;
    if head.len() < 3 {
        return Err("malformed container: composite block header truncated".into());
    }
    let (lf_off, avail, mx_au_index, mx_count, mx_first) = if xhe_mid {
        // mid section: flags(1) then count(1) au_index(3) [len(3) + payload]*
        let (mx_au_index, mx_count, mx_first, rel) =
            au_list_meta(get, offset + 1, len - 1, 8, "mid")?;
        (offset + 1 + rel, len - 1 - rel, mx_au_index, mx_count, mx_first)
    } else {
        let mid_len = u16::from_le_bytes(head[1..3].try_into().unwrap()) as u64;
        if 3 + mid_len + 1 > len {
            return Err("malformed container: mid section overruns its block".into());
        }
        (offset + 3 + mid_len, len - 3 - mid_len, None, 0, [0; 8])
    };
    let head1 = get(lf_off, avail.min(1) as usize)?;
    if head1.is_empty() {
        return Err("malformed container: LF count truncated".into());
    }
    let lf_count = head1[0];
    let (au_index, _, first, lf_rel) = if lf_count == 0 {
        (None, 0, [0; 8], 1u64)
    } else {
        let (ai, c, f, rel) = au_list_meta(get, lf_off, avail, 4, "LF")?;
        (ai, c, f, rel)
    };
    // v4 xHE top band: count(1) au_index(3) [len(2) + payload]*, after the
    // LF section. Only the AU index and the independency flags are needed.
    let (hf_au_index, hf_count, hf_first) = if xhe_hf {
        let hf_off = lf_off + lf_rel;
        let hf_avail = len.saturating_sub(hf_off - offset);
        let head2 = get(hf_off, hf_avail.min(1) as usize)?;
        if head2.is_empty() {
            return Err("malformed container: top-band count truncated".into());
        }
        let count = head2[0];
        if count == 0 {
            (None, 0, [0; 8])
        } else {
            if count > 8 {
                // Same bound as the xHE mid: the EOS clamp packs the
                // encoder's tail-coverage AUs into the final block.
                return Err("malformed container: top-band AU count exceeds sanity bound".into());
            }
            let idx = get(hf_off + 1, 3)?;
            if idx.len() < 3 {
                return Err("malformed container: top-band au_index truncated".into());
            }
            let mut i3 = [0u8; 4];
            i3[..3].copy_from_slice(&idx);
            let hf_au_index = u32::from_le_bytes(i3);
            let mut first = [0u8; 8];
            let mut rel = 4u64;
            for i in 0..count as usize {
                let h = get(hf_off + rel, 3u64.min(hf_avail.saturating_sub(rel)) as usize)?;
                if h.len() < 3 {
                    return Err("malformed container: top-band AU header truncated".into());
                }
                let au_len = u16::from_le_bytes(h[..2].try_into().unwrap()) as u64;
                first[i] = h[2];
                rel += 2 + au_len;
                if rel > hf_avail {
                    return Err("malformed container: top-band AU overruns its block".into());
                }
            }
            (Some(hf_au_index), count, first)
        }
    } else {
        (None, 0, [0; 8])
    };
    Ok(BlockMeta {
        lf_au_index: au_index,
        lf_count,
        lf_first: first,
        mx_au_index,
        mx_count,
        mx_first,
        hf_au_index,
        hf_count,
        hf_first,
    })
}

/// Index metadata of one block (see BlockRef).
struct BlockMeta {
    lf_au_index: Option<u32>,
    lf_count: u8,
    lf_first: [u8; 8],
    mx_au_index: Option<u32>,
    mx_count: u8,
    mx_first: [u8; 8],
    hf_au_index: Option<u32>,
    hf_count: u8,
    hf_first: [u8; 8],
}

/// Parse a counted AU list section: count(1) au_index(3) [len(3) +
/// payload]*. Returns (first au_index, count, first payload bytes (up to
/// `max_count`), section length in bytes). `max_count` is the sanity bound.
fn au_list_meta<R: FnMut(u64, usize) -> Result<Vec<u8>, String>>(
    get: &mut R,
    off: u64,
    avail: u64,
    max_count: u8,
    what: &str,
) -> Result<(Option<u32>, u8, [u8; 8], u64), String> {
    let head1 = get(off, avail.min(1) as usize)?;
    if head1.is_empty() {
        return Err(format!("malformed container: {what} count truncated"));
    }
    let count = head1[0];
    if count == 0 {
        return Ok((None, 0, [0; 8], 1));
    }
    if count > max_count {
        return Err(format!("malformed container: {what} AU count exceeds sanity bound"));
    }
    let idx = get(off + 1, 3)?;
    if idx.len() < 3 {
        return Err(format!("malformed container: {what} au_index truncated"));
    }
    let mut i3 = [0u8; 4];
    i3[..3].copy_from_slice(&idx);
    let au_index = u32::from_le_bytes(i3);
    let mut first = [0u8; 8];
    let mut rel = 4u64; // past count + au_index
    for i in 0..count as usize {
        let h = get(off + rel, 4u64.min(avail.saturating_sub(rel)) as usize)?;
        if h.len() < 4 {
            return Err(format!("malformed container: {what} AU header truncated"));
        }
        let mut l3 = [0u8; 4];
        l3[..3].copy_from_slice(&h[..3]);
        let au_len = u32::from_le_bytes(l3) as u64;
        first[i] = h[3];
        rel += 3 + au_len;
        if rel > avail {
            return Err(format!("malformed container: {what} AU overruns its block"));
        }
    }
    Ok((Some(au_index), count, first, rel))
}

fn walk_cluster_w<R: FnMut(u64, usize) -> Result<Vec<u8>, String>>(
    w: &mut Window<R>,
    cluster: &RawElem,
    blocks: &mut Vec<BlockRef>,
    xhe_mid: bool,
    xhe_hf: bool,
) -> Result<(), String> {
    let mut cluster_t = 0u64;
    let mut p = cluster.data_start;
    while p < cluster.data_end {
        let e = read_elem_w(w, p)?;
        if e.data_end > cluster.data_end {
            return Err("malformed container: block overruns its Cluster".into());
        }
        if e.id == CLUSTER_TIMESTAMP_ID {
            if e.size > 8 {
                return Err("malformed container: cluster timestamp too long".into());
            }
            let payload = w.get(e.data_start, e.size as usize)?;
            cluster_t = read_uint(&payload, "cluster timestamp").map_err(|e| e.to_string())?;
        } else if e.id == SIMPLE_BLOCK_ID {
            let plen = e.data_end - e.data_start;
            let head_len = plen.min(8) as usize;
            let head = w.get(e.data_start, head_len)?;
            let (track, rel, header_len) =
                parse_simple_block_header(&head, 0..head_len).map_err(|e| e.to_string())?;
            if track != 1 {
                return Err(format!(
                    "malformed container: SimpleBlock for track {track} in a single-track Sena file"
                ));
            }
            if (header_len as u64) > plen {
                return Err("malformed container: SimpleBlock header overruns payload".into());
            }
            let body_len = plen - header_len as u64;
            if body_len > MAX_FRAME_BYTES {
                return Err(format!(
                    "malformed container: SimpleBlock payload {body_len} bytes exceeds the {MAX_FRAME_BYTES}-byte sanity cap"
                ));
            }
            let payload_off = e.data_start + header_len as u64;
            let meta = es_block_meta(
                &mut |pos, len| w.get(pos, len),
                payload_off,
                body_len,
                xhe_mid,
                xhe_hf,
            )?;
            // Sign-extending cast (see parse_simple_block).
            let rel = rel as u64;
            let t_ns = cluster_t
                .saturating_mul(TIMECODE_SCALE_NS)
                .saturating_add(rel.saturating_mul(TIMECODE_SCALE_NS));
            blocks.push(BlockRef {
                t_ns,
                offset: payload_off,
                len: body_len as u32,
                au_index: meta.lf_au_index,
                lf_count: meta.lf_count,
                lf_first: meta.lf_first,
                mx_au_index: meta.mx_au_index,
                mx_count: meta.mx_count,
                mx_first: meta.mx_first,
                hf_au_index: meta.hf_au_index,
                hf_count: meta.hf_count,
                hf_first: meta.hf_first,
            });
            if blocks.len() > MAX_INDEX_FRAMES {
                return Err(format!(
                    "malformed container: more than {MAX_INDEX_FRAMES} blocks (unrealistic)"
                ));
            }
        }
        p = e.data_end;
    }
    Ok(())
}

/// Scan a Sena file through a positioned-read callback, keeping memory
/// bounded: head elements (Tracks/Tags) are parsed, cluster payloads are
/// skipped. With `want_blocks` the cluster SimpleBlocks are indexed (by
/// offset, length and ES header metadata) for lazy decode; without it
/// cluster contents are skipped entirely (probe/tag-edit paths).
pub fn index_container<R: FnMut(u64, usize) -> Result<Vec<u8>, String>>(
    read_at: &mut R,
    want_blocks: bool,
) -> Result<IndexedFile, String> {
    let mut w = Window {
        read_at,
        base: 0,
        buf: Vec::new(),
    };
    let magic = w.get(0, 4)?;
    if magic != b"\x1A\x45\xDF\xA3" {
        return Err(DemuxError::Malformed("missing EBML header".into()).to_string());
    }
    let mut pos = 0u64;
    let segment = loop {
        let e = read_elem_w(&mut w, pos)?;
        if e.id == SEGMENT_ID {
            break e;
        }
        pos = e.data_end;
    };
    if segment.size == (1u64 << (7 * segment.size_len)) - 1 {
        return Err(DemuxError::Unsupported("unknown-size Segment".into()).to_string());
    }

    let mut tracks = Vec::new();
    let mut tags = Vec::new();
    let mut tag_ranges = Vec::new();
    let mut attachment_ranges = Vec::new();
    let mut blocks = Vec::new();
    let mut tracks_end = 0u64;
    let mut audio_span_bytes = 0u64;
    // xHE mid (ES v3) / xHE top (ES v4) flip the block header layout;
    // known once Tracks has been parsed (Tracks precede Clusters in our
    // writer).
    let mut xhe_mid = false;
    let mut xhe_hf = false;
    let mut p = segment.data_start;
    while p < segment.data_end {
        let e = read_elem_w(&mut w, p)?;
        if e.data_end > segment.data_end {
            return Err("malformed container: top-level element overruns the Segment".into());
        }
        if e.id == TRACKS_ID {
            if e.size > MAX_MASTER_PAYLOAD {
                return Err("malformed container: Tracks element exceeds the sanity cap".into());
            }
            tracks_end = e.data_end;
            let payload = w.get(e.data_start, e.size as usize)?;
            let local = Elem {
                start: 0,
                id_len: e.id.len(),
                size_len: e.size_len,
                data_start: 0,
                data_end: payload.len(),
            };
            for_each_child(&payload, local, |tid, tentry| {
                if tid == TRACK_ENTRY_ID {
                    tracks.push(parse_track(&payload, tentry)?);
                }
                Ok(())
            })
            .map_err(|e| e.to_string())?;
        } else if e.id == TAGS_ID {
            if e.size > MAX_MASTER_PAYLOAD {
                return Err("malformed container: Tags element exceeds the sanity cap".into());
            }
            let payload = w.get(e.data_start, e.size as usize)?;
            let local = Elem {
                start: 0,
                id_len: e.id.len(),
                size_len: e.size_len,
                data_start: 0,
                data_end: payload.len(),
            };
            let entries = parse_tags_payload(&payload, local).map_err(|e| e.to_string())?;
            tags.push(entries);
            tag_ranges.push((e.start, e.data_end));
        } else if e.id == CLUSTER_ID {
            audio_span_bytes += e.data_end - e.start;
            if want_blocks {
                if !tracks.is_empty() && !xhe_mid {
                    let (init, _, _) = sena_track_init(&tracks).map_err(|e| e.to_string())?;
                    xhe_mid = init.mid_codec == sena_core::MidCodec::Xhe;
                    xhe_hf = init.hf_codec == sena_core::HfCodec::Xhe;
                }
                walk_cluster_w(&mut w, &e, &mut blocks, xhe_mid, xhe_hf)?;
            }
        } else if e.id == ATTACHMENTS_ID {
            attachment_ranges.push((e.start, e.data_start, e.data_end));
        }
        p = e.data_end;
    }

    let (init, avg_bytes_per_sec, _track_number) = sena_track_init(&tracks).map_err(|e| e.to_string())?;
    Ok(IndexedFile {
        init,
        avg_bytes_per_sec,
        tags,
        tag_ranges,
        attachment_ranges,
        blocks,
        warnings: Vec::new(),
        audio_span_bytes,
        segment_payload_start: segment.data_start,
        segment_payload_end: segment.data_end,
        tracks_end,
        segment_size_pos: segment.start + segment.id.len() as u64,
        segment_size_len: segment.size_len,
    })
}

/// Reads `len` bytes at `pos` from an in-memory buffer (the `index_container`
/// backend for whole-file callers); short only at the buffer end.
pub fn mem_read_at(bytes: &[u8], pos: u64, len: usize) -> Result<Vec<u8>, String> {
    let start = usize::try_from(pos).map_err(|_| "file too large for this platform".to_string())?;
    let end = start
        .checked_add(len)
        .unwrap_or(bytes.len())
        .min(bytes.len());
    bytes
        .get(start..end)
        .map(|s| s.to_vec())
        .ok_or_else(|| "range outside buffer".to_string())
}

/* --------------------------------------------------------- rewrite planning
   A tag/attachment rewrite is fully described by a handful of positioned
   writes: void the old elements in place, append the replacement at the
   Segment tail, re-patch the Segment size. Computing the plan from an
   IndexedFile avoids materializing a rewritten copy of the whole file.
*/

pub enum WriteOp {
    /// Overwrite `data.len()` bytes at `offset` (in-place Void header and
    /// the Segment size patch).
    Bytes { offset: u64, data: Vec<u8> },
    /// Insert `data` at `offset`, growing the file (the Segment tail
    /// append). For callback IO this is only valid at EOF - the caller
    /// guarantees the Segment ends exactly at the file end.
    Insert { offset: u64, data: Vec<u8> },
    /// Write `len` zero bytes at `offset` (the body of an in-place Void).
    Zero { offset: u64, len: u64 },
}

/// EBML Void header (0xEC + size vint) for `total_len` bytes, header only;
/// the remaining payload is zeros (see [`WriteOp::Zero`]).
pub(crate) fn void_header(total_len: u64) -> Result<Vec<u8>, String> {
    for size_len in 1..=8u64 {
        if total_len < 1 + size_len {
            continue;
        }
        let payload = total_len - 1 - size_len;
        if sena_mux::ebml::vint_size(payload) == size_len as usize {
            let mut out = Vec::with_capacity(1 + size_len as usize);
            out.push(0xEC);
            out.extend_from_slice(&encode_vint_exact(payload, size_len as usize)?);
            return Ok(out);
        }
    }
    Err(format!("cannot encode {total_len}-byte Void element"))
}

pub(crate) fn encode_vint_exact(value: u64, len: usize) -> Result<Vec<u8>, String> {
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

/// Push the ops voiding one element in place (Void header + zeroed payload).
pub(crate) fn push_void_ops(ops: &mut Vec<WriteOp>, start: u64, end: u64) -> Result<(), String> {
    let header = void_header(end - start)?;
    let header_len = header.len() as u64;
    ops.push(WriteOp::Bytes {
        offset: start,
        data: header,
    });
    if end - start > header_len {
        ops.push(WriteOp::Zero {
            offset: start + header_len,
            len: end - start - header_len,
        });
    }
    Ok(())
}

/// Apply a rewrite plan to an in-memory file buffer.
pub fn apply_write_ops_to_vec(out: &mut Vec<u8>, ops: &[WriteOp]) {
    for op in ops {
        match op {
            WriteOp::Bytes { offset, data } => {
                let start = *offset as usize;
                out.splice(start..start + data.len(), data.iter().copied());
            }
            WriteOp::Insert { offset, data } => {
                out.splice(*offset as usize..*offset as usize, data.iter().copied());
            }
            WriteOp::Zero { offset, len } => {
                let start = *offset as usize;
                out.splice(
                    start..start + *len as usize,
                    std::iter::repeat_n(0, *len as usize),
                );
            }
        }
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

    #[test]
    fn index_matches_full_parse() {
        let path = concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../assets/e2e/01__p300__lfa__hf144.sena"
        );
        let bytes = std::fs::read(path).unwrap();
        let demux = Demuxed::parse(bytes.clone()).unwrap();
        let indexed = index_container(&mut |pos, len| mem_read_at(&bytes, pos, len), true).unwrap();

        assert_eq!(indexed.init, demux.init);
        assert_eq!(indexed.avg_bytes_per_sec, demux.avg_bytes_per_sec);
        assert_eq!(
            indexed.segment_payload_end,
            demux.segment.elem.data_end as u64
        );
        assert_eq!(indexed.warnings, demux.warnings);

        // Every indexed block points at the same payload bytes the full
        // parse copied, in the same order.
        assert_eq!(indexed.blocks.len(), demux.blocks.len());
        for (br, b) in indexed.blocks.iter().zip(demux.blocks.iter()) {
            assert_eq!(br.t_ns, b.t_ns);
            let start = br.offset as usize;
            assert_eq!(&bytes[start..start + br.len as usize], b.data.as_slice());
            // The ES header metadata matches the parsed block.
            let parsed = es::decode_block(&b.data, sena_core::MidCodec::Opus, sena_core::HfCodec::Opus).unwrap();
            match (br.au_index, parsed.lf_au_index) {
                (Some(ai), Some(p_ai)) => {
                    assert_eq!(ai, p_ai);
                    assert_eq!(br.lf_count as usize, parsed.lf.len());
                    for (i, au) in parsed.lf.iter().enumerate() {
                        assert_eq!(br.lf_first[i], au.first().copied().unwrap_or(0));
                    }
                }
                (None, None) => assert_eq!(br.lf_count, 0),
                other => panic!("au_index mismatch: {other:?}"),
            }
        }
    }

    #[test]
    fn v1_file_rejected() {
        // A v1 layout (A_OPUS + A_SENALF track ids, no A_MS/ACM track) must
        // fail with a clear unsupported error.
        let mut tracks = Vec::new();
        for (i, id) in ["A_OPUS", "A_SENALF"].iter().enumerate() {
            let mut te = vec![];
            // reuse the mux crate's EBML writer for a minimal Tracks element
            sena_mux::ebml::uint(&mut te, &[0xD7], (i + 1) as u64);
            sena_mux::ebml::uint(&mut te, &[0x83], 2);
            sena_mux::ebml::str(&mut te, &[0x86], id);
            let mut audio = vec![];
            sena_mux::ebml::float(&mut audio, &[0xB5], 48000.0);
            sena_mux::ebml::uint(&mut audio, &[0x9F], 2);
            sena_mux::ebml::write_element(&mut te, &[0xE1], &audio);
            sena_mux::ebml::write_element(&mut tracks, &[0xAE], &te);
        }
        let mut file = vec![];
        file.extend_from_slice(b"\x1A\x45\xDF\xA3");
        // minimal: not a full EBML header, but Demuxed::parse only needs the
        // magic and a Segment; build them.
        let mut file2 = vec![];
        sena_mux::ebml::str(&mut file2, &[0x42, 0x82], "matroska");
        let mut hdr = vec![];
        hdr.extend_from_slice(&file2);
        let mut out = vec![];
        sena_mux::ebml::write_element(&mut out, &[0x1A, 0x45, 0xDF, 0xA3], &hdr);
        let mut seg = vec![];
        let mut tracks_el = vec![];
        tracks_el.extend_from_slice(&tracks);
        sena_mux::ebml::write_element(&mut seg, &[0x16, 0x54, 0xAE, 0x6B], &tracks_el);
        sena_mux::ebml::write_element(&mut out, &[0x18, 0x53, 0x80, 0x67], &seg);
        let _ = file;
        match Demuxed::parse(out) {
            Err(DemuxError::Unsupported(e)) => assert!(e.contains("v1"), "{e}"),
            other => panic!("expected v1 rejection, got {other:?}"),
        }
    }
}
