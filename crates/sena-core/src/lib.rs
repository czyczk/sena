//! Sena shared constants: profiles, pads, delays, accounting.

pub mod es;

pub const SAMPLE_RATE: u32 = 48000;
pub const LF_RATE: u32 = 16000;
pub const PRE_GAIN: f64 = 0.631; // -4 dB
pub const FIR_TRANSITION_RATIO: f64 = 0.2;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Profile {
    At300,
    At600,
}

impl Profile {
    pub fn crossover_hz(self) -> f64 {
        match self {
            Profile::At300 => 300.0,
            Profile::At600 => 600.0,
        }
    }
    /// exhale non-eSBR preset (nominal 16*#+48 kbit/s)
    pub fn xhe_preset(self) -> u32 {
        match self {
            Profile::At300 => 1,
            Profile::At600 => 5,
        }
    }

    /// LF encoder input rate: exhale upsamples to 32 kHz internally for
    /// presets >= 5, so feed the profile's native rate directly.
    pub fn lf_rate(self) -> u32 {
        match self {
            Profile::At300 => 16000,
            Profile::At600 => 32000,
        }
    }
    /// Nominal budget allotted to the xHE-AAC (LF) track when splitting the
    /// requested total. This counts what the LF encoder is *measured* to
    /// spend on average; the encoder parameters themselves
    /// (`xhe_preset`) are unchanged by this number.
    pub fn deduct_kbps(self) -> u32 {
        match self {
            Profile::At300 => 24,
            Profile::At600 => 32,
        }
    }
    /// Hard minimum total bitrate: the LF deduction plus the lowest
    /// listenable Opus allocation ([`OPUS_MIN_KBPS`]). 56 for @300, 64 for
    /// @600.
    pub fn min_total_kbps(self) -> u32 {
        self.deduct_kbps() + OPUS_MIN_KBPS
    }
    pub fn from_crossover(fc: f64) -> Option<Profile> {
        match fc {
            x if (x - 300.0).abs() < 1.0 => Some(Profile::At300),
            x if (x - 600.0).abs() < 1.0 => Some(Profile::At600),
            _ => None,
        }
    }

    /// INIT wire byte for the profile (the ES header form).
    pub fn to_byte(self) -> u8 {
        match self {
            Profile::At300 => 0,
            Profile::At600 => 1,
        }
    }
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(Profile::At300),
            1 => Some(Profile::At600),
            _ => None,
        }
    }
}

/// Lowest listenable Opus allocation; each profile's hard minimum total is
/// its LF deduction plus this.
pub const OPUS_MIN_KBPS: u32 = 32;

/// Below this total bitrate a plain Opus encode is recommended over Sena
/// (soft reject; `--bypass-recommendations` overrides).
pub const RECOMMENDED_MIN_TOTAL_KBPS: u32 = 128;

pub const SENAV_THRESHOLD_KBPS: u32 = 192;

/// warmup trim in core-rate samples (one 1024-sample core frame)
pub const XHE_WARMUP_CORE: usize = 1024;

/// warmup trim in samples at the 48 kHz output rate, for a stream at lf_rate
pub fn xhe_delay_48k_at(lf_rate: u32) -> usize {
    XHE_WARMUP_CORE * SAMPLE_RATE as usize / lf_rate as usize
}

/// Bitrate accounting (rule B). Returns (xhe_kbps, opus_kbps).
pub fn account(total_kbps: u32, profile: Profile) -> Option<(u32, u32)> {
    if total_kbps < profile.min_total_kbps() {
        return None;
    }
    let d = profile.deduct_kbps();
    Some((d, total_kbps - d))
}

/// Second split point of the three-track layout: the Opus b19/b20 boundary
/// at 48 kHz / 20 ms frames (bands 19+20 = 12 kHz..20 kHz). This is the
/// profile number (@12000) AND the SSB carrier - the boundary the decoder
/// cares about; the FIR cutoff (12120, placing the transition above the
/// carrier) is an internal design detail and must never leak into the
/// profile name. The top band is
/// shifted down by this same frequency (analytic-signal SSB shift) so the
/// top track codes it as baseband content; at 16 kHz stream rate the 12 kHz
/// shift exactly fills the 0..8 kHz baseband, and content above 20 kHz is
/// dropped (Opus itself stops at 20 kHz).
pub const HF_SPLIT_HZ: f64 = 12000.0;

