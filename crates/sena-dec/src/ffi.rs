//! C-compatible ABI (`sena_dec_*` / `sena_file_*` / `sena_es_*`) for plugin
//! hosts. The entry points live in submodules by API family; this root owns
//! the ABI types, the status codes, and the shared glue (error delivery,
//! panic guards, handle teardown).

use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

mod art;
mod dec;
mod es_push;
mod io;
mod tags;

pub use art::*;
pub use dec::*;
pub use es_push::*;
pub use tags::*;

pub const SENA_DEC_ABI_VERSION: u32 = 3;
pub const SENA_DEC_OK: i32 = 0;
pub const SENA_DEC_ERR_IO: i32 = -1;
pub const SENA_DEC_ERR_FORMAT: i32 = -2;
pub const SENA_DEC_ERR_UNSUPPORTED: i32 = -3;
pub const SENA_DEC_ERR_DECODE: i32 = -4;
pub const SENA_DEC_ERR_INVALID: i32 = -5;
pub const SENA_DEC_EOF: i32 = -6;

/// Hex digits of the audio SHA-256 carried by [`SenaDecInfo`]
/// (the field adds one NUL).
pub const SENA_SHA256_HEX_LEN: usize = 64;

/// The f32 read ABI is stereo-interleaved.
pub(crate) const INTERLEAVED_CHANNELS: usize = 2;

/// `seek` whence values, matching foobar2000's `file::t_seek_mode`.
pub(crate) const WHENCE_SET: i32 = 0;
pub(crate) const WHENCE_CUR: i32 = 1;

// The C info struct exposes the mid codec as the INIT byte values.
const _: () = {
    assert!(sena_core::MidCodec::Opus as u32 == 0);
    assert!(sena_core::MidCodec::Xhe as u32 == 1);
};

pub type IoRead = Option<RawReadFn>;
pub type IoWrite = Option<RawWriteFn>;
pub type IoSeek = Option<RawSeekFn>;
pub type IoTell = Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> i64>;
pub type IoSize = Option<unsafe extern "C" fn(*mut std::ffi::c_void) -> u64>;

pub(crate) type RawReadFn = unsafe extern "C" fn(*mut std::ffi::c_void, *mut std::ffi::c_void, u64) -> i64;
pub(crate) type RawWriteFn =
    unsafe extern "C" fn(*mut std::ffi::c_void, *const std::ffi::c_void, u64) -> i64;
pub(crate) type RawSeekFn = unsafe extern "C" fn(*mut std::ffi::c_void, i64, i32) -> i64;

#[repr(C)]
pub struct SenaDecIo {
    pub user_data: *mut std::ffi::c_void,
    pub read: IoRead,
    pub seek: IoSeek,
    /// Reserved; currently unused.
    pub tell: IoTell,
    pub size: IoSize,
}

#[repr(C)]
pub struct SenaFileIo {
    pub user_data: *mut std::ffi::c_void,
    pub read: IoRead,
    pub write: IoWrite,
    pub seek: IoSeek,
    /// Reserved; currently unused.
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
    pub audio_sha256: [c_char; SENA_SHA256_HEX_LEN + 1],
    /// Encoded size of all Cluster elements (codec payloads plus block
    /// framing). Tags, Attachments (cover art) and Void filler are excluded,
    /// so hosts derive the average bitrate from this, not the file size.
    pub audio_span_bytes: u64,
    /// Non-zero when the file carries a third track (A_OPUSHF, the
    /// 12 kHz..20 kHz opus track). ABI 2 field; hosts compiled against
    /// ABI 1 headers must be rebuilt (struct size changed).
    pub three_track: u32,
    /// Mid-layer codec: 0 = Opus, 1 = xHE-AAC (the encoder identity behind
    /// an xHE mid is an encode-side detail and not exposed).
    pub mid_codec: u32,
    /// Top-band codec: 0 = Opus, 1 = xHE-AAC (three-track only; the xfi
    /// 288k variant, whose output runs at 44.1 kHz). ABI 3 field; hosts
    /// compiled against ABI 1/2 headers must be rebuilt (struct size
    /// changed).
    pub hf_codec: u32,
}

#[repr(C)]
pub struct SenaDecReadInfo {
    pub start_frame: u64,
    pub frames: u64,
    pub payload_bits: u64,
}

impl From<crate::pipeline::ReadInfo> for SenaDecReadInfo {
    fn from(i: crate::pipeline::ReadInfo) -> Self {
        Self {
            start_frame: i.start_frame,
            frames: i.frames,
            payload_bits: i.payload_bits,
        }
    }
}

