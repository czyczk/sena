//! Host-callback io plumbing: slurp reads, positioned range reads, and the
//! plan-then-write tag/attachment rewrite machinery.

use super::{RawReadFn, RawSeekFn, RawWriteFn, SenaDecIo, SenaFileIo, WHENCE_SET, WHENCE_CUR};

/// Slurp-read chunk size.
const SLURP_CHUNK: usize = 64 * 1024;

/// Range-read chunk size (range reads are exact-length).
const RANGE_READ_CHUNK: usize = 64 * 1024;

/// Slurp the whole stream through the read callback (used when the host
/// cannot seek). Shared by the SenaDecIo and SenaFileIo wrappers below.
pub(crate) unsafe fn slurp(user: *mut std::ffi::c_void, read: RawReadFn) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let mut buf = vec![0u8; SLURP_CHUNK];
    loop {
        let n = unsafe { read(user, buf.as_mut_ptr().cast(), buf.len() as u64) };
        if n < 0 {
            return Err(format!("io: read failed ({n})"));
        }
        if n == 0 {
            break;
        }
        let n = n as usize;
        if n > buf.len() {
            return Err("io: read callback returned more than the buffer size".into());
        }
        out.extend_from_slice(&buf[..n]);
    }
    Ok(out)
}

pub(crate) unsafe fn read_all(io: &SenaDecIo) -> Result<Vec<u8>, String> {
    let read = io.read.ok_or("io: read callback is NULL")?;
    unsafe { slurp(io.user_data, read) }
}

