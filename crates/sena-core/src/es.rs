//! Sena composite elementary stream (ES) framing, format v4.
//!
//! One Sena track in Matroska is a single `A_MS/ACM` track whose
//! CodecPrivate is a WAVEFORMATEX wrapper around the SENA-INIT blob, and
//! whose SimpleBlocks are composite 20 ms blocks carrying the layer
//! payloads (mid Opus, LF xHE-AAC AU(s), optional top-band Opus).

use crate::{Profile, SAMPLE_RATE};

/// ACM format tag of the Sena track; the DirectShow media subtype is
/// FOURCCMap(WAVE_FORMAT_SENA) = {0000534E-0000-0010-8000-00AA00389B71}.
pub const WAVE_FORMAT_SENA: u16 = 0x534E;
pub const SENA_CODEC_ID: &str = "A_MS/ACM";

/// Display/codec name of the format: `Sena xAAC-Opus@<lf crossover>` for the
/// two-layer layout, `Sena xAAC-Opus@<lf crossover>-Opus@12000` when the
/// top-band layer (three-layer layout) rides along; xHE-AAC mid/top layers
/// read `xAAC` ("Sena xAAC-xAAC@600-xAAC@12000" for the all-FhG 288k
/// variant).
pub fn codec_name(
    profile: crate::Profile,
    three_track: bool,
    mid: crate::MidCodec,
    hf: crate::HfCodec,
) -> String {
    let lf = profile.crossover_hz() as u32;
    let mid_name = match mid {
        crate::MidCodec::Opus => "Opus",
        crate::MidCodec::Xhe => "xAAC",
    };
    if three_track {
        let hf_name = match hf {
            crate::HfCodec::Opus => "Opus",
            crate::HfCodec::Xhe => "xAAC",
        };
        let split = crate::HF_SPLIT_HZ as u32;
        format!("Sena xAAC-{mid_name}@{lf}-{hf_name}@{split}")
    } else {
        format!("Sena xAAC-{mid_name}@{lf}")
    }
}
pub const ES_MAGIC: [u8; 4] = *b"SEN2";
/// Current ES version: v4 adds the hf-codec byte to SENA-INIT and the
/// counted-AU top-band block section used by an xHE-AAC top band (the xfi
/// 288k variant, whose decoded output rate is 44.1 kHz). v3 added the
/// mid-codec byte + counted-AU mid section. v2/v3 files (Opus mid/top
/// implied) remain decodable.
pub const ES_VERSION: u16 = 4;
pub const ES_VERSION_MIN: u16 = 2;
pub const CHANNELS: u16 = 2;
pub const BITS_PER_SAMPLE: u16 = 32;

/// Composite block period: 20 ms = 960 frames at 48 kHz.
pub const BLOCK_NS: u64 = 20_000_000;
pub const BLOCK_FRAMES: u64 = 960;

/// Informational SeekPreRoll written on the track (foreign demuxers only;
/// the Sena seek path uses block-source warmup instead).
pub const SEEK_PREROLL_NS: u64 = 500_000_000;

/// SHA-256 domain separator for the v3 content hash (SENA-INIT + blocks).
pub const SHA256_DOMAIN_V3: &[u8] = b"SENA encoded audio sha256 v3\0";
/// SHA-256 domain separator for the v4 content hash (the v4 INIT layout
/// differs, so the domain separates too).
pub const SHA256_DOMAIN_V4: &[u8] = b"SENA encoded audio sha256 v4\0";

