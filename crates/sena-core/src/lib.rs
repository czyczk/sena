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
    pub fn deduct_kbps(self) -> u32 {
        match self {
            Profile::At300 => 16,
            Profile::At600 => 24,
        }
    }
    pub fn from_crossover(fc: f64) -> Option<Profile> {
        match fc {
            x if (x - 300.0).abs() < 1.0 => Some(Profile::At300),
            x if (x - 600.0).abs() < 1.0 => Some(Profile::At600),
            _ => None,
        }
    }
}

pub const MIN_TOTAL_KBPS: u32 = 160;
pub const SENAV_THRESHOLD_KBPS: u32 = 192;

/// warmup trim in core-rate samples (one 1024-sample core frame)
pub const XHE_WARMUP_CORE: usize = 1024;

/// warmup trim in samples at the 48 kHz output rate, for a stream at lf_rate
pub fn xhe_delay_48k_at(lf_rate: u32) -> usize {
    XHE_WARMUP_CORE * 48000 / lf_rate as usize
}

/// Bitrate accounting (rule B). Returns (xhe_kbps, opus_kbps).
pub fn account(total_kbps: u32, profile: Profile) -> Option<(u32, u32)> {
    if total_kbps < MIN_TOTAL_KBPS {
        return None;
    }
    let d = profile.deduct_kbps();
    if total_kbps <= d {
        return None;
    }
    Some((d, total_kbps - d))
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn accounting() {
        assert_eq!(account(160, Profile::At300), Some((16, 144)));
        assert_eq!(account(160, Profile::At600), Some((24, 136)));
        assert_eq!(account(159, Profile::At300), None);
    }
    #[test]
    fn delays() {
        assert_eq!(xhe_delay_48k_at(16000), 3072);
        assert_eq!(xhe_delay_48k_at(32000), 1536);
    }
}