pub(crate) unsafe fn read_file_at(io: &SenaFileIo, pos: u64, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let read = io.read.ok_or("io: read callback is NULL")?;
    let seek = io
        .seek
        .ok_or("io: seek callback is required for range reads")?;
    if unsafe { seek(io.user_data, pos as i64, WHENCE_SET) } < 0 {
        return Err(format!("io: seek to {pos} failed"));
    }
    let mut out = Vec::with_capacity(len);
    let mut chunk = vec![0u8; len.min(RANGE_READ_CHUNK)];
    while out.len() < len {
        let want = (len - out.len()).min(chunk.len());
        let n = unsafe { read(io.user_data, chunk.as_mut_ptr().cast(), want as u64) };
        if n < 0 {
            return Err(format!("io: read failed ({n})"));
        }
        if n == 0 {
            return Err(format!(
                "io: truncated range read at {} (wanted {want})",
                pos + out.len() as u64
            ));
        }
        let n = n as usize;
        if n > want {
            return Err("io: read callback returned more than the buffer size".into());
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

pub(crate) unsafe fn read_all_file(io: &SenaFileIo) -> Result<Vec<u8>, String> {
    let read = io.read.ok_or("io: read callback is NULL")?;
    unsafe { slurp(io.user_data, read) }
}

/// In-place rewrite strategy: compare old and new bytes and write only the
/// changed byte ranges (the Segment size patch, old user-Tags voided in
/// place, and the appended tail Tags), never rewriting unchanged clusters.
pub(crate) unsafe fn write_rewrite_tail(
    io: &SenaFileIo,
    original: &[u8],
    rewritten: &[u8],
) -> Result<(), String> {
    let write = io.write.ok_or("io: write callback is NULL")?;
    let seek = io
        .seek
        .ok_or("io: seek callback is required for tag editing")?;
    let mut i = 0usize;
    while i < rewritten.len() {
        if i < original.len() && original[i] == rewritten[i] {
            i += 1;
            continue;
        }
        let start = i;
        while i < rewritten.len() && (i >= original.len() || original[i] != rewritten[i]) {
            i += 1;
        }
        let chunk = &rewritten[start..i];
        if unsafe { seek(io.user_data, start as i64, WHENCE_SET) } < 0 {
            return Err(format!("io: seek to {start} failed"));
        }
        let mut off = 0usize;
        while off < chunk.len() {
            let n = unsafe {
                write(
                    io.user_data,
                    chunk[off..].as_ptr().cast(),
                    (chunk.len() - off) as u64,
                )
            };
            if n <= 0 {
                return Err(format!("io: write failed at {off} ({n})"));
            }
            off += n as usize;
        }
    }
    Ok(())
}

/// Builds a positioned reader over the host's io callbacks, or `None` when
/// the stream is not seekable (probed with a no-op reposition, WHENCE_CUR).
/// This is the building block of the bounded-memory paths: the container is
/// scanned via [`crate::demux::index_container`] and audio payloads are
/// fetched on demand instead of slurping the whole file.
pub(crate) fn range_reader(
    user: *mut std::ffi::c_void,
    read: RawReadFn,
    seek: RawSeekFn,
) -> Option<impl FnMut(u64, usize) -> Result<Vec<u8>, String>> {
    if unsafe { seek(user, 0, WHENCE_CUR) } < 0 {
        return None;
    }
    Some(move |pos: u64, len: usize| -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if unsafe { seek(user, pos as i64, WHENCE_SET) } < 0 {
            return Err(format!("io: seek to {pos} failed"));
        }
        let mut out = vec![0u8; len];
        let mut off = 0usize;
        while off < len {
            let n = unsafe { read(user, out[off..].as_mut_ptr().cast(), (len - off) as u64) };
            if n < 0 {
                return Err(format!("io: read failed ({n})"));
            }
            if n == 0 {
                break; // EOF: short buffer; callers decide if that is an error
            }
            let n = n as usize;
            if n > len - off {
                return Err("io: read callback returned more than requested".into());
            }
            off += n;
        }
        out.truncate(off);
        Ok(out)
    })
}

/// Write all of `data` at absolute `pos` through the io callbacks.
unsafe fn write_all_at(
    user: *mut std::ffi::c_void,
    write: RawWriteFn,
    seek: RawSeekFn,
    pos: u64,
    data: &[u8],
) -> Result<(), String> {
    if unsafe { seek(user, pos as i64, WHENCE_SET) } < 0 {
        return Err(format!("io: seek to {pos} failed"));
    }
    let mut off = 0usize;
    while off < data.len() {
        let n = unsafe { write(user, data[off..].as_ptr().cast(), (data.len() - off) as u64) };
        if n <= 0 {
            return Err(format!("io: write failed at {off} ({n})"));
        }
        off += n as usize;
    }
    Ok(())
}

/// Apply a rewrite plan through the write/seek callbacks. `Insert` ops only
/// ever target EOF (the caller checked the Segment ends at the file end), so
/// a positioned write is the correct primitive. Zero runs go out in chunks
/// to stay allocation-free.
unsafe fn apply_write_ops_io(
    user: *mut std::ffi::c_void,
    write: RawWriteFn,
    seek: RawSeekFn,
    ops: &[crate::demux::WriteOp],
) -> Result<(), String> {
    use crate::demux::WriteOp;
    const ZEROS: [u8; 16 * 1024] = [0u8; 16 * 1024];
    for op in ops {
        match op {
            WriteOp::Bytes { offset, data } | WriteOp::Insert { offset, data } => unsafe {
                write_all_at(user, write, seek, *offset, data)?;
            },
            WriteOp::Zero { offset, len } => {
                if unsafe { seek(user, *offset as i64, WHENCE_SET) } < 0 {
                    return Err(format!("io: seek to {offset} failed"));
                }
                let mut left = *len;
                while left > 0 {
                    let chunk = left.min(ZEROS.len() as u64);
                    let n = unsafe { write(user, ZEROS.as_ptr().cast(), chunk) };
                    if n <= 0 {
                        return Err(format!("io: write failed ({n})"));
                    }
                    left -= n as u64;
                }
            }
        }
    }
    Ok(())
}

/// Shared plan-then-write tag/attachment rewrite: scan the container through
/// positioned reads, apply the planned ops, verify the immutable tags. Falls
/// back to `buffered` (whole-file rewrite) when the stream is not seekable
/// or the Segment does not end exactly at EOF (trailing data would not
/// survive an append-as-write).
pub(crate) unsafe fn rewrite_via_plan<Plan>(
    io: &SenaFileIo,
    buffered: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
    plan: Plan,
) -> Result<(), String>
where
    Plan: FnOnce(&crate::demux::IndexedFile) -> Result<Vec<crate::demux::WriteOp>, String>,
{
    let read = io.read.ok_or("io: read callback is NULL")?;
    let write = io.write.ok_or("io: write callback is NULL")?;
    let prepared = io.seek.zip(io.size).and_then(|(seek, size)| {
        let mut ra = range_reader(io.user_data, read, seek)?;
        let scan = crate::demux::index_container(&mut ra, false).ok()?;
        let file_size = unsafe { size(io.user_data) };
        if file_size != scan.segment_payload_end {
            return None;
        }
        Some((scan, seek))
    });
    if let Some((scan, seek)) = prepared {
        let ops = plan(&scan)?;
        // A mid-file Insert (head-positioned Tags/Attachments) cannot be
        // applied through positioned writes; use the buffered whole-file
        // rewrite for it instead.
        let needs_buffered = ops.iter().any(|op| match op {
            crate::demux::WriteOp::Insert { offset, .. } => *offset != scan.segment_payload_end,
            _ => false,
        });
        if needs_buffered {
            if unsafe { seek(io.user_data, 0, WHENCE_SET) } < 0 {
                return Err("io: rewind failed".into());
            }
            let original = unsafe { read_all_file(io) }?;
            let rewritten = buffered(&original)?;
            return unsafe { write_rewrite_tail(io, &original, &rewritten) };
        }
        unsafe { apply_write_ops_io(io.user_data, write, seek, &ops) }?;
        let mut ra =
            range_reader(io.user_data, read, seek).ok_or("io: stream became unseekable")?;
        let check = crate::demux::index_container(&mut ra, false)
            .map_err(|e| format!("rewritten file invalid: {e}"))?;
        if check.init != scan.init {
            return Err("SENA-INIT changed after rewrite".into());
        }
        return Ok(());
    }
    let original = unsafe { read_all_file(io) }?;
    let rewritten = buffered(&original)?;
    unsafe { write_rewrite_tail(io, &original, &rewritten) }
}
