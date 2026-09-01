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

/// Incremental WAV redder: feed bytes in arbitrary chunks, then take
/// decoded interleaved frames. Header parsing waits until the fmt/data
/// chunks are fully buffered; streaming data lengths (0 / 0xFFFFFFFF) run
/// until EOF. Supported payloads: 8/16/24-bit PCM and IEEE float32.
pub struct WavStream {
    parsed: bool,
    ch: usize,
    rate: u32,
    bits: usize,
    fmt: u16,
    /// Maximum frames from the header's data-chunk size when sane
    /// (streaming pipes often declare 0 / 0xFFFFFFFF -> run until EOF).
    declared_frames: Option<usize>,
    frames_taken: usize,
    buf: Vec<u8>, // after parse: data payload only
}

impl WavStream {
    pub fn new() -> Self {
        Self {
            parsed: false,
            ch: 0,
            rate: 0,
            bits: 0,
            fmt: 0,
            declared_frames: None,
            frames_taken: 0,
            buf: Vec::new(),
        }
    }

    /// Append bytes; tries to parse the header once enough is available.
    pub fn feed(&mut self, bytes: &[u8]) -> Result<(), Error> {
        self.buf.extend_from_slice(bytes);
        if !self.parsed {
            self.try_parse()?;
        }
        Ok(())
    }

    fn try_parse(&mut self) -> Result<(), Error> {
        let f = &self.buf;
        if f.len() < 12 {
            return Ok(()); // wait for more bytes
        }
        if &f[0..4] != b"RIFF" || &f[8..12] != b"WAVE" {
            return Err(Error::Format("not a WAV file".into()));
        }
        let mut pos = 12;
        let mut fmt_info: Option<(usize, u32, usize, u16)> = None;
        let mut data = None;
        let mut data_len = 0usize;
        while pos + 8 <= f.len() {
            let id = &f[pos..pos + 4];
            let sz = u32::from_le_bytes([f[pos + 4], f[pos + 5], f[pos + 6], f[pos + 7]]) as usize;
            if id == b"data" {
                data = Some(pos + 8);
                data_len = sz;
                break;
            }
            if pos + 8 + sz > f.len() {
                return Ok(()); // chunk header not fully buffered yet
            }
            if id == b"fmt " && sz >= 16 {
                // payload starts at pos + 8: fmtTag(2) ch(2) rate(4)
                // byteRate(4) blockAlign(2) bits(2)
                let fmt = u16::from_le_bytes([f[pos + 8], f[pos + 9]]);
                let ch = u16::from_le_bytes([f[pos + 10], f[pos + 11]]) as usize;
                let rate = u32::from_le_bytes([f[pos + 12], f[pos + 13], f[pos + 14], f[pos + 15]]);
                let bits = u16::from_le_bytes([f[pos + 22], f[pos + 23]]) as usize;
                fmt_info = Some((ch, rate, bits, fmt));
            }
            pos += 8 + sz + (sz & 1);
        }
        let (ch, rate, bits, fmt) = fmt_info.ok_or(Error::Format("no fmt chunk".into()))?;
        let ds = data.ok_or(Error::Format("no data chunk".into()))?;
        if ch == 0 || rate == 0 || bits / 8 * ch == 0 {
            return Err(Error::Format("bad WAV header".into()));
        }
        self.parsed = true;
        self.ch = ch;
        self.rate = rate;
        self.bits = bits;
        self.fmt = fmt;
        // The data chunk size as declared: sane sizes are authoritative so
        // any trailing bytes after the audio (metadata/padding the pipe may
        // append) are never decoded as samples. 0 / 0xFFFFFFFF = stream to
        // EOF.
        let block = bits / 8 * ch;
        let dsz = if data_len == 0 || data_len == usize::MAX { None } else { Some(data_len) };
        self.declared_frames = dsz.map(|d| d / block);
        self.buf.drain(..ds);
        Ok(())
    }

    pub fn parsed(&self) -> bool {
        self.parsed
    }

    pub fn rate(&self) -> Option<u32> {
        self.parsed.then_some(self.rate)
    }

    pub fn channels(&self) -> Option<usize> {
        self.parsed.then_some(self.ch)
    }