pub const FLAG_RESERVED: u8 = 0; // no flag bits are defined in v2

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EsError {
    BadMagic,
    UnsupportedVersion(u16),
    BadProfile(u8),
    BadMidCodec,
    BadHfCodec,
    LfRateMismatch,
    HeadMismatch,
    Truncated,
    Overlong,
    BadWaveFormat,
    BadBlock(&'static str),
}

impl core::fmt::Display for EsError {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        match self {
            EsError::BadMagic => write!(f, "bad SENA-INIT magic"),
            EsError::UnsupportedVersion(v) => write!(f, "unsupported ES version {v}"),
            EsError::BadProfile(p) => write!(f, "bad profile {p}"),
            EsError::BadMidCodec => write!(f, "bad mid codec"),
            EsError::BadHfCodec => write!(f, "bad hf codec"),
            EsError::LfRateMismatch => write!(f, "lf_rate does not match profile"),
            EsError::HeadMismatch => write!(f, "three-track flag/head mismatch"),
            EsError::Truncated => write!(f, "truncated data"),
            EsError::Overlong => write!(f, "trailing bytes"),
            EsError::BadWaveFormat => write!(f, "bad WAVEFORMATEX wrapper"),
            EsError::BadBlock(what) => write!(f, "bad composite block: {what}"),
        }
    }
}

/// SENA-INIT: the immutable per-file parameters (replaces the v1
/// SENA_PROFILE / SENA_VERSION / SENA_PLAYABLE_SAMPLES / SENA_AUDIO_SHA256
/// tags).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Init {
    pub profile: Profile,
    pub three_track: bool,
    /// Mid-layer codec (v3+; v2 files imply Opus).
    pub mid_codec: crate::MidCodec,
    /// Top-band codec (v4+; earlier files imply Opus). Meaningful only
    /// when `three_track` is set.
    pub hf_codec: crate::HfCodec,
    pub lf_rate: u32,
    pub playable_samples: u64,
    pub audio_sha256: [u8; 32],
    /// LF xHE-AAC AudioSpecificConfig.
    pub lf_asc: Vec<u8>,
    /// Mid-layer head: OpusHead (Opus mid) or xHE-AAC AudioSpecificConfig
    /// (xHE mid).
    pub mid_head: Vec<u8>,
    /// Top-layer head: OpusHead (Opus top) or xHE-AAC AudioSpecificConfig
    /// (xHE top) — three-track only.
    pub hf_head: Option<Vec<u8>>,
}

impl Init {
    /// Decoded output rate of this stream: 44.1 kHz when every band is
    /// xHE-AAC at 44.1 kHz (the xfi 288k variant — no lane exceeds that
    /// rate, so no upsampling), 48 kHz otherwise.
    pub fn output_rate(&self) -> u32 {
        if self.mid_codec == crate::MidCodec::Xhe
            && self.three_track
            && self.hf_codec == crate::HfCodec::Xhe
        {
            crate::XFI_MID_RATE
        } else {
            SAMPLE_RATE
        }
    }
}

impl Init {
    pub fn encode(&self) -> Vec<u8> {
        let mut b = Vec::new();
        b.extend_from_slice(&ES_MAGIC);
        b.extend_from_slice(&ES_VERSION.to_le_bytes());
        b.push(self.profile.to_byte());
        b.push(if self.three_track { 1 } else { 0 });
        b.push(self.mid_codec.to_byte());
        b.push(self.hf_codec.to_byte());
        b.extend_from_slice(&self.lf_rate.to_le_bytes());
        b.extend_from_slice(&self.playable_samples.to_le_bytes());
        b.extend_from_slice(&self.audio_sha256);
        push16(&mut b, &self.lf_asc);
        push16(&mut b, &self.mid_head);
        if let Some(hf) = &self.hf_head {
            push16(&mut b, hf);
        }
        b
    }

    /// Encode with the hash field zeroed (the hashed form; see SHA256 v3).
    pub fn encode_zeroed_hash(&self) -> Vec<u8> {
        let mut c = self.clone();
        c.audio_sha256 = [0; 32];
        c.encode()
    }

