//! Sena shared constants: profiles, pads, delays, accounting.

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
    XHE_WARMUP_CORE * 48000 / lf_rate as usize
}

/// Bitrate accounting (rule B). Returns (xhe_kbps, opus_kbps).
pub fn account(total_kbps: u32, profile: Profile) -> Option<(u32, u32)> {
    if total_kbps < profile.min_total_kbps() {
        return None;
    }
    let d = profile.deduct_kbps();
    Some((d, total_kbps - d))
}

/// Second split point of the three-track layout: the Opus b19 edge at
/// 48 kHz / 20 ms frames (bands 19+20 = 15.6 kHz..Nyquist). The top band is
/// shifted down by this same frequency (analytic-signal SSB shift) so the
/// top track codes it as baseband content.
pub const HF_SPLIT_HZ: f64 = 15600.0;

/// Fixed nominal rate of the A_OPUSHF (top band) opus track.
pub const HF_TRACK_KBPS: u32 = 64;

/// Stream rate of the A_OPUSHF track: the top band (15600 Hz..Nyquist) is
/// shifted down to baseband (0..8.4 kHz) and carried at 16 kHz.
pub const HF_TRACK_RATE: u32 = 16000;

/// Nominal totals at (and above) which the encoder uses the three-track
/// layout; below it the top bands stay in the single A_OPUS track.
pub const THREE_TRACK_MIN_KBPS: u32 = 256;

/// Topband-stereo default tier: opus-senav with a nominal total inside
/// [SENAV_THRESHOLD_KBPS, THREE_TRACK_MIN_KBPS) floors the intensity-exit
/// frames at the opus budget by default.
pub const TOPBAND_DEFAULT_RANGE: std::ops::Range<u32> = SENAV_THRESHOLD_KBPS..THREE_TRACK_MIN_KBPS;

/// Three-track bitrate accounting. Returns (xhe_kbps, mid_kbps, hf_kbps);
/// `hf` is fixed at [`HF_TRACK_KBPS`] and the remainder goes to the mid
/// track, which keeps the two-track [`OPUS_MIN_KBPS`] floor.
pub fn account3(total_kbps: u32, profile: Profile) -> Option<(u32, u32, u32)> {
    if total_kbps < profile.min_total_kbps() + HF_TRACK_KBPS {
        return None;
    }
    let d = profile.deduct_kbps();
    Some((d, total_kbps - d - HF_TRACK_KBPS, HF_TRACK_KBPS))
}

/// `SENA_PROFILE` tag value for the two-track layout ("300"/"600").
pub fn profile_tag(profile: Profile) -> String {
    (profile.crossover_hz() as u32).to_string()
}

/// `SENA_PROFILE` tag value for the three-track layout ("600@15600").
pub fn three_track_tag(profile: Profile) -> String {
    format!("{}@{}", profile.crossover_hz() as u32, HF_SPLIT_HZ as u64)
}

/// Parse a `SENA_PROFILE` tag: `300`/`600` (two-track) or `300@15600`/
/// `600@15600` (three-track). Returns the LF profile and, for the
/// three-track layout, the HF split point (only 15600 is defined).
pub fn parse_profile_tag(tag: &str) -> Option<(Profile, Option<f64>)> {
    let (lf, hf) = match tag.split_once('@') {
        None => (tag, None),
        Some((lf, hf)) => (lf, Some(hf)),
    };
    let lf_hz: f64 = lf.parse().ok()?;
    let profile = Profile::from_crossover(lf_hz)?;
    match hf {
        None => Some((profile, None)),
        Some(hf) => {
            let hf_hz: f64 = hf.parse().ok()?;
            if (hf_hz - HF_SPLIT_HZ).abs() < 0.5 {
                Some((profile, Some(hf_hz)))
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
        assert_eq!(account3(256, Profile::At600), Some((32, 160, 64)));
        assert_eq!(account3(256, Profile::At300), Some((24, 168, 64)));
        assert_eq!(account3(320, Profile::At600), Some((32, 224, 64)));
        assert_eq!(account3(128, Profile::At600), Some((32, 32, 64)));
        assert_eq!(account3(127, Profile::At600), None);
    }
    #[test]
    fn profile_tags() {
        assert_eq!(profile_tag(Profile::At600), "600");
        assert_eq!(three_track_tag(Profile::At300), "300@15600");
        assert_eq!(parse_profile_tag("600"), Some((Profile::At600, None)));
        assert_eq!(
            parse_profile_tag("300@15600"),
            Some((Profile::At300, Some(HF_SPLIT_HZ)))
        );
        assert_eq!(
            parse_profile_tag("600@15600"),
            Some((Profile::At600, Some(HF_SPLIT_HZ)))
        );
        assert_eq!(parse_profile_tag("600@14400"), None);
        assert_eq!(parse_profile_tag("1200"), None);
        assert_eq!(parse_profile_tag(""), None);
    }
    #[test]
    fn topband_default_tier() {
        assert!(TOPBAND_DEFAULT_RANGE.contains(&192));
        assert!(TOPBAND_DEFAULT_RANGE.contains(&255));
        assert!(!TOPBAND_DEFAULT_RANGE.contains(&256));
        assert!(!TOPBAND_DEFAULT_RANGE.contains(&191));
    }
    #[test]
    fn delays() {
        assert_eq!(xhe_delay_48k_at(16000), 3072);
        assert_eq!(xhe_delay_48k_at(32000), 1536);
    }
}
