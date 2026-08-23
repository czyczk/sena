//! Minimal WAV read/write (16-bit PCM in, 16-bit and float32 out).

use super::Error;

pub fn read_f64(path: &std::path::Path) -> Result<(Vec<f64>, usize), Error> {
    let f = std::fs::read(path)?;
    if f.len() < 44 || &f[0..4] != b"RIFF" {
        return Err(Error::Format("not a WAV file".into()));
    }
    let ch = u16::from_le_bytes([f[22], f[23]]) as usize;
    let rate = u32::from_le_bytes([f[24], f[25], f[26], f[27]]);
    let bits = u16::from_le_bytes([f[34], f[35]]) as usize;
    let fmt = u16::from_le_bytes([f[20], f[21]]);
    let mut pos = 12;
    let mut data = None;
    while pos + 8 <= f.len() {
        let id = &f[pos..pos + 4];
        let sz = u32::from_le_bytes([f[pos + 4], f[pos + 5], f[pos + 6], f[pos + 7]]) as usize;
        if id == b"data" {
            data = Some((pos + 8, sz));
            break;
        }
        pos += 8 + sz + (sz & 1);
    }
    let (d0, dlen) = data.ok_or_else(|| Error::Format("no data chunk".into()))?;
    let bytes = bits / 8;
    let n = dlen / (bytes * ch);
    let mut out = vec![0.0; n * ch];
    match (fmt, bits) {
        (1, 16) => {
            for i in 0..n * ch {
                let v = i16::from_le_bytes([f[d0 + 2 * i], f[d0 + 2 * i + 1]]);
                out[i] = v as f64 / 32768.0;
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
    Ok((out, ch))
}

pub fn write_s16(path: &std::path::Path, x: &[f64], rate: u32) -> Result<(), Error> {
    let n = x.len();
    let mut out = Vec::with_capacity(44 + n * 2);
    write_hdr(&mut out, n, rate, 2, 16, n * 2);
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
    write_hdr(&mut out, n, rate, 2, 32, n * 4);
    for &v in x {
        out.extend_from_slice(&(v as f32).to_le_bytes());
    }
    std::fs::write(path, out)?;
    Ok(())
}

fn write_hdr(out: &mut Vec<u8>, n: usize, rate: u32, ch: u16, bits: u16, data_len: usize) {
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
