//! Container decode entry points (`sena_dec_*`): open/close/info/read/seek
//! plus the head-only probe.

use std::ffi::c_char;
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::io::{range_reader, read_all};
use super::{
    SENA_DEC_ERR_DECODE, SENA_DEC_ERR_INVALID, SENA_DEC_EOF, SENA_DEC_OK, SenaDecInfo,
    SenaDecIo, SenaDecReadInfo, close_box, finish, guard, map_decode_error, pcm_out_slice,
    to_c_info,
};
use crate::demux::Demuxed;
use crate::stream::StreamingDecoder;

pub struct SenaDecHandle {
    decoder: StreamingDecoder,
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
        let decoder = match io
            .seek
            .and_then(|seek| range_reader(io.user_data, read, seek))
        {
            // Seekable input: index the container, fetch frame payloads
            // lazily - per-instance memory stays bounded no matter the file
            // size (32-bit foobar2000 has a 2 GiB address space and runs
            // several decode threads concurrently for ReplayGain scans).
            Some(mut read_at) => {
                let mut indexed = crate::demux::index_container(&mut read_at, true)?;
                let blocks = std::mem::take(&mut indexed.blocks);
                let store = crate::es::BlockStore::Lazy {
                    blocks,
                    read_at: Box::new(read_at),
                    window: (0, Vec::new()),
                };
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
    finish(result, err, err_len, |_| {})
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_close(dec: *mut SenaDecHandle) {
    unsafe { close_box(dec) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_dec_get_info(dec: *mut SenaDecHandle, info: *mut SenaDecInfo) -> i32 {
    if dec.is_null() || info.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*dec };
        unsafe {
            *info = to_c_info(d.decoder.info());
        };
        Ok(SENA_DEC_OK)
    })
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
        let slice = unsafe { pcm_out_slice(interleaved, frames) }?;
        match d.decoder.read_f32(slice, frames) {
            Ok(n) => {
                unsafe { *out_frames = n };
                Ok(if n == 0 { SENA_DEC_EOF } else { SENA_DEC_OK })
            }
            Err(e) => Err(map_decode_error(&e)),
        }
    })
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
        unsafe {
            *info = i.into();
        };
        Ok(SENA_DEC_OK)
    })
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
}

// The pinned ABI number must match the literal in the version string below.
const _: () = assert!(super::SENA_DEC_ABI_VERSION == 3);

#[unsafe(no_mangle)]
pub extern "C" fn sena_dec_version() -> *const c_char {
    concat!("sena-dec ", env!("CARGO_PKG_VERSION"), " (ABI 3)\0").as_ptr().cast()
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
        let (probe, _warnings) = match io
            .seek
            .and_then(|seek| range_reader(io.user_data, read, seek))
        {
            // Head-only scan: tracks + immutable tags, cluster payloads and
            // even the frame index are skipped.
            Some(mut read_at) => {
                let indexed = crate::demux::index_container(&mut read_at, false)?;
                crate::pipeline::probe_params(&indexed.init, indexed.audio_span_bytes)
                    .map_err(|e| e.to_string())?
            }
            None => {
                let bytes = unsafe { read_all(io) }?;
                let demux = Demuxed::parse(bytes).map_err(|e| e.to_string())?;
                crate::pipeline::probe_params(&demux.init, demux.audio_span_bytes)
                    .map_err(|e| e.to_string())?
            }
        };
        Ok(to_c_info(&probe))
    }));
    finish(result, err, err_len, |i| unsafe { *info = i })
}
