//! Minimal EBML writer (Matroska subset).

/// EBML variable-length integer.
pub fn vint_size(v: u64) -> usize {
    let mut size = 1;
    while size <= 8 && v >= (1u64 << (7 * size)) - 1 {
        size += 1;
    }
    size
}

pub fn write_vint(out: &mut Vec<u8>, v: u64) {
    let size = vint_size(v);
    let mut b = [0u8; 8];
    let mut t = v;
    for i in (0..size).rev() {
        b[i] = (t & 0xFF) as u8;
        t >>= 8;
    }
    b[0] |= 1 << (8 - size); // marker bit
    out.extend_from_slice(&b[..size]);
}

/// Write an element: fixed-size ID + size + payload.
pub fn write_element(out: &mut Vec<u8>, id: &[u8], payload: &[u8]) {
    out.extend_from_slice(id);
    write_vint(out, payload.len() as u64);
    out.extend_from_slice(payload);
}

/// Element with size forced to a given byte length (for clusters/segment).
pub fn write_element_sized(out: &mut Vec<u8>, id: &[u8], size_len: usize, payload: &[u8]) {
    out.extend_from_slice(id);
    assert!(size_len >= 1 && size_len <= 8);
    assert!(vint_size(payload.len() as u64) <= size_len);
    // pad the size vint to size_len bytes by using a larger marker
    let first_marker = 1 << (8 - size_len);
    let mut b = [0u8; 8];
    let mut t = payload.len() as u64;
    for i in (0..size_len).rev() {
        b[i] = (t & 0xFF) as u8;
        t >>= 8;
    }
    b[0] |= first_marker;
    out.extend_from_slice(&b[..size_len]);
    out.extend_from_slice(payload);
}

pub fn uint(out: &mut Vec<u8>, id: &[u8], v: u64) {
    let mut b = [0u8; 8];
    let mut t = v;
    let mut n = 0;
    while t > 0 || n == 0 {
        b[7 - n] = (t & 0xFF) as u8;
        t >>= 8;
        n += 1;
    }
    write_element(out, id, &b[8 - n..]);
}

pub fn float(out: &mut Vec<u8>, id: &[u8], v: f64) {
    let mut p = vec![];
    let bits = v.to_bits();
    let mut len = 4;
    if bits & 0x0000_0000_FFFF_FFFF == 0 {
        len = 4;
    }
    for i in (0..len).rev() {
        p.push(((bits >> (8 * i)) & 0xFF) as u8);
    }
    write_element(out, id, &p);
}

pub fn str(out: &mut Vec<u8>, id: &[u8], s: &str) {
    write_element(out, id, s.as_bytes());
}

pub fn bin(out: &mut Vec<u8>, id: &[u8], b: &[u8]) {
    write_element(out, id, b);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vint_roundtrip() {
        for v in [0u64, 1, 127, 128, 8192, 1 << 20, 1 << 50] {
            let mut b = vec![];
            write_vint(&mut b, v);
            let size = b.len();
            let mut r = 0u64;
            for &x in &b {
                r = (r << 8) | x as u64;
            }
            r &= (1u64 << (7 * size)) - 1;
            assert_eq!(r, v, "vint {v}");
        }
    }
}
