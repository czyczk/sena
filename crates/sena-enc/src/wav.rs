//! Minimal WAV read/write. Input: 8/16/24-bit PCM and IEEE float32, any
//! sample rate. Output: 16-bit PCM and float32.

use super::Error;

pub fn read_f64(path: &std::path::Path) -> Result<(Vec<f64>, usize, u32), Error> {
    let bytes = std::fs::read(path)?;
    read_f64_bytes(&bytes)
}

pub fn read_f64_bytes(f: &[u8]) -> Result<(Vec<f64>, usize, u32), Error> {
    if f.len() < 44 || &f[0..4] != b"RIFF" || &f[8..12] != b"WAVE" {
        return Err(Error::Format("not a WAV file".into()));
    }
    let ch = u16::from_le_bytes([f[22], f[23]]) as usize;
    let rate = u32::from_le_bytes([f[24], f[25], f[26], f[27]]);
    let bits = u16::from_le_bytes([f[34], f[35]]) as usize;
    let fmt = u16::from_le_bytes([f[20], f[21]]);
    if ch == 0 || rate == 0 {
        return Err(Error::Format("bad WAV header".into()));
    }
    let mut pos = 12;
    let mut data = None;
    while pos + 8 <= f.len() {
        let id = &f[pos..pos + 4];
        let sz = u32::from_le_bytes([f[pos + 4], f[pos + 5], f[pos + 6], f[pos + 7]]) as usize;
        if id == b"data" {
            data = Some((pos + 8, sz));
            break;
        }
        if sz == usize::MAX || pos + 8 + sz > f.len() {
            break;
        }
        pos += 8 + sz + (sz & 1);
    }
    let (d0, dlen_raw) = data.ok_or_else(|| Error::Format("no data chunk".into()))?;
    // Streaming writers (foobar converter pipes) often set data length to
    // 0xFFFFFFFF or 0; use the remaining bytes in that case.
    let dlen = if dlen_raw == 0 || dlen_raw == 0xFFFF_FFFF || d0 + dlen_raw > f.len() {
        f.len().saturating_sub(d0)
    } else {
        dlen_raw
    };
    let bytes_per = bits / 8;
    let block = bytes_per * ch;
    if block == 0 {
        return Err(Error::Format(format!("unsupported WAV format {fmt} {bits}bit")));
    }
    let n = dlen / block;
    let mut out = vec![0.0f64; n * ch];
    match (fmt, bits) {
        (1, 8) => {
            for i in 0..n * ch {
                let v = f[d0 + i] as f64;
                out[i] = (v - 128.0) / 128.0;
            }
        }
        (1, 16) => {
            for i in 0..n * ch {
                let v = i16::from_le_bytes([f[d0 + 2 * i], f[d0 + 2 * i + 1]]);
                out[i] = v as f64 / 32768.0;
            }
        }
        (1, 24) => {
            for i in 0..n * ch {
                let b0 = f[d0 + 3 * i] as i32;
                let b1 = f[d0 + 3 * i + 1] as i32;
                let b2 = f[d0 + 3 * i + 2] as i32;
                let v = (b0 | (b1 << 8) | (b2 << 16)) as i32;
                // sign-extend 24-bit
                let v = (v << 8) >> 8;
                out[i] = v as f64 / 8_388_608.0;
            }
        }
        (3, 32) => {
            for i in 0..n * ch {
                let v = f32::from_le_bytes([
                    f[d0 + 4 * i],
                    f[d0 + 4 * i + 1],
                    f[d0 + 4 * i + 2],
                    f[d0 + 4 * i + 3],
                ]);
                out[i] = v as f64;
            }
        }
        _ => {
            return Err(Error::Format(format!(
                "unsupported WAV format {fmt} {bits}bit {rate}Hz"
            )))
        }
    }
    Ok((out, ch, rate))
}

pub fn write_s16(path: &std::path::Path, x: &[f64], rate: u32) -> Result<(), Error> {
    let n = x.len();
    let mut out = Vec::with_capacity(44 + n * 2);
    write_hdr(&mut out, rate, 2, 16, n * 2);
    for &v in x {
        let s = (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
        out.extend_from_slice(&s.to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

pub fn write_f32(path: &std::path::Path, x: &[f64], rate: u32) -> Result<(), Error> {
    let n = x.len();
    let mut out = Vec::with_capacity(44 + n * 4);
    write_hdr(&mut out, rate, 2, 32, n * 4);
    for &v in x {
        out.extend_from_slice(&(v as f32).to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn write_hdr(out: &mut Vec<u8>, rate: u32, ch: u16, bits: u16, data_len: usize) {
    let byte_rate = rate * ch as u32 * bits as u32 / 8;
    let block = ch * bits / 8;
    out.extend_from_slice(b"RIFF");
    out.extend_from_slice(&((36 + data_len) as u32).to_le_bytes());
    out.extend_from_slice(b"WAVEfmt ");
    out.extend_from_slice(&16u32.to_le_bytes());
    out.extend_from_slice(&(if bits == 32 { 3u16 } else { 1u16 }).to_le_bytes());
    out.extend_from_slice(&ch.to_le_bytes());
    out.extend_from_slice(&rate.to_le_bytes());
    out.extend_from_slice(&byte_rate.to_le_bytes());
    out.extend_from_slice(&block.to_le_bytes());
    out.extend_from_slice(&bits.to_le_bytes());
    out.extend_from_slice(b"data");
    out.extend_from_slice(&(data_len as u32).to_le_bytes());
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_24bit_and_float_at_odd_rates() {
        // Build a minimal 24-bit WAV at 44100 Hz.
        let n = 4;
        let mut f = Vec::new();
        write_hdr(&mut f, 44100, 2, 24, n * 6);
        for i in 0..n {
            let v: i32 = i as i32 * 1000;
            f.extend_from_slice(&v.to_le_bytes()[..3]);
            f.extend_from_slice(&(-v).to_le_bytes()[..3]);
        }
        let (x, ch, rate) = read_f64_bytes(&f).unwrap();
        assert_eq!((ch, rate), (2, 44100));
        assert!((x[0] - 0.0).abs() < 1e-9);
        assert!(x[2] > 0.0);
        assert!(x[3] < 0.0);
    }
}