    pub fn decode(mut b: &[u8]) -> Result<Init, EsError> {
        let m = take(&mut b, 4)?;
        if m != ES_MAGIC {
            return Err(EsError::BadMagic);
        }
        let version = u16::from_le_bytes(take(&mut b, 2)?.try_into().unwrap());
        if !(ES_VERSION_MIN..=ES_VERSION).contains(&version) {
            return Err(EsError::UnsupportedVersion(version));
        }
        let pb = take(&mut b, 1)?[0];
        let profile = Profile::from_byte(pb).ok_or(EsError::BadProfile(pb))?;
        let three_track = take(&mut b, 1)?[0] & 1 != 0;
        let mid_codec = if version >= 3 {
            crate::MidCodec::from_byte(take(&mut b, 1)?[0]).ok_or(EsError::BadMidCodec)?
        } else {
            crate::MidCodec::Opus
        };
        let hf_codec = if version >= 4 {
            crate::HfCodec::from_byte(take(&mut b, 1)?[0]).ok_or(EsError::BadHfCodec)?
        } else {
            crate::HfCodec::Opus
        };
        let lf_rate = u32::from_le_bytes(take(&mut b, 4)?.try_into().unwrap());
        if lf_rate != profile.lf_rate() {
            return Err(EsError::LfRateMismatch);
        }
        let playable_samples = u64::from_le_bytes(take(&mut b, 8)?.try_into().unwrap());
        let audio_sha256: [u8; 32] = take(&mut b, 32)?.try_into().unwrap();
        let lf_asc = take16(&mut b)?.to_vec();
        let mid_head = take16(&mut b)?.to_vec();
        let hf_head = if three_track {
            Some(take16(&mut b)?.to_vec())
        } else {
            None
        };
        if !b.is_empty() {
            return Err(EsError::Overlong);
        }
        if three_track != hf_head.is_some() {
            return Err(EsError::HeadMismatch);
        }
        Ok(Init {
            profile,
            three_track,
            mid_codec,
            hf_codec,
            lf_rate,
            playable_samples,
            audio_sha256,
            lf_asc,
            mid_head,
            hf_head,
        })
    }
}

fn push16(b: &mut Vec<u8>, data: &[u8]) {
    debug_assert!(
        data.len() <= u16::MAX as usize,
        "section of {} bytes does not fit the 16-bit wire field",
        data.len()
    );
    b.extend_from_slice(&(data.len() as u16).to_le_bytes());
    b.extend_from_slice(data);
}

/// 24-bit wire field (AU indices and AU payload lengths in a block).
fn push24(b: &mut Vec<u8>, v: u32) {
    debug_assert!(v < (1 << 24), "value {v} does not fit the 24-bit wire field");
    b.extend_from_slice(&v.to_le_bytes()[..3]);
}

fn take<'a>(b: &mut &'a [u8], n: usize) -> Result<&'a [u8], EsError> {
    if b.len() < n {
        return Err(EsError::Truncated);
    }
    let (h, t) = b.split_at(n);
    *b = t;
    Ok(h)
}

fn take16<'a>(b: &mut &'a [u8]) -> Result<&'a [u8], EsError> {
    let n = u16::from_le_bytes(take(b, 2)?.try_into().unwrap()) as usize;
    take(b, n)
}

/// 24-bit wire field read (see [`push24`]).
fn take24(b: &mut &[u8]) -> Result<u32, EsError> {
    let mut i3 = [0u8; 4];
    i3[..3].copy_from_slice(take(b, 3)?);
    Ok(u32::from_le_bytes(i3))
}

/// Build the CodecPrivate: WAVEFORMATEX (18 bytes) + SENA-INIT blob.
pub fn build_codec_private(init: &Init, avg_bytes_per_sec: u32) -> Vec<u8> {
    let blob = init.encode();
    let mut w = Vec::with_capacity(WAVEFORMATEX_LEN + blob.len());
    w.extend_from_slice(&WAVE_FORMAT_SENA.to_le_bytes());
    w.extend_from_slice(&CHANNELS.to_le_bytes());
    w.extend_from_slice(&init.output_rate().to_le_bytes());
    w.extend_from_slice(&avg_bytes_per_sec.to_le_bytes());
    w.extend_from_slice(&WAVE_BLOCK_ALIGN.to_le_bytes());
    w.extend_from_slice(&BITS_PER_SAMPLE.to_le_bytes());
    w.extend_from_slice(&(blob.len() as u16).to_le_bytes());
    w.extend_from_slice(&blob);
    w
}

/// WAVEFORMATEX header length prepended to the INIT blob in CodecPrivate.
const WAVEFORMATEX_LEN: usize = 18;