/// Nominal rate of the A_OPUSHF (top band) opus track as a function of the
/// nominal total: [`HF_KBPS_TIER1`] for three-track totals below
/// [`HF_TIER2_MIN_TOTAL_KBPS`], [`HF_KBPS_TIER2`] at and above.
pub fn hf_track_kbps(total_kbps: u32) -> u32 {
    if total_kbps >= HF_TIER2_MIN_TOTAL_KBPS {
        HF_KBPS_TIER2
    } else {
        HF_KBPS_TIER1
    }
}

/// A_OPUSHF nominal rate tiers: 96 kbit/s, and 128 kbit/s at totals >= 320k.
pub const HF_KBPS_TIER1: u32 = 96;
pub const HF_KBPS_TIER2: u32 = 128;
pub const HF_TIER2_MIN_TOTAL_KBPS: u32 = 320;

/// Stream rate of the A_OPUSHF track: the top band (12000 Hz..20 kHz) is
/// shifted down to baseband (0..8 kHz) and carried at 16 kHz.
pub const HF_TRACK_RATE: u32 = 16000;

/// Nominal totals at (and above) which the encoder uses the three-track
/// layout; below it the top bands stay in the single A_OPUS track.
pub const THREE_TRACK_MIN_KBPS: u32 = 288;

/// Topband-stereo default tier: opus-senav two-track with a nominal total in
/// [SENAV_THRESHOLD_KBPS, THREE_TRACK_MIN_KBPS) arms AUDIFF_TOPBAND_STEREO at
/// the Opus budget by default (below a 192k budget this is the exit-frame
/// floor; at/above it the deficit self-sensing - the knob switches semantics
/// inside the encoder at 192k).
pub const TOPBAND_DEFAULT_RANGE: std::ops::Range<u32> = SENAV_THRESHOLD_KBPS..THREE_TRACK_MIN_KBPS;

/// Three-track bitrate accounting. Returns (xhe_kbps, mid_kbps, hf_kbps);
/// `hf` follows [`hf_track_kbps`] and the remainder goes to the mid
/// track, which keeps the two-track [`OPUS_MIN_KBPS`] floor.
pub fn account3(total_kbps: u32, profile: Profile) -> Option<(u32, u32, u32)> {
    let hf = hf_track_kbps(total_kbps);
    if total_kbps < profile.min_total_kbps() + hf {
        return None;
    }
    let d = profile.deduct_kbps();
    Some((d, total_kbps - d - hf, hf))
}

/// Decode-relevant identity of the mid-layer codec, carried in the ES
/// header. The encoder recipe behind a non-Opus mid (e.g. Fraunhofer IIS
/// xHE-AAC CBR 128k via ezcd-cli) is an encode-side detail and is
/// deliberately not stored: any xHE-AAC mid decodes the same way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MidCodec {
    Opus,
    Xhe,
}

impl MidCodec {
    pub fn to_byte(self) -> u8 {
        match self {
            MidCodec::Opus => 0,
            MidCodec::Xhe => 1,
        }
    }
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(MidCodec::Opus),
            1 => Some(MidCodec::Xhe),
            _ => None,
        }
    }
    /// Short tag letter: the mid-codec slot of the profile naming scheme
    /// (`<split><mid>`: "600o" / "600x"). Opus is the elided default in
    /// two-track display names but is explicit in tags.
    pub fn tag_letter(self) -> &'static str {
        match self {
            MidCodec::Opus => "o",
            MidCodec::Xhe => "x",
        }
    }
}

/// Decode-relevant identity of the top-band codec, carried in the ES
/// header (v4+; earlier files imply Opus). Same encoder-identity rule as
/// [`MidCodec`]: the recipe is not stored.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HfCodec {
    Opus,
    Xhe,
}

impl HfCodec {
    pub fn to_byte(self) -> u8 {
        match self {
            HfCodec::Opus => 0,
            HfCodec::Xhe => 1,
        }
    }
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            0 => Some(HfCodec::Opus),
            1 => Some(HfCodec::Xhe),
            _ => None,
        }
    }
    /// Short tag letter appended after the HF split in the three-track tag
    /// when not the Opus default ("600x@12000x").
    pub fn tag_letter(self) -> &'static str {
        match self {
            HfCodec::Opus => "",
            HfCodec::Xhe => "x",
        }
    }
}