    /// Decode up to `target_frames` complete frames from the buffered data.
    /// Ok(None) when no complete frame is available; feed more or finish().
    pub fn take(&mut self, target_frames: usize) -> Result<Option<Vec<f64>>, Error> {
        if !self.parsed {
            return Ok(None);
        }
        let block = self.bits / 8 * self.ch;
        if self.buf.len() < block {
            return Ok(None);
        }
        let mut n = (self.buf.len() / block).min(target_frames);
        if let Some(d) = self.declared_frames {
            let left = d.saturating_sub(self.frames_taken);
            if left == 0 {
                return Ok(None);
            }
            n = n.min(left);
        }
        let mut out = Vec::with_capacity(n * self.ch);
        match (self.fmt, self.bits) {
            (1, 8) => {
                for i in 0..n * self.ch {
                    out.push((self.buf[i] as f64 - 128.0) / 128.0);
                }
            }
            (1, 16) => {
                for i in 0..n * self.ch {
                    let v = i16::from_le_bytes([self.buf[2 * i], self.buf[2 * i + 1]]);
                    out.push(v as f64 / 32768.0);
                }
            }
            (1, 24) => {
                for i in 0..n * self.ch {
                    let b0 = self.buf[3 * i] as i32;
                    let b1 = self.buf[3 * i + 1] as i32;
                    let b2 = self.buf[3 * i + 2] as i32;
                    let v = ((b0 | (b1 << 8) | (b2 << 16)) << 8) >> 8;
                    out.push(v as f64 / 8_388_608.0);
                }
            }
            (3, 32) => {
                for i in 0..n * self.ch {
                    let v = f32::from_le_bytes([
                        self.buf[4 * i],
                        self.buf[4 * i + 1],
                        self.buf[4 * i + 2],
                        self.buf[4 * i + 3],
                    ]);
                    out.push(v as f64);
                }
            }
            _ => return Err(Error::Format(format!("unsupported WAV format {} {}bit", self.fmt, self.bits))),
        }
        self.buf.drain(..n * block);
        self.frames_taken += n;
        Ok(Some(out))
    }

    /// Signal end of input: a trailing partial frame is dropped, like the
    /// batch reader (which floors by the block size).
    pub fn finish(&self) -> Result<(), Error> {
        if !self.parsed {
            return Err(Error::Format("no WAV header".into()));
        }
        Ok(())
    }
}

/// Streaming WAV writer: writes a placeholder header, appends interleaved
/// f64 samples (s16 or f32), patches the RIFF/data sizes on finish.
pub struct WavWriter {
    f: std::fs::File,
    bits: usize,
    body: u64,
}

impl WavWriter {
    pub fn create(path: &std::path::Path, rate: u32, bits: u16, ch: u16) -> Result<Self, Error> {
        use std::io::{Seek, Write};
        let mut f = std::fs::File::create(path)?;
        let mut hdr = Vec::new();
        write_hdr(&mut hdr, rate, ch, bits, 0);
        f.write_all(&hdr)?;
        f.seek(std::io::SeekFrom::End(0))?;
        Ok(Self { f, bits: bits as usize, body: 0 })
    }

    pub fn push(&mut self, x: &[f64]) -> Result<(), Error> {
        use std::io::Write;
        let mut out = Vec::with_capacity(x.len() * (self.bits / 8));
        match self.bits {
            16 => {
                for &v in x {
                    let s = (v.clamp(-1.0, 1.0) * 32767.0).round() as i16;
                    out.extend_from_slice(&s.to_le_bytes());
                }
            }
            32 => {
                for &v in x {
                    out.extend_from_slice(&(v as f32).to_le_bytes());
                }
            }
            _ => unreachable!("unsupported output bits"),
        }
        self.f.write_all(&out)?;
        self.body += out.len() as u64;
        Ok(())
    }

    /// Patch the RIFF and data chunk sizes now that the body length is known.
    pub fn finish(mut self) -> Result<(), Error> {
        use std::io::{Seek, Write};
        let total = self.body + 36;
        let mut patch = Vec::new();
        patch.extend_from_slice(&(total as u32).to_le_bytes());
        patch.extend_from_slice(&(self.body as u32).to_le_bytes());
        self.f.seek(std::io::SeekFrom::Start(4))?;
        self.f.write_all(&patch[..4])?;
        self.f.seek(std::io::SeekFrom::Start(40))?;
        self.f.write_all(&patch[4..])?;
        Ok(())
    }
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
