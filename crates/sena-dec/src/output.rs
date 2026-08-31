//! PCM output conversion and WAV/raw encoding.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SampleFormat {
    F32,
    S24,
    S16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dither {
    Tpdf,
    None,
}

struct SplitMix64 {
    state: u64,
}

impl SplitMix64 {
    fn new(seed: u64) -> Self {
        Self { state: seed }
    }
    fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    }
    fn next_unit(&mut self) -> f64 {
        ((self.next_u64() >> 40) as f64) * (1.0 / 16_777_216.0)
    }
}

fn f32_to_s24(x: f64) -> i32 {
    (x * 8_388_608.0).round().clamp(-8_388_608.0, 8_388_607.0) as i32
}

fn f32_to_s16(x: f64, dither: &mut Option<SplitMix64>) -> i16 {
    let noise = match dither {
        Some(rng) => rng.next_unit() + rng.next_unit() - 1.0,
        None => 0.0,
    };
    (x * 32768.0 + noise).round().clamp(-32768.0, 32767.0) as i16
}

pub fn encode_wav(pcm: &[f64], format: SampleFormat, dither: Dither) -> Result<Vec<u8>, String> {
    if pcm.len() % 2 != 0 {
        return Err("interleaved stereo buffer has odd length".into());
    }
    let mut body = match format {
        SampleFormat::F32 => {
            let mut b = Vec::with_capacity(pcm.len() * 4);
            for &s in pcm {
                b.extend_from_slice(&(s as f32).to_le_bytes());
            }
            b
        }
        SampleFormat::S24 => {
            let mut b = Vec::with_capacity(pcm.len() * 3);
            for &s in pcm {
                let q = f32_to_s24(s);
                b.extend_from_slice(&q.to_le_bytes()[..3]);
            }
            b
        }
        SampleFormat::S16 => {
            let mut rng = match dither {
                Dither::Tpdf => Some(SplitMix64::new(0x5345_4E41_0000_0001)),
                Dither::None => None,
            };
            let mut b = Vec::with_capacity(pcm.len() * 2);
            for &s in pcm {
                b.extend_from_slice(&f32_to_s16(s, &mut rng).to_le_bytes());
            }
            b
        }
    };
    let data_len = u32::try_from(body.len()).map_err(|_| "audio too large for RIFF".to_string())?;
    let (format_tag, bits) = match format {
        SampleFormat::F32 => (3u16, 32u16),
        _ => (1u16, if matches!(format, SampleFormat::S24) { 24 } else { 16 }),
    };
    let mut out = Vec::with_capacity(44 + body.len());
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&(36 + data_len).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&format_tag.to_le_bytes());
    out.extend_from_slice(&2u16.to_le_bytes());
    out.extend_from_slice(&48_000u32.to_le_bytes());
    let bytes_per = bits / 8;
    out.extend_from_slice(&(48_000u32 * u32::from(bytes_per) * 2).to_le_bytes());
    out.extend_from_slice(&(2u16 * bytes_per).to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&data_len.to_le_bytes());
    out.append(&mut body);
    Ok(out)
}

pub fn encode_raw(pcm: &[f64], format: SampleFormat, dither: Dither) -> Result<Vec<u8>, String> {
    let mut out = match format {
        SampleFormat::F32 => {
            let mut b = Vec::with_capacity(pcm.len() * 4);
            for &s in pcm {
                b.extend_from_slice(&(s as f32).to_le_bytes());
            }
            b
        }
        SampleFormat::S24 => {
            let mut b = Vec::with_capacity(pcm.len() * 3);
            for &s in pcm {
                let q = f32_to_s24(s);
                b.extend_from_slice(&q.to_le_bytes()[..3]);
            }
            b
        }
        SampleFormat::S16 => {
            let mut rng = match dither {
                Dither::Tpdf => Some(SplitMix64::new(0x5345_4E41_0000_0001)),
                Dither::None => None,
            };
            let mut b = Vec::with_capacity(pcm.len() * 2);
            for &s in pcm {
                b.extend_from_slice(&f32_to_s16(s, &mut rng).to_le_bytes());
            }
            b
        }
    };
    out.retain(|_| true);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn s16_full_scale_and_dither_none() {
        let v = f32_to_s16(1.0, &mut None);
        assert_eq!(v, 32767);
        let v = f32_to_s16(-1.0, &mut None);
        assert_eq!(v, -32768);
        let v = f32_to_s16(0.5, &mut None);
        assert_eq!(v, 16384);
    }

    #[test]
    fn tpdf_is_reproducible() {
        let pcm = vec![0.0, -0.25, 0.75, 0.125];
        let a = encode_raw(&pcm, SampleFormat::S16, Dither::Tpdf).unwrap();
        let b = encode_raw(&pcm, SampleFormat::S16, Dither::Tpdf).unwrap();
        assert_eq!(a, b);
        let c = encode_raw(&pcm, SampleFormat::S16, Dither::None).unwrap();
        assert_ne!(a, c);
    }

    #[test]
    fn wav_header_smoke() {
        let w = encode_wav(&[0.0, 0.0], SampleFormat::S16, Dither::None).unwrap();
        assert_eq!(&w[0..4], b"RIFF");
        assert_eq!(w.len(), 48);
    }
}