/// The "xfi" recipe family: exhale LF (unchanged) + mid Fraunhofer-IIS
/// xHE-AAC CBR 128 kbit/s at 44.1 kHz + top band (shifted, 12 kHz split).
/// The top band is Opus at 96-127k for nominal totals 256-287k; at 288k it
/// switches to Fraunhofer-IIS xHE-AAC CBR 128k at 44.1 kHz and the whole
/// stream's output rate becomes 44.1 kHz (no lane exceeds that rate, so no
/// upsampling). Always three-track; nominal totals are continuous, with
/// everything above the minimum feeding the top band (the LF deduction is
/// independent of the hf allotment; any slack stays unallocated).
pub const XFI_MIN_TOTAL_KBPS: u32 = 256;
pub const XFI_MAX_TOTAL_KBPS: u32 = 288;
pub const XFI_MID_KBPS: u32 = 128;
/// Stream rate of the xHE-AAC mid layer (encoder-enforced, CBR), and of
/// the xHE-AAC top band (and therefore of the whole decoded output) at the
/// 288k nominal.
pub const XFI_MID_RATE: u32 = 44100;

/// xfi-family top-band codec selection: FhG-IIS xHE-AAC at the 288k
/// nominal (where the hf allotment reaches 128k), Opus below it.
pub fn xfi_hf_codec(total_kbps: u32) -> HfCodec {
    if total_kbps >= XFI_MAX_TOTAL_KBPS {
        HfCodec::Xhe
    } else {
        HfCodec::Opus
    }
}

/// The xfi mid band carries at most the 12 kHz-split content, so the
/// 44.1 kHz <-> 48 kHz resamples only need a 13 kHz passband (stopband at
/// the 22.05 kHz Nyquist as usual) — the full-Nyquist spec would force a
/// ~192k-tap kernel at the 7.056 MHz lcm for zero audible benefit.
pub const XFI_MID_RESAMPLE_PASS_HZ: f64 = 13_000.0;

/// Nominal top-band rate for the xfi family: 96k at the 256k minimum,
/// +1k per +1k total, capped at 128k (reached at 288k).
pub fn xfi_hf_kbps(total_kbps: u32) -> u32 {
    96 + (total_kbps.clamp(XFI_MIN_TOTAL_KBPS, XFI_MAX_TOTAL_KBPS) - XFI_MIN_TOTAL_KBPS)
}

/// xfi-family accounting. Returns (xhe_kbps, mid_kbps, hf_kbps) with mid
/// fixed at [`XFI_MID_KBPS`] and hf following [`xfi_hf_kbps`].
pub fn account3_xfi(total_kbps: u32, profile: Profile) -> Option<(u32, u32, u32)> {
    if !(XFI_MIN_TOTAL_KBPS..=XFI_MAX_TOTAL_KBPS).contains(&total_kbps) {
        return None;
    }
    Some((profile.deduct_kbps(), XFI_MID_KBPS, xfi_hf_kbps(total_kbps)))
}

/// `SENA_PROFILE` tag value for the two-track layout ("300o"/"600o").
pub fn profile_tag(profile: Profile, mid: MidCodec) -> String {
    format!("{}{}", profile.crossover_hz() as u32, mid.tag_letter())
}

/// `SENA_PROFILE` tag value for the three-track layout ("600o@12000",
/// "600x@12000"; a trailing xHE top-band letter marks the xfi 288k
/// variant: "600x@12000x").
pub fn three_track_tag(profile: Profile, mid: MidCodec, hf: HfCodec) -> String {
    format!(
        "{}{}@{}{}",
        profile.crossover_hz() as u32,
        mid.tag_letter(),
        HF_SPLIT_HZ as u64,
        hf.tag_letter()
    )
}