/// nBlockAlign the encoder writes (and the parser requires): the Sena
/// track's blocks are not PCM-aligned.
const WAVE_BLOCK_ALIGN: u16 = 1;

/// Parse a CodecPrivate WAVEFORMATEX wrapper; returns the INIT blob and the
/// declared nAvgBytesPerSec.
pub fn parse_codec_private(w: &[u8]) -> Result<(Init, u32), EsError> {
    if w.len() < WAVEFORMATEX_LEN {
        return Err(EsError::Truncated);
    }
    let tag = u16::from_le_bytes(w[0..2].try_into().unwrap());
    let channels = u16::from_le_bytes(w[2..4].try_into().unwrap());
    let rate = u32::from_le_bytes(w[4..8].try_into().unwrap());
    let avg = u32::from_le_bytes(w[8..12].try_into().unwrap());
    let block_align = u16::from_le_bytes(w[12..14].try_into().unwrap());
    let bits = u16::from_le_bytes(w[14..16].try_into().unwrap());
    let cb_size = u16::from_le_bytes(w[16..18].try_into().unwrap()) as usize;
    if tag != WAVE_FORMAT_SENA
        || channels != CHANNELS
        || block_align != WAVE_BLOCK_ALIGN
        || bits != BITS_PER_SAMPLE
    {
        return Err(EsError::BadWaveFormat);
    }
    if w.len() - WAVEFORMATEX_LEN != cb_size {
        return Err(EsError::BadWaveFormat);
    }
    let init = Init::decode(&w[WAVEFORMATEX_LEN..])?;
    if rate != init.output_rate() {
        return Err(EsError::BadWaveFormat);
    }
    Ok((init, avg))
}

/// 48 kHz frames covered by one LF AU (1024 samples at lf_rate).
pub fn lf_au_frames_48k(lf_rate: u32) -> u64 {
    1024 * SAMPLE_RATE as u64 / lf_rate as u64
}

/// 48 kHz frame index of the end of xHE-AAC AU `k`'s playable coverage
/// (exact rational floor): AU k (k >= 1) covers playable [(k-1)*1024,
/// k*1024) samples at `rate`. Unlike [`lf_au_frames_48k`] this is exact at
/// any rate, including 44100 (which does not divide 48 kHz).
pub fn xhe_au_end48(k: u64, rate: u32) -> u64 {
    xhe_au_end_out(k, rate, SAMPLE_RATE)
}

/// Output-frame index of the end of xHE-AAC AU `k`'s playable coverage on
/// a stream whose output rate is `out_rate` (the 44.1 kHz-output variant
/// of [`xhe_au_end48`]).
pub fn xhe_au_end_out(k: u64, rate: u32, out_rate: u32) -> u64 {
    k * 1024 * out_rate as u64 / rate as u64
}

/// Output frames per composite 20 ms block at output rate `out_rate`
/// (960 at 48 kHz, 882 at 44.1 kHz — both exact).
pub fn block_frames_out(out_rate: u32) -> u64 {
    BLOCK_FRAMES * out_rate as u64 / SAMPLE_RATE as u64
}

/// Composite block index carrying xHE-AAC AU `k` at any stream rate: by-end
/// placement (the block containing the AU's last playable sample), AU 0
/// (warmup) in block 0, clamped to the final block.
pub fn xhe_block(k: u64, rate: u32, n_blocks: u64) -> u64 {
    let b = if k == 0 {
        0
    } else {
        (xhe_au_end48(k, rate) - 1) / BLOCK_FRAMES
    };
    b.min(n_blocks.saturating_sub(1))
}

/// Composite block index carrying LF AU `k` on a grid of `n_blocks`: the
/// block containing the AU's last playable sample (AU 0 is warmup and maps
/// to block 0), clamped to the final block. AU k >= 1 covers playable
/// [(k-1)*1024, k*1024) samples at lf_rate; by-end placement keeps the AU
/// covering any mid-stream block boundary inside that block, so a cold
/// start there has no LF hole.
pub fn lf_block(k: u64, lf_rate: u32, n_blocks: u64) -> u64 {
    xhe_block(k, lf_rate, n_blocks)
}

