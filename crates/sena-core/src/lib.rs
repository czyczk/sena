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
    fn delays() {
        assert_eq!(xhe_delay_48k_at(16000), 3072);
        assert_eq!(xhe_delay_48k_at(32000), 1536);
    }
}
