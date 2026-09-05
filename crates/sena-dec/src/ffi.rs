//! C-compatible ABI (`sena_dec_*` / `sena_file_*`) for plugin hosts.

use std::ffi::{CStr, c_char, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use crate::demux::Demuxed;
use crate::stream::StreamingDecoder;
use crate::tags::rewrite_user_tags;

pub const SENA_DEC_ABI_VERSION: u32 = 1;
pub const SENA_DEC_OK: i32 = 0;
pub const SENA_DEC_ERR_IO: i32 = -1;
pub const SENA_DEC_ERR_FORMAT: i32 = -2;
pub const SENA_DEC_ERR_UNSUPPORTED: i32 = -3;
pub const SENA_DEC_ERR_DECODE: i32 = -4;
pub const SENA_DEC_ERR_INVALID: i32 = -5;
pub const SENA_DEC_EOF: i32 = -6;

/// Album art is a few MB in practice; beyond this the container is corrupt.
const MAX_ART_ELEMENT_BYTES: u64 = 64 * 1024 * 1024;

pub type IoRead = Option<unsafe extern "C" fn(*mut c_void, *mut c_void, u64) -> i64>;
pub type IoWrite = Option<unsafe extern "C" fn(*mut c_void, *const c_void, u64) -> i64>;
pub type IoSeek = Option<unsafe extern "C" fn(*mut c_void, i64, i32) -> i64>;
pub type IoTell = Option<unsafe extern "C" fn(*mut c_void) -> i64>;
pub type IoSize = Option<unsafe extern "C" fn(*mut c_void) -> u64>;

#[repr(C)]
pub struct SenaDecIo {
    pub user_data: *mut c_void,
    pub read: IoRead,
    pub seek: IoSeek,
    pub tell: IoTell,
    pub size: IoSize,
}

#[repr(C)]
pub struct SenaFileIo {
    pub user_data: *mut c_void,
    pub read: IoRead,
    pub write: IoWrite,
    pub seek: IoSeek,
    pub tell: IoTell,
    pub size: IoSize,
}

#[repr(C)]
pub struct SenaDecInfo {
    pub sample_rate: u32,
    pub channels: u32,
    pub playable_frames: u64,
    pub profile: u32,
    pub sena_version: u32,
    /// SENA_AUDIO_SHA256, hash of the encoded Opus + xHE-AAC streams
    /// (64 hex chars + NUL); empty for files without it.
    pub audio_sha256: [c_char; 65],
}

#[repr(C)]
pub struct SenaDecReadInfo {
    pub start_frame: u64,
    pub frames: u64,
    pub payload_bits: u64,
}

#[repr(C)]
pub struct SenaMetaEntry {
    pub key: *const c_char,
    pub value: *const c_char,
}

pub struct SenaDecHandle {
    decoder: StreamingDecoder,
}

/// Album art (Matroska Attachments) access via the same callback file IO.
pub struct SenaArtHandle {
    names: Vec<Vec<u8>>,
    mimes: Vec<Vec<u8>>,
    datas: Vec<Vec<u8>>,
}

#[repr(C)]
pub struct SenaArtInput {
    pub name: *const c_char,
    pub mime: *const c_char,
    pub data: *const u8,
    pub data_len: usize,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_art_read(
    io: *const SenaFileIo,
    out: *mut *mut SenaArtHandle,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<*mut SenaArtHandle, String> {
        if io.is_null() || out.is_null() {
            return Err("invalid argument: NULL io/out".into());
        }
        let io = unsafe { &*io };
        let read = io.read.ok_or("io: read callback is NULL")?;
        let arts = match io.seek.and_then(|seek| range_reader(io.user_data, read, seek)) {
            // Range-read only the Attachments elements; the audio payload is
            // never touched.
            Some(mut read_at) => {
                let indexed = crate::demux::index_container(&mut read_at, false)?;
                let mut arts = Vec::new();
                for &(_, payload_start, end) in &indexed.attachment_ranges {
                    let len = end - payload_start;
                    if len > MAX_ART_ELEMENT_BYTES {
                        return Err(format!(
                            "unsupported: Attachments element of {len} bytes exceeds the {MAX_ART_ELEMENT_BYTES}-byte sanity cap"
                        ));
                    }
                    let payload = read_at(payload_start, len as usize)?;
                    if payload.len() != len as usize {
                        return Err("io: truncated Attachments element".into());
                    }
                    arts.extend(crate::attachments::parse_attachments_payload(&payload));
                }
                arts
            }
            None => {
                let bytes = unsafe { read_all_file(io) }?;
                let demux = Demuxed::parse(bytes).map_err(|e| e.to_string())?;
                crate::attachments::parse_attachments(&demux)
            }
        };
        let mut names = Vec::new();
        let mut mimes = Vec::new();
        let mut datas = Vec::new();
        for a in arts {
            names.push(cstr_vec(&a.name));
            mimes.push(cstr_vec(&a.mime));
            datas.push(a.data);
        }
        Ok(Box::into_raw(Box::new(SenaArtHandle { names, mimes, datas })))
    }));
    match result {
        Ok(Ok(h)) => {
            unsafe { *out = h };
            SENA_DEC_OK
        }
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_count(tags: *const SenaArtHandle) -> u32 {
    if tags.is_null() {
        return 0;
    }
    let t = unsafe { &*tags };
    t.names.len() as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_name(tags: *const SenaArtHandle, index: u32) -> *const c_char {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.names.get(index as usize).map(|v| v.as_ptr().cast()).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_mime(tags: *const SenaArtHandle, index: u32) -> *const c_char {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.mimes.get(index as usize).map(|v| v.as_ptr().cast()).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_data(tags: *const SenaArtHandle, index: u32) -> *const u8 {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.datas.get(index as usize).map(|v| v.as_ptr()).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_data_len(tags: *const SenaArtHandle, index: u32) -> usize {
    if tags.is_null() {
        return 0;
    }
    let t = unsafe { &*tags };
    t.datas.get(index as usize).map(|v| v.len()).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_close(tags: *mut SenaArtHandle) {
    if !tags.is_null() {
        unsafe { drop(Box::from_raw(tags)) };
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_art_write(
    io: *const SenaFileIo,
    entries: *const SenaArtInput,
    count: u32,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        if io.is_null() {
            return Err("invalid argument: NULL io".into());
        }
        let mut arts = Vec::new();
        if count > 0 {
            if entries.is_null() {
                return Err("invalid argument: NULL entries".into());
            }
            let slice = unsafe { std::slice::from_raw_parts(entries, count as usize) };
            for e in slice {
                if e.name.is_null() || e.mime.is_null() || (e.data.is_null() && e.data_len > 0) {
                    return Err("invalid argument: NULL art entry field".into());
                }
                let name = unsafe { CStr::from_ptr(e.name) }.to_string_lossy().into_owned();
                let mime = unsafe { CStr::from_ptr(e.mime) }.to_string_lossy().into_owned();
                if name.is_empty() {
                    return Err("invalid argument: empty art name".into());
                }
                let data = if e.data_len == 0 {
                    Vec::new()
                } else {
                    unsafe { std::slice::from_raw_parts(e.data, e.data_len) }.to_vec()
                };
                arts.push(crate::attachments::Attachment::new(&name, &mime, data));
            }
        }
        let io = unsafe { &*io };
        unsafe {
            rewrite_via_plan(
                io,
                |original| crate::attachments::rewrite_attachments(original, &arts),
                |scan| crate::attachments::plan_attachment_rewrite(scan, &arts),
            )
        }
    }));
    match result {
        Ok(Ok(())) => SENA_DEC_OK,
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

pub struct SenaTagsHandle {
    keys: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
}

fn write_err(err: *mut c_char, err_len: usize, msg: &str) {
    if err.is_null() || err_len == 0 {
        return;
    }
    let bytes = msg.as_bytes();
    let n = bytes.len().min(err_len - 1);
    unsafe {
        ptr::copy_nonoverlapping(bytes.as_ptr(), err.cast::<u8>(), n);
        *err.add(n) = 0;
    }
}

fn error_code<T>(e: &T) -> i32
where
    T: std::fmt::Display,
{
    let s = e.to_string();
    if s.starts_with("io:") {
        SENA_DEC_ERR_IO
    } else if s.starts_with("unsupported:") || s.starts_with("unsupported container") {
        SENA_DEC_ERR_UNSUPPORTED
    } else if s.starts_with("codec:") {
        SENA_DEC_ERR_DECODE
    } else {
        SENA_DEC_ERR_FORMAT
    }
}

unsafe fn read_all(io: &SenaDecIo) -> Result<Vec<u8>, String> {
    let read = io.read.ok_or("io: read callback is NULL")?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = unsafe { read(io.user_data, buf.as_mut_ptr().cast(), buf.len() as u64) };
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

unsafe fn read_file_at(io: &SenaFileIo, pos: u64, len: usize) -> Result<Vec<u8>, String> {
    if len == 0 {
        return Ok(Vec::new());
    }
    let read = io.read.ok_or("io: read callback is NULL")?;
    let seek = io.seek.ok_or("io: seek callback is required for range reads")?;
    if unsafe { seek(io.user_data, pos as i64, 0) } < 0 {
        return Err(format!("io: seek to {pos} failed"));
    }
    let mut out = Vec::with_capacity(len);
    let mut chunk = vec![0u8; len.min(64 * 1024)];
    while out.len() < len {
        let want = (len - out.len()).min(chunk.len());
        let n = unsafe { read(io.user_data, chunk.as_mut_ptr().cast(), want as u64) };
        if n < 0 {
            return Err(format!("io: read failed ({n})"));
        }
        if n == 0 {
            return Err(format!("io: truncated range read at {} (wanted {want})", pos + out.len() as u64));
        }
        let n = n as usize;
        if n > want {
            return Err("io: read callback returned more than the buffer size".into());
        }
        out.extend_from_slice(&chunk[..n]);
    }
    Ok(out)
}

unsafe fn read_all_file(io: &SenaFileIo) -> Result<Vec<u8>, String> {
    let read = io.read.ok_or("io: read callback is NULL")?;
    let mut out = Vec::new();
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = unsafe { read(io.user_data, buf.as_mut_ptr().cast(), buf.len() as u64) };
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

/// In-place rewrite strategy: compare old and new bytes and write only the
/// changed byte ranges (the Segment size patch, old user-Tags voided in
/// place, and the appended tail Tags), never rewriting unchanged clusters.
unsafe fn write_rewrite_tail(io: &SenaFileIo, original: &[u8], rewritten: &[u8]) -> Result<(), String> {
    let write = io.write.ok_or("io: write callback is NULL")?;
    let seek = io.seek.ok_or("io: seek callback is required for tag editing")?;
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
        if unsafe { seek(io.user_data, start as i64, 0) } < 0 {
            return Err(format!("io: seek to {start} failed"));
        }
        let mut off = 0usize;
        while off < chunk.len() {
            let n = unsafe { write(io.user_data, chunk[off..].as_ptr().cast(), (chunk.len() - off) as u64) };
            if n <= 0 {
                return Err(format!("io: write failed at {off} ({n})"));
            }
            off += n as usize;
        }
    }
    Ok(())
}

fn map_decode_error(e: &crate::pipeline::DecodeError) -> i32 {
    match e {
        crate::pipeline::DecodeError::Unsupported(_) => SENA_DEC_ERR_UNSUPPORTED,
        crate::pipeline::DecodeError::Codec(_) => SENA_DEC_ERR_DECODE,
        crate::pipeline::DecodeError::Io(_) => SENA_DEC_ERR_IO,
        _ => SENA_DEC_ERR_FORMAT,
    }
}

type RawReadFn = unsafe extern "C" fn(*mut c_void, *mut c_void, u64) -> i64;
type RawWriteFn = unsafe extern "C" fn(*mut c_void, *const c_void, u64) -> i64;
type RawSeekFn = unsafe extern "C" fn(*mut c_void, i64, i32) -> i64;

/// Builds a positioned reader over the host's io callbacks, or `None` when
/// the stream is not seekable (probed with a no-op reposition; whence 1 =
/// current position, matching foobar2000's `file::t_seek_mode`). This is the
/// building block of the bounded-memory paths: the container is scanned via
/// [`crate::demux::index_container`] and audio payloads are fetched on
/// demand instead of slurping the whole file.
fn range_reader(
    user: *mut c_void,
    read: RawReadFn,
    seek: RawSeekFn,
) -> Option<impl FnMut(u64, usize) -> Result<Vec<u8>, String>> {
    if unsafe { seek(user, 0, 1) } < 0 {
        return None;
    }
    Some(move |pos: u64, len: usize| -> Result<Vec<u8>, String> {
        if len == 0 {
            return Ok(Vec::new());
        }
        if unsafe { seek(user, pos as i64, 0) } < 0 {
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
    user: *mut c_void,
    write: RawWriteFn,
    seek: RawSeekFn,
    pos: u64,
    data: &[u8],
) -> Result<(), String> {
    if unsafe { seek(user, pos as i64, 0) } < 0 {
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
    user: *mut c_void,
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
                if unsafe { seek(user, *offset as i64, 0) } < 0 {
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
unsafe fn rewrite_via_plan<Plan>(
    io: &SenaFileIo,
    buffered: impl FnOnce(&[u8]) -> Result<Vec<u8>, String>,
    plan: Plan,
) -> Result<(), String>
where
    Plan: FnOnce(&crate::demux::IndexedFile) -> Result<Vec<crate::demux::WriteOp>, String>,
{
    let read = io.read.ok_or("io: read callback is NULL")?;
    let write = io.write.ok_or("io: write callback is NULL")?;
    let prepared = io
        .seek
        .zip(io.size)
        .and_then(|(seek, size)| {
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
        unsafe { apply_write_ops_io(io.user_data, write, seek, &ops) }?;
        let mut ra = range_reader(io.user_data, read, seek).ok_or("io: stream became unseekable")?;
        let check = crate::demux::index_container(&mut ra, false)
            .map_err(|e| format!("rewritten file invalid: {e}"))?;
        return crate::demux::check_immutable_survived(&scan, &check);
    }
    let original = unsafe { read_all_file(io) }?;
    let rewritten = buffered(&original)?;
    unsafe { write_rewrite_tail(io, &original, &rewritten) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_open(
    io: *const SenaDecIo,
    out: *mut *mut SenaDecHandle,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| {
        if io.is_null() || out.is_null() {
            return Err("invalid argument: NULL io/out".to_string());
        }
        let io = unsafe { &*io };
        let read = io.read.ok_or("io: read callback is NULL")?;
        let decoder = match io.seek.and_then(|seek| range_reader(io.user_data, read, seek)) {
            // Seekable input: index the container, fetch frame payloads
            // lazily - per-instance memory stays bounded no matter the file
            // size (32-bit foobar2000 has a 2 GiB address space and runs
            // several decode threads concurrently for ReplayGain scans).
            Some(mut read_at) => {
                let mut indexed = crate::demux::index_container(&mut read_at, true)?;
                let frames = std::mem::take(&mut indexed.frames);
                let store = crate::stream::FrameStore::Lazy { frames, read_at: Box::new(read_at), window: (0, Vec::new()) };
                StreamingDecoder::open_indexed(indexed, store).map_err(|e| e.to_string())?
            }
            // Non-seekable input: one pass into memory, as before.
            None => {
                let bytes = unsafe { read_all(io) }?;
                let demux = Demuxed::parse(bytes).map_err(|e| e.to_string())?;
                StreamingDecoder::open(demux).map_err(|e| e.to_string())?
            }
        };
        unsafe { *out = Box::into_raw(Box::new(SenaDecHandle { decoder })) };
        Ok(())
    }));
    match result {
        Ok(Ok(())) => SENA_DEC_OK,
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_close(dec: *mut SenaDecHandle) {
    if !dec.is_null() {
        unsafe { drop(Box::from_raw(dec)) };
    }
}

/// Runs `f` and converts an escaping panic into `panic_code` instead of
/// aborting the host process: an unwind crossing an `extern "C"` boundary
/// kills the whole player instantly, with the panic message lost on stderr
/// (invisible in a GUI host). The decode-path entry points below are the
/// hottest and most complex code in the crate, so they must never unwind.
fn guard<T>(panic_code: i32, f: impl FnOnce() -> Result<T, i32>) -> Result<T, i32> {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r,
        Err(_) => Err(panic_code),
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_get_info(
    dec: *mut SenaDecHandle,
    info: *mut SenaDecInfo,
) -> i32 {
    if dec.is_null() || info.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*dec };
        let i = d.decoder.info();
        let mut sha = [0 as c_char; 65];
        for (k, c) in i.audio_sha256.as_bytes().iter().take(64).enumerate() {
            sha[k] = *c as c_char;
        }
        unsafe { *info = SenaDecInfo { sample_rate: i.sample_rate, channels: i.channels, playable_frames: i.playable_frames, profile: i.profile, sena_version: i.sena_version, audio_sha256: sha } };
        Ok(SENA_DEC_OK)
    })
    .unwrap_or_else(|e| e)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_read_f32(
    dec: *mut SenaDecHandle,
    interleaved: *mut f32,
    frames: u64,
    out_frames: *mut u64,
) -> i32 {
    if dec.is_null() || interleaved.is_null() || out_frames.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &mut *dec };
        let want = frames as usize;
        let n2 = match want.checked_mul(2) {
            Some(n) => n,
            None => return Err(SENA_DEC_ERR_INVALID),
        };
        let slice = unsafe { std::slice::from_raw_parts_mut(interleaved, n2) };
        match d.decoder.read_f32(slice, frames) {
            Ok(n) => {
                unsafe { *out_frames = n };
                Ok(if n == 0 { SENA_DEC_EOF } else { SENA_DEC_OK })
            }
            Err(e) => Err(map_decode_error(&e)),
        }
    })
    .unwrap_or_else(|e| e)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_get_read_info(
    dec: *mut SenaDecHandle,
    info: *mut SenaDecReadInfo,
) -> i32 {
    if dec.is_null() || info.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*dec };
        let i = d.decoder.read_info();
        unsafe { *info = SenaDecReadInfo { start_frame: i.start_frame, frames: i.frames, payload_bits: i.payload_bits } };
        Ok(SENA_DEC_OK)
    })
    .unwrap_or_else(|e| e)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_seek(dec: *mut SenaDecHandle, frame: u64) -> i32 {
    if dec.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &mut *dec };
        match d.decoder.seek(frame) {
            Ok(()) => Ok(SENA_DEC_OK),
            Err(e) => Err(map_decode_error(&e)),
        }
    })
    .unwrap_or_else(|e| e)
}

#[unsafe(no_mangle)]
pub extern "C" fn sena_dec_version() -> *const c_char {
    b"sena-dec 0.1.0 (ABI 1)\0".as_ptr().cast()
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_probe_info(
    io: *const SenaDecIo,
    info: *mut SenaDecInfo,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<SenaDecInfo, String> {
        if io.is_null() || info.is_null() {
            return Err("invalid argument: NULL io/info".into());
        }
        let io = unsafe { &*io };
        let read = io.read.ok_or("io: read callback is NULL")?;
        let (probe, _warnings) = match io.seek.and_then(|seek| range_reader(io.user_data, read, seek)) {
            // Head-only scan: tracks + immutable tags, cluster payloads and
            // even the frame index are skipped.
            Some(mut read_at) => {
                let indexed = crate::demux::index_container(&mut read_at, false)?;
                crate::pipeline::probe(&indexed).map_err(|e| e.to_string())?
            }
            None => {
                let bytes = unsafe { read_all(io) }?;
                let demux = Demuxed::parse(bytes).map_err(|e| e.to_string())?;
                crate::pipeline::probe(&demux).map_err(|e| e.to_string())?
            }
        };
        let mut sha = [0 as c_char; 65];
        for (k, c) in probe.audio_sha256.as_bytes().iter().take(64).enumerate() {
            sha[k] = *c as c_char;
        }
        Ok(SenaDecInfo { sample_rate: probe.sample_rate, channels: probe.channels, playable_frames: probe.playable_frames, profile: probe.profile, sena_version: probe.sena_version, audio_sha256: sha })
    }));
    match result {
        Ok(Ok(i)) => {
            unsafe { *info = i };
            SENA_DEC_OK
        }
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

unsafe fn copy_meta_entries(entries: *const SenaMetaEntry, count: u32) -> Result<Vec<(String, String)>, String> {
    if count == 0 {
        return Ok(Vec::new());
    }
    if entries.is_null() {
        return Err("invalid argument: NULL entries".into());
    }
    let slice = unsafe { std::slice::from_raw_parts(entries, count as usize) };
    let mut out = Vec::with_capacity(count as usize);
    for e in slice {
        if e.key.is_null() || e.value.is_null() {
            return Err("invalid argument: NULL metadata key/value".into());
        }
        let k = unsafe { CStr::from_ptr(e.key) }.to_string_lossy().into_owned();
        let v = unsafe { CStr::from_ptr(e.value) }.to_string_lossy().into_owned();
        if k.is_empty() {
            return Err("invalid argument: empty metadata key".into());
        }
        out.push((k, v));
    }
    Ok(out)
}

unsafe fn file_rewrite(io: *const SenaFileIo, entries: &[(String, String)], err: *mut c_char, err_len: usize) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        if io.is_null() {
            return Err("invalid argument: NULL io".into());
        }
        let io = unsafe { &*io };
        unsafe {
            rewrite_via_plan(
                io,
                |original| rewrite_user_tags(original, entries),
                |scan| crate::tags::plan_user_tag_rewrite(scan, entries),
            )
        }
    }));
    match result {
        Ok(Ok(())) => SENA_DEC_OK,
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_write_tags(
    io: *const SenaFileIo,
    entries: *const SenaMetaEntry,
    count: u32,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let entries = match unsafe { copy_meta_entries(entries, count) } {
        Ok(e) => e,
        Err(e) => {
            write_err(err, err_len, &e);
            return SENA_DEC_ERR_INVALID;
        }
    };
    unsafe { file_rewrite(io, &entries, err, err_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_remove_tags(
    io: *const SenaFileIo,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    unsafe { file_rewrite(io, &[], err, err_len) }
}

fn cstr_vec(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_read_tags(
    io: *const SenaFileIo,
    out: *mut *mut SenaTagsHandle,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<*mut SenaTagsHandle, String> {
        if io.is_null() || out.is_null() {
            return Err("invalid argument: NULL io/out".into());
        }
        let io = unsafe { &*io };
        let mut read_at = |pos: u64, len: usize| unsafe { read_file_at(io, pos, len) };
        let entries = crate::tags::scan_user_tags(&mut read_at)?;
        let mut keys = Vec::new();
        let mut values = Vec::new();
        for (k, v) in entries {
            keys.push(cstr_vec(&k));
            values.push(cstr_vec(&v));
        }
        let handle = Box::new(SenaTagsHandle { keys, values });
        Ok(Box::into_raw(handle))
    }));
    match result {
        Ok(Ok(handle)) => {
            unsafe { *out = handle };
            SENA_DEC_OK
        }
        Ok(Err(e)) => {
            write_err(err, err_len, &e);
            error_code(&e)
        }
        Err(_) => {
            write_err(err, err_len, "internal panic");
            SENA_DEC_ERR_DECODE
        }
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_count(tags: *const SenaTagsHandle) -> u32 {
    if tags.is_null() {
        return 0;
    }
    let t = unsafe { &*tags };
    t.keys.len() as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_key(tags: *const SenaTagsHandle, index: u32) -> *const c_char {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.keys.get(index as usize).map(|v| v.as_ptr().cast()).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_value(tags: *const SenaTagsHandle, index: u32) -> *const c_char {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.values.get(index as usize).map(|v| v.as_ptr().cast()).unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_close(tags: *mut SenaTagsHandle) {
    if !tags.is_null() {
        unsafe { drop(Box::from_raw(tags)) };
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::UnsafeCell;

    struct MemFile {
        data: UnsafeCell<Vec<u8>>,
        pos: UnsafeCell<usize>,
    }

    impl MemFile {
        fn new(data: Vec<u8>) -> Self {
            Self { data: UnsafeCell::new(data), pos: UnsafeCell::new(0) }
        }
    }

    unsafe extern "C" fn mem_read(user: *mut c_void, buf: *mut c_void, len: u64) -> i64 {
        let f = unsafe { &*(user as *const MemFile) };
        let data = unsafe { &*f.data.get() };
        let pos = unsafe { &mut *f.pos.get() };
        let n = (len as usize).min(data.len().saturating_sub(*pos));
        unsafe { std::ptr::copy_nonoverlapping(data[*pos..*pos + n].as_ptr(), buf.cast::<u8>(), n) };
        *pos += n;
        n as i64
    }

    unsafe extern "C" fn mem_write(user: *mut c_void, buf: *const c_void, len: u64) -> i64 {
        let f = unsafe { &*(user as *const MemFile) };
        let data = unsafe { &mut *f.data.get() };
        let pos = unsafe { &mut *f.pos.get() };
        let src = unsafe { std::slice::from_raw_parts(buf.cast::<u8>(), len as usize) };
        if data.len() < *pos + src.len() {
            data.resize(*pos + src.len(), 0);
        }
        data[*pos..*pos + src.len()].copy_from_slice(src);
        *pos += src.len();
        len as i64
    }

    unsafe extern "C" fn mem_seek(user: *mut c_void, offset: i64, _whence: i32) -> i64 {
        let f = unsafe { &*(user as *const MemFile) };
        let pos = unsafe { &mut *f.pos.get() };
        if offset < 0 {
            return -1;
        }
        *pos = offset as usize;
        0
    }

    unsafe extern "C" fn mem_size(user: *mut c_void) -> u64 {
        let f = unsafe { &*(user as *const MemFile) };
        unsafe { &*f.data.get() }.len() as u64
    }

    fn open_mem(path: &str) -> MemFile {
        let full = format!("{}/../../{}", env!("CARGO_MANIFEST_DIR"), path);
        MemFile::new(std::fs::read(full).unwrap())
    }

    #[test]
    fn error_code_mapping() {
        assert_eq!(error_code(&"unsupported: x".to_string()), SENA_DEC_ERR_UNSUPPORTED);
        assert_eq!(error_code(&"io: x".to_string()), SENA_DEC_ERR_IO);
    }

    #[test]
    fn abi_open_read_seek_and_dynamic_info() {
        let mem = open_mem("assets/e2e/01__p300__lfa__hf144.sena");
        let io = SenaDecIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            seek: Some(mem_seek),
            tell: None,
            size: None,
        };
        let mut handle: *mut SenaDecHandle = std::ptr::null_mut();
        let mut err = [0u8; 256];
        assert_eq!(unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        assert!(!handle.is_null());
        let mut info = SenaDecInfo { sample_rate: 0, channels: 0, playable_frames: 0, profile: 0, sena_version: 0, audio_sha256: [0; 65] };
        assert_eq!(unsafe { sena_dec_get_info(handle, &mut info) }, SENA_DEC_OK);
        assert_eq!((info.sample_rate, info.channels, info.playable_frames, info.profile), (48000, 2, 960000, 300));

        let mut buf = vec![0.0f32; 4800 * 2];
        let mut got = 0u64;
        assert_eq!(unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) }, SENA_DEC_OK);
        assert_eq!(got, 4800);
        let mut ri = SenaDecReadInfo { start_frame: 0, frames: 0, payload_bits: 0 };
        assert_eq!(unsafe { sena_dec_get_read_info(handle, &mut ri) }, SENA_DEC_OK);
        assert_eq!(ri.frames, 4800);
        assert!(ri.payload_bits > 0);

        assert_eq!(unsafe { sena_dec_seek(handle, 959_900) }, SENA_DEC_OK);
        assert_eq!(unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) }, SENA_DEC_OK);
        assert_eq!(got, 100);
        assert_eq!(unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) }, SENA_DEC_EOF);
        assert_eq!(got, 0);
        unsafe { sena_dec_close(handle) };
    }

    #[test]
    fn abi_tag_rewrite() {
        let mem = open_mem("assets/e2e/01__p300__lfa__hf144.sena");
        let io = SenaFileIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            write: Some(mem_write),
            seek: Some(mem_seek),
            tell: None,
            size: None,
        };
        let key = b"TITLE\0".as_ptr().cast();
        let value = b"test song\0".as_ptr().cast();
        let entries = [SenaMetaEntry { key, value }];
        let mut err = [0u8; 256];
        assert_eq!(unsafe { sena_file_write_tags(&io, entries.as_ptr(), 1, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        let data = unsafe { &*mem.data.get() };
        let demux = Demuxed::parse(data.clone()).unwrap();
        assert_eq!(demux.tags.last().unwrap().get("TITLE"), Some("test song"));
        assert_eq!(demux.immutable_tag("SENA_PROFILE"), Some("300"));

        // The C ABI read path now scans EBML headers with seek+read and must
        // see the freshly appended tail Tags without reading the clusters.
        let mut handle: *mut SenaTagsHandle = std::ptr::null_mut();
        assert_eq!(unsafe { sena_file_read_tags(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        assert_eq!(unsafe { sena_tags_count(handle) }, 1);
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(sena_tags_key(handle, 0)) }.to_bytes(), b"TITLE");
        assert_eq!(unsafe { std::ffi::CStr::from_ptr(sena_tags_value(handle, 0)) }.to_bytes(), b"test song");
        unsafe { sena_tags_close(handle) };
    }

    /// Same as `abi_tag_rewrite`, but with a working `size` callback so the
    /// write goes through the positioned-write plan (no whole-file copies).
    #[test]
    fn abi_tag_rewrite_planned_ops() {
        let mem = open_mem("assets/e2e/01__p300__lfa__hf144.sena");
        let before_len = unsafe { &*mem.data.get() }.len();
        let io = SenaFileIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            write: Some(mem_write),
            seek: Some(mem_seek),
            tell: None,
            size: Some(mem_size),
        };
        let key = c"TITLE".as_ptr();
        let value = c"test song".as_ptr();
        let entries = [SenaMetaEntry { key, value }];
        let mut err = [0u8; 256];
        assert_eq!(unsafe { sena_file_write_tags(&io, entries.as_ptr(), 1, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        let data = unsafe { &*mem.data.get() };
        // The file grew by exactly the appended tail Tags element; everything
        // else was written in place.
        assert!(data.len() > before_len);
        let demux = Demuxed::parse(data.clone()).unwrap();
        assert_eq!(demux.tags.last().unwrap().get("TITLE"), Some("test song"));
        assert_eq!(demux.immutable_tag("SENA_PROFILE"), Some("300"));
        // Clusters untouched: a full decode still yields the same length.
        let dec = crate::pipeline::Decoder::open(&demux).unwrap();
        assert_eq!(dec.info().playable_frames, 960000);
    }

    /// `sena_dec_probe_info` over a seekable stream uses the bounded head
    /// scan and returns the same info as the decode handle.
    #[test]
    fn abi_probe_info_head_scan() {
        let mem = open_mem("assets/e2e/01__p300__lfa__hf144.sena");
        let io = SenaDecIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            seek: Some(mem_seek),
            tell: None,
            size: Some(mem_size),
        };
        let mut info = SenaDecInfo { sample_rate: 0, channels: 0, playable_frames: 0, profile: 0, sena_version: 0, audio_sha256: [0; 65] };
        let mut err = [0u8; 256];
        assert_eq!(unsafe { sena_dec_probe_info(&io, &mut info, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        assert_eq!((info.sample_rate, info.channels, info.playable_frames, info.profile), (48000, 2, 960000, 300));
    }

    /// The lazy io path must be bit-exact with the in-memory store: same
    /// streaming decoder, same bytes, only the payload source differs. (The
    /// separate whole-file `pipeline::Decoder` is NOT bit-identical to the
    /// streaming decoder by design - the zero-phase resampler sees different
    /// context - so it is not a reference here.)
    #[test]
    fn abi_lazy_decode_matches_reference() {
        let bytes = std::fs::read(format!("{}/../../assets/e2e/01__p300__lfa__hf144.sena", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let reference = {
            let demux = Demuxed::parse(bytes.clone()).unwrap();
            let mut dec = crate::stream::StreamingDecoder::open(demux).unwrap();
            let mut pcm = Vec::new();
            let mut buf = vec![0.0f32; 4096 * 2];
            loop {
                let n = dec.read_f32(&mut buf, 4096).unwrap();
                if n == 0 {
                    break;
                }
                pcm.extend_from_slice(&buf[..n as usize * 2]);
            }
            pcm
        };

        let mem = MemFile::new(bytes);
        let io = SenaDecIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            seek: Some(mem_seek),
            tell: None,
            size: Some(mem_size),
        };
        let mut handle: *mut SenaDecHandle = std::ptr::null_mut();
        let mut err = [0u8; 256];
        assert_eq!(unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
        let mut lazy = Vec::new();
        let mut buf = vec![0.0f32; 4096 * 2];
        loop {
            let mut got = 0u64;
            let rc = unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4096, &mut got) };
            assert!(rc == SENA_DEC_OK || rc == SENA_DEC_EOF);
            lazy.extend_from_slice(&buf[..got as usize * 2]);
            if rc == SENA_DEC_EOF {
                break;
            }
        }
        unsafe { sena_dec_close(handle) };
        assert_eq!(lazy.len(), reference.len());
        assert_eq!(lazy, reference);
    }

    /// Mirrors the foobar2000 ReplayGain batch scanner: many threads each
    /// run an independent decoder over the lazy io path to EOF.
    #[test]
    fn abi_concurrent_decode() {
        let bytes = std::fs::read(format!("{}/../../assets/e2e/01__p300__lfa__hf144.sena", env!("CARGO_MANIFEST_DIR"))).unwrap();
        let mut handles = Vec::new();
        for _ in 0..8 {
            let bytes = bytes.clone();
            handles.push(std::thread::spawn(move || {
                let mem = MemFile::new(bytes);
                let io = SenaDecIo {
                    user_data: (&mem as *const MemFile).cast_mut().cast(),
                    read: Some(mem_read),
                    seek: Some(mem_seek),
                    tell: None,
                    size: Some(mem_size),
                };
                let mut handle: *mut SenaDecHandle = std::ptr::null_mut();
                let mut err = [0u8; 256];
                assert_eq!(unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) }, SENA_DEC_OK);
                let mut buf = vec![0.0f32; 4096 * 2];
                let mut total = 0u64;
                loop {
                    let mut got = 0u64;
                    let rc = unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4096, &mut got) };
                    assert!(rc == SENA_DEC_OK || rc == SENA_DEC_EOF);
                    total += got;
                    if rc == SENA_DEC_EOF {
                        break;
                    }
                }
                assert_eq!(total, 960000);
                unsafe { sena_dec_close(handle) };
            }));
        }
        for h in handles {
            h.join().unwrap();
        }
    }
}