/// Composite block index carrying top-layer packet `j`: by-end placement
/// puts packet j in block j exactly for any pre-skip < 960, clamped to the
/// final block (the final block MAY carry two packets when the top stream
/// has one more packet than the mid grid).
pub fn hf_block(j: u64, n_blocks: u64) -> u64 {
    j.min(n_blocks.saturating_sub(1))
}

/// Number of composite blocks: the mid grid covers pre-skip + playable.
pub fn block_count(playable_samples: u64, mid_pre_skip: u32) -> u64 {
    (playable_samples + mid_pre_skip as u64).div_ceil(BLOCK_FRAMES)
}

/// Mid-layer payload of one composite block. Opus mid: exactly one packet
/// per block (the block grid IS the mid Opus packet grid). xHE mid (v3):
/// a counted AU list with the absolute AU index of the first AU, placed by
/// the same by-end rule as the LF layer (AUs are 23.22 ms at 44.1 kHz and
/// do not align to the 20 ms block grid).
#[derive(Debug, Clone)]
pub enum MidSection<'a> {
    Opus(&'a [u8]),
    Xhe { au_index: u32, aus: Vec<&'a [u8]> },
}

/// Decoded view of one composite block.
#[derive(Debug, Clone)]
pub struct Block<'a> {
    pub mid: MidSection<'a>,
    /// Absolute AU index of the first LF AU (present iff lf is non-empty).
    pub lf_au_index: Option<u32>,
    pub lf: Vec<&'a [u8]>,
    /// Absolute AU index of the first top-band AU (xHE top only; present
    /// iff hf is non-empty).
    pub hf_au_index: Option<u32>,
    pub hf: Vec<&'a [u8]>,
}

#[derive(Debug, Clone)]
pub struct BlockParts<'a> {
    pub mid: MidSection<'a>,
    /// (au_index of the first, AUs in order); au_index is ignored when the
    /// AU list is empty.
    pub lf: (u32, &'a [&'a [u8]]),
    /// (au_index of the first, packets/AUs in order); au_index is written
    /// only for an xHE-AAC top band and ignored when the list is empty.
    pub hf: (u32, &'a [&'a [u8]]),
}

pub fn encode_block(p: &BlockParts, hf_codec: crate::HfCodec) -> Vec<u8> {
    let mut b = Vec::new();
    b.push(FLAG_RESERVED);
    match &p.mid {
        MidSection::Opus(pkt) => {
            b.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
            b.extend_from_slice(pkt);
        }
        MidSection::Xhe { au_index, aus } => {
            b.push(aus.len() as u8);
            if !aus.is_empty() {
                push24(&mut b, *au_index);
                for au in aus {
                    push24(&mut b, au.len() as u32);
                    b.extend_from_slice(au);
                }
            }
        }
    }
    let (au_index, aus) = p.lf;
    b.push(aus.len() as u8);
    if !aus.is_empty() {
        push24(&mut b, au_index);
        for au in aus {
            push24(&mut b, au.len() as u32);
            b.extend_from_slice(au);
        }
    }
    let (hf_au_index, hf) = p.hf;
    b.push(hf.len() as u8);
    if hf_codec == crate::HfCodec::Xhe && !hf.is_empty() {
        push24(&mut b, hf_au_index);
    }
    for pkt in hf {
        b.extend_from_slice(&(pkt.len() as u16).to_le_bytes());
        b.extend_from_slice(pkt);
    }
    b
}