#[repr(C)]
pub struct SenaMetaEntry {
    pub key: *const c_char,
    pub value: *const c_char,
}

/// Project a decoded-stream info into the C ABI struct (the sha256 hex is
/// copied into the fixed buffer, NUL-padded).
pub(crate) fn to_c_info(i: &crate::pipeline::DecodedInfo) -> SenaDecInfo {
    let mut sha = [0 as c_char; SENA_SHA256_HEX_LEN + 1];
    for (k, c) in i
        .audio_sha256
        .as_bytes()
        .iter()
        .take(SENA_SHA256_HEX_LEN)
        .enumerate()
    {
        sha[k] = *c as c_char;
    }
    SenaDecInfo {
        sample_rate: i.sample_rate,
        channels: i.channels,
        playable_frames: i.playable_frames,
        profile: i.profile,
        sena_version: i.sena_version,
        audio_sha256: sha,
        audio_span_bytes: i.audio_span_bytes,
        three_track: i.three_track as u32,
        mid_codec: i.mid_codec as u32,
        hf_codec: i.hf_codec as u32,
    }
}

/// Borrow the host's f32 stereo-interleaved output buffer for `frames`
/// frames (the multiplication is overflow-checked).
pub(crate) unsafe fn pcm_out_slice<'a>(ptr: *mut f32, frames: u64) -> Result<&'a mut [f32], i32> {
    let n = usize::try_from(frames)
        .ok()
        .and_then(|w| w.checked_mul(INTERLEAVED_CHANNELS))
        .ok_or(SENA_DEC_ERR_INVALID)?;
    Ok(unsafe { std::slice::from_raw_parts_mut(ptr, n) })
}

pub(crate) fn write_err(err: *mut c_char, err_len: usize, msg: &str) {
    if err.is_null() || err_len == 0 {
        return;
    }
    // Never split a UTF-8 codepoint: the host treats the buffer as text.
    let mut n = msg.len().min(err_len - 1);
    while n > 0 && !msg.is_char_boundary(n) {
        n -= 1;
    }
    unsafe {
        std::ptr::copy_nonoverlapping(msg.as_ptr(), err.cast::<u8>(), n);
        *err.add(n) = 0;
    }
}

pub(crate) fn error_code<T>(e: &T) -> i32
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
    } else if s.starts_with("invalid argument:") {
        SENA_DEC_ERR_INVALID
    } else {
        SENA_DEC_ERR_FORMAT
    }
}

pub(crate) fn map_decode_error(e: &crate::pipeline::DecodeError) -> i32 {
    use crate::pipeline::DecodeError as D;
    match e {
        D::Unsupported(_) => SENA_DEC_ERR_UNSUPPORTED,
        D::Codec(_) => SENA_DEC_ERR_DECODE,
        D::Io(_) => SENA_DEC_ERR_IO,
        D::Format(_) | D::ShortOutput { .. } => SENA_DEC_ERR_FORMAT,
    }
}

/// Runs `f` and converts an escaping panic into `panic_code` instead of
/// aborting the host process: an unwind crossing an `extern "C"` boundary
/// kills the whole player instantly, with the panic message lost on stderr
/// (invisible in a GUI host). The decode-path entry points below are the
/// hottest and most complex code in the crate, so they must never unwind.
pub(crate) fn guard(panic_code: i32, f: impl FnOnce() -> Result<i32, i32>) -> i32 {
    match catch_unwind(AssertUnwindSafe(f)) {
        Ok(r) => r.unwrap_or_else(|e| e),
        Err(_) => panic_code,
    }
}

/// Common epilogue for the String-error entry points: deliver the value via
/// `ok` on success, otherwise write the message to the host buffer and
/// classify it into a status code.
pub(crate) fn finish<T>(
    result: Result<Result<T, String>, Box<dyn std::any::Any + Send>>,
    err: *mut c_char,
    err_len: usize,
    ok: impl FnOnce(T),
) -> i32 {
    match result {
        Ok(Ok(v)) => {
            ok(v);
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

/// Drop a heap handle handed to the host (NULL is a no-op).
pub(crate) unsafe fn close_box<T>(p: *mut T) {
    if !p.is_null() {
        unsafe { drop(Box::from_raw(p)) };
    }
}

/// NUL-terminated copy of `s` for the art/tags handle vectors.
pub(crate) fn cstr_vec(s: &str) -> Vec<u8> {
    let mut v = s.as_bytes().to_vec();
    v.push(0);
    v
}

#[cfg(test)]
mod tests;