/// Parse a `SENA_PROFILE` tag: `300o`/`600o`/`300x`/`600x` (two-track) or
/// with `@12000` (three-track; a trailing `x` after the split marks the
/// xHE top band). Bare `300`/`600` read as the Opus default. Returns the
/// LF profile, the mid codec and, for the three-track layout, the HF
/// split point and top-band codec (only 12000 is defined).
pub fn parse_profile_tag(tag: &str) -> Option<(Profile, MidCodec, Option<(f64, HfCodec)>)> {
    let (lf, hf) = match tag.split_once('@') {
        None => (tag, None),
        Some((lf, hf)) => (lf, Some(hf)),
    };
    let (lf, mid) = match lf.strip_suffix('x') {
        Some(lf) => (lf, MidCodec::Xhe),
        None => (lf.strip_suffix('o').unwrap_or(lf), MidCodec::Opus),
    };
    let lf_hz: f64 = lf.parse().ok()?;
    let profile = Profile::from_crossover(lf_hz)?;
    match hf {
        None => Some((profile, mid, None)),
        Some(hf) => {
            let (hf, codec) = match hf.strip_suffix('x') {
                Some(h) => (h, HfCodec::Xhe),
                None => (hf, HfCodec::Opus),
            };
            let hf_hz: f64 = hf.parse().ok()?;
            if (hf_hz - HF_SPLIT_HZ).abs() < 0.5 {
                Some((profile, mid, Some((hf_hz, codec))))
            } else {
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accounting() {
        assert_eq!(account(160, Profile::At300), Some((24, 136)));
        assert_eq!(account(160, Profile::At600), Some((32, 128)));
        assert_eq!(account(192, Profile::At600), Some((32, 160)));
        // hard minimums: deduction + 32k Opus floor
        assert_eq!(Profile::At300.min_total_kbps(), 56);
        assert_eq!(Profile::At600.min_total_kbps(), 64);
        assert_eq!(account(55, Profile::At300), None);
        assert_eq!(account(56, Profile::At300), Some((24, 32)));
        assert_eq!(account(63, Profile::At600), None);
        assert_eq!(account(64, Profile::At600), Some((32, 32)));
    }
    #[test]
    fn accounting3() {
        assert_eq!(account3(288, Profile::At600), Some((32, 160, 96)));
        assert_eq!(account3(288, Profile::At300), Some((24, 168, 96)));
        assert_eq!(account3(320, Profile::At600), Some((32, 160, 128)));
        assert_eq!(account3(352, Profile::At600), Some((32, 192, 128)));
        assert_eq!(account3(159, Profile::At600), None);
        assert_eq!(account3(160, Profile::At600), Some((32, 32, 96)));
    }
    #[test]
    fn accounting3_xfi() {
        // continuous [256, 288]: mid fixed 128, hf 96 -> 128, lf deduct off
        // the hf allotment
        assert_eq!(account3_xfi(256, Profile::At600), Some((32, 128, 96)));
        assert_eq!(account3_xfi(256, Profile::At300), Some((24, 128, 96)));
        assert_eq!(account3_xfi(272, Profile::At600), Some((32, 128, 112)));
        assert_eq!(account3_xfi(288, Profile::At600), Some((32, 128, 128)));
        assert_eq!(account3_xfi(288, Profile::At300), Some((24, 128, 128)));
        assert_eq!(account3_xfi(255, Profile::At600), None);
        assert_eq!(account3_xfi(289, Profile::At600), None);
    }
    #[test]
    fn profile_tags() {
        assert_eq!(profile_tag(Profile::At600, MidCodec::Opus), "600o");
        assert_eq!(
            three_track_tag(Profile::At300, MidCodec::Opus, HfCodec::Opus),
            "300o@12000"
        );
        assert_eq!(
            three_track_tag(Profile::At600, MidCodec::Xhe, HfCodec::Opus),
            "600x@12000"
        );
        assert_eq!(
            three_track_tag(Profile::At600, MidCodec::Xhe, HfCodec::Xhe),
            "600x@12000x"
        );
        assert_eq!(
            parse_profile_tag("600"),
            Some((Profile::At600, MidCodec::Opus, None))
        );
        assert_eq!(
            parse_profile_tag("600o"),
            Some((Profile::At600, MidCodec::Opus, None))
        );
        assert_eq!(
            parse_profile_tag("300x"),
            Some((Profile::At300, MidCodec::Xhe, None))
        );
        assert_eq!(
            parse_profile_tag("300o@12000"),
            Some((Profile::At300, MidCodec::Opus, Some((HF_SPLIT_HZ, HfCodec::Opus))))
        );
        assert_eq!(
            parse_profile_tag("600x@12000x"),
            Some((Profile::At600, MidCodec::Xhe, Some((HF_SPLIT_HZ, HfCodec::Xhe))))
        );
        assert_eq!(
            parse_profile_tag("600x@12000"),
            Some((Profile::At600, MidCodec::Xhe, Some((HF_SPLIT_HZ, HfCodec::Opus))))
        );
        assert_eq!(parse_profile_tag("600@15600"), None);
        assert_eq!(parse_profile_tag("1200"), None);
        assert_eq!(parse_profile_tag(""), None);
    }
    #[test]
    fn topband_default_tier() {
        assert!(TOPBAND_DEFAULT_RANGE.contains(&192));
        assert!(TOPBAND_DEFAULT_RANGE.contains(&287));
        assert!(!TOPBAND_DEFAULT_RANGE.contains(&288));
        assert!(!TOPBAND_DEFAULT_RANGE.contains(&191));
    }
    #[test]
    fn delays() {
        assert_eq!(xhe_delay_48k_at(16000), 3072);
        assert_eq!(xhe_delay_48k_at(32000), 1536);
    }
}