pub fn decode_block(
    mut b: &[u8],
    mid_codec: crate::MidCodec,
    hf_codec: crate::HfCodec,
) -> Result<Block<'_>, EsError> {
    let flags = take(&mut b, 1)?[0];
    if flags != 0 {
        return Err(EsError::BadBlock("unknown flag bits"));
    }
    let mid = match mid_codec {
        crate::MidCodec::Opus => {
            let mid_len = u16::from_le_bytes(take(&mut b, 2)?.try_into().unwrap()) as usize;
            let pkt = take(&mut b, mid_len)?;
            if pkt.is_empty() {
                return Err(EsError::BadBlock("empty mid frame"));
            }
            MidSection::Opus(pkt)
        }
        crate::MidCodec::Xhe => {
            let count = take(&mut b, 1)?[0] as usize;
            let au_index = if count > 0 { take24(&mut b)? } else { 0 };
            let mut aus = Vec::with_capacity(count);
            for _ in 0..count {
                let n = take24(&mut b)? as usize;
                aus.push(take(&mut b, n)?);
            }
            MidSection::Xhe { au_index, aus }
        }
    };
    let lf_count = take(&mut b, 1)?[0] as usize;
    let lf_au_index = if lf_count > 0 {
        Some(take24(&mut b)?)
    } else {
        None
    };
    let mut lf = Vec::with_capacity(lf_count);
    for _ in 0..lf_count {
        let n = take24(&mut b)? as usize;
        lf.push(take(&mut b, n)?);
    }
    let hf_count = take(&mut b, 1)?[0] as usize;
    let hf_au_index = if hf_codec == crate::HfCodec::Xhe && hf_count > 0 {
        Some(take24(&mut b)?)
    } else {
        None
    };
    let mut hf = Vec::with_capacity(hf_count);
    for _ in 0..hf_count {
        let n = u16::from_le_bytes(take(&mut b, 2)?.try_into().unwrap()) as usize;
        hf.push(take(&mut b, n)?);
    }
    if !b.is_empty() {
        return Err(EsError::Overlong);
    }
    Ok(Block {
        mid,
        lf_au_index,
        lf,
        hf_au_index,
        hf,
    })
}

/// Payload bytes of one block, for bit accounting (excludes nothing: the
/// composite block payload is what the container carries).
pub fn block_payload_bits(payload_len: usize) -> u64 {
    payload_len as u64 * 8
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_init(three: bool) -> Init {
        Init {
            profile: Profile::At600,
            three_track: three,
            mid_codec: crate::MidCodec::Opus,
            hf_codec: crate::HfCodec::Opus,
            lf_rate: 32000,
            playable_samples: 960_000,
            audio_sha256: [0xAB; 32],
            lf_asc: vec![1, 2, 3, 4, 5],
            mid_head: b"OpusHead\x01\x02\x00\x38\x01\x00\x00\x00\x00".to_vec(),
            hf_head: three.then(|| b"OpusHead\x01\x02\x00\x38\x01\x80\xBB\x00\x00".to_vec()),
        }
    }

    #[test]
    fn init_roundtrip() {
        for three in [false, true] {
            let init = sample_init(three);
            let b = init.encode();
            assert_eq!(Init::decode(&b).unwrap(), init);
        }
    }

    #[test]
    fn init_rejects() {
        let init = sample_init(false);
        let mut b = init.encode();
        b[0] = b'X';
        assert!(matches!(Init::decode(&b), Err(EsError::BadMagic)));
        let mut b = init.encode();
        b[4] = 9;
        assert!(matches!(Init::decode(&b), Err(EsError::UnsupportedVersion(9))));
        let mut b = init.encode();
        b[6] = 7;
        assert!(matches!(Init::decode(&b), Err(EsError::BadProfile(7))));
        let mut b = init.encode();
        b[8] = 9;
        assert!(matches!(Init::decode(&b), Err(EsError::BadMidCodec)));
        let mut b = init.encode();
        b[9] = 9;
        assert!(matches!(Init::decode(&b), Err(EsError::BadHfCodec)));
        let mut b = init.encode();
        b[10..14].copy_from_slice(&16000u32.to_le_bytes());
        assert!(matches!(Init::decode(&b), Err(EsError::LfRateMismatch)));
        let mut b = init.encode();
        b.push(0);
        assert!(matches!(Init::decode(&b), Err(EsError::Overlong)));
        let b = &init.encode()[..11];
        assert!(matches!(Init::decode(b), Err(EsError::Truncated)));
    }

    #[test]
    fn init_v2_decodes_as_opus_mid() {
        // Hand-encode a v2 INIT (no mid-codec byte): still accepted, mid
        // codec implied Opus.
        let i3 = sample_init(true).encode();
        let mut v2 = Vec::new();
        v2.extend_from_slice(&i3[..6]); // magic, version
        v2[4] = 2;
        v2.push(i3[6]); // profile
        v2.push(i3[7]); // three_track
        v2.extend_from_slice(&i3[10..]); // skip the v3 mid-codec + v4 hf-codec bytes
        let d = Init::decode(&v2).unwrap();
        assert_eq!(d.mid_codec, crate::MidCodec::Opus);
        assert_eq!(d.hf_codec, crate::HfCodec::Opus);
        assert!(d.three_track);
        // A v3 stream (mid-codec byte, no hf-codec byte) decodes as Opus top.
        let mut v3 = Vec::new();
        v3.extend_from_slice(&i3[..6]);
        v3[4] = 3;
        v3.extend_from_slice(&i3[6..9]); // profile, three_track, mid_codec
        v3.extend_from_slice(&i3[10..]);
        let d = Init::decode(&v3).unwrap();
        assert_eq!(d.hf_codec, crate::HfCodec::Opus);
    }

    #[test]
    fn codec_private_roundtrip() {
        let init = sample_init(true);
        let w = build_codec_private(&init, 20_001);
        assert_eq!(w.len(), 18 + init.encode().len());
        let (i2, avg) = parse_codec_private(&w).unwrap();
        assert_eq!(i2, init);
        assert_eq!(avg, 20_001);
        // not sena tag
        let mut bad = w.clone();
        bad[0] = 1;
        assert!(matches!(parse_codec_private(&bad), Err(EsError::BadWaveFormat)));
    }

    #[test]
    fn block_roundtrip() {
        let empty_lf: &[&[u8]] = &[];
        let empty_hf: &[&[u8]] = &[];
        let cases: Vec<BlockParts> = vec![
            BlockParts { mid: MidSection::Opus(b"mid"), lf: (0, empty_lf), hf: (0, empty_hf) },
            BlockParts { mid: MidSection::Opus(b"mid"), lf: (42, &[b"lfau"]), hf: (0, empty_hf) },
            BlockParts { mid: MidSection::Opus(b"mid"), lf: (0, &[b"lf0", b"lf1"]), hf: (0, &[b"h0", b"h1"]) },
            BlockParts { mid: MidSection::Opus(b"mid"), lf: (0, empty_lf), hf: (0, &[b"hf"]) },
            BlockParts { mid: MidSection::Xhe { au_index: 7, aus: vec![b"m0", b"m1"] }, lf: (0, empty_lf), hf: (0, empty_hf) },
            BlockParts { mid: MidSection::Xhe { au_index: 0, aus: vec![] }, lf: (3, &[b"lf"]), hf: (0, &[b"h"]) },
        ];
        for p in cases {
            let codec = match &p.mid {
                MidSection::Opus(_) => crate::MidCodec::Opus,
                MidSection::Xhe { .. } => crate::MidCodec::Xhe,
            };
            let b = encode_block(&p, crate::HfCodec::Opus);
            let d = decode_block(&b, codec, crate::HfCodec::Opus).unwrap();
            match (&d.mid, &p.mid) {
                (MidSection::Opus(a), MidSection::Opus(b)) => assert_eq!(a, b),
                (
                    MidSection::Xhe { au_index: a1, aus: u1 },
                    MidSection::Xhe { au_index: a2, aus: u2 },
                ) => {
                    assert_eq!(a1, a2);
                    assert_eq!(u1, u2);
                }
                _ => panic!("mid section kind mismatch"),
            }
            assert_eq!(d.lf, p.lf.1);
            assert_eq!(d.hf, p.hf.1);
            assert_eq!(
                d.lf_au_index,
                if d.lf.is_empty() { None } else { Some(p.lf.0) }
            );
            assert_eq!(d.hf_au_index, None);
        }
        // xHE top band: the hf section carries the first AU's index.
        let p = BlockParts {
            mid: MidSection::Xhe { au_index: 3, aus: vec![b"m"] },
            lf: (0, empty_lf),
            hf: (42, &[b"hf0", b"hf1"]),
        };
        let b = encode_block(&p, crate::HfCodec::Xhe);
        let d = decode_block(&b, crate::MidCodec::Xhe, crate::HfCodec::Xhe).unwrap();
        assert_eq!(d.hf_au_index, Some(42));
        assert_eq!(d.hf, p.hf.1);
        // ... and an empty hf section carries none.
        let p = BlockParts {
            mid: MidSection::Xhe { au_index: 3, aus: vec![b"m"] },
            lf: (0, empty_lf),
            hf: (42, empty_hf),
        };
        let b = encode_block(&p, crate::HfCodec::Xhe);
        let d = decode_block(&b, crate::MidCodec::Xhe, crate::HfCodec::Xhe).unwrap();
        assert_eq!(d.hf_au_index, None);
    }

    #[test]
    fn block_rejects() {
        assert!(decode_block(&[], crate::MidCodec::Opus, crate::HfCodec::Opus).is_err());
        // unknown flag
        assert!(matches!(
            decode_block(&[0x80, 1, 0, b'x', 0, 0], crate::MidCodec::Opus, crate::HfCodec::Opus),
            Err(EsError::BadBlock(_))
        ));
        // empty mid
        assert!(matches!(
            decode_block(&[0, 0, 0, 0, 0], crate::MidCodec::Opus, crate::HfCodec::Opus),
            Err(EsError::BadBlock("empty mid frame"))
        ));
        // trailing byte
        assert!(matches!(
            decode_block(&[0, 1, 0, b'x', 0, 0, 0], crate::MidCodec::Opus, crate::HfCodec::Opus),
            Err(EsError::Overlong)
        ));
        // truncated mid
        assert!(decode_block(&[0, 5, 0, b'x'], crate::MidCodec::Opus, crate::HfCodec::Opus).is_err());
        // lf section truncated
        assert!(decode_block(&[0, 1, 0, b'x', 1, 0, 0], crate::MidCodec::Opus, crate::HfCodec::Opus).is_err());
        // xhe mid section truncated
        assert!(decode_block(&[0, 1, 7, 0, 0], crate::MidCodec::Xhe, crate::HfCodec::Opus).is_err());
    }

    #[test]
    fn lf_placement() {
        // 16 kHz: AU spans 3072 48k frames; by-end placement.
        // AU0 (warmup) -> block 0; AU1 covers [0,3072) -> block 3.
        let n = 1001;
        assert_eq!(lf_block(0, 16000, n), 0);
        assert_eq!(lf_block(1, 16000, n), 3);
        assert_eq!(lf_block(2, 16000, n), 6);
        assert_eq!(lf_block(3, 16000, n), 9);
        assert_eq!(lf_block(5, 16000, n), 15);
        // 32 kHz: AU spans 1536 frames
        assert_eq!(lf_block(1, 32000, n), 1);
        assert_eq!(lf_block(2, 32000, n), 3);
        assert_eq!(lf_block(3, 32000, n), 4);
        // The AU covering any block boundary sits inside that block:
        // AU k covers [(k-1)*u, k*u) and lands in the block containing k*u-1.
        for rate in [16000, 32000] {
            let u = lf_au_frames_48k(rate);
            let mut prev = 0;
            for k in 1..10_000u64 {
                let b = lf_block(k, rate, 100_000);
                assert!(b * BLOCK_FRAMES < k * u, "rate {rate} au {k}");
                assert!((b + 1) * BLOCK_FRAMES >= k * u, "rate {rate} au {k}");
                assert!(b > prev, "rate {rate} au {k} not monotonic");
                prev = b;
            }
        }
        // EOS clamp: AUs past the grid land in the final block.
        assert_eq!(lf_block(400, 16000, 100), 99);
    }

    #[test]
    fn hf_placement() {
        let n = 1001;
        assert_eq!(hf_block(0, n), 0);
        assert_eq!(hf_block(1, n), 1);
        assert_eq!(hf_block(1000, n), 1000);
        assert_eq!(hf_block(1001, n), 1000); // EOS clamp
    }

    #[test]
    fn counts() {
        assert_eq!(block_count(960_000, 312), 1001);
        assert_eq!(block_count(960, 0), 1);
        assert_eq!(block_count(961, 0), 2);
    }
}
