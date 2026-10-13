//! Elementary-stream entry points (`sena_es_*`) for hosts that demux the
//! container themselves (the ffmpeg decoder plugin, a future DirectShow
//! transform filter): open from the track CodecPrivate (WAVEFORMATEX +
//! SENA-INIT), push composite blocks in timeline order, pull PCM.

use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};

use super::{
    SENA_DEC_ERR_DECODE, SENA_DEC_ERR_INVALID, SENA_DEC_EOF, SENA_DEC_OK, SenaDecInfo,
    SenaDecReadInfo, close_box, finish, guard, map_decode_error, pcm_out_slice, to_c_info,
};

pub struct SenaEsHandle {
    dec: crate::es::EsDecoder,
}

#[repr(C)]
pub struct SenaEsStats {
    pub lf_bytes: u64,
    pub mid_bytes: u64,
    pub hf_bytes: u64,
    pub blocks_decoded: u64,
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_open(
    codec_private: *const u8,
    codec_private_len: usize,
    out: *mut *mut SenaEsHandle,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(|| -> Result<(), String> {
        if codec_private.is_null() || out.is_null() {
            return Err("invalid argument: NULL codec_private/out".into());
        }
        let cp = unsafe { std::slice::from_raw_parts(codec_private, codec_private_len) };
        let init =
            sena_core::es::Init::decode(cp).map_err(|e| format!("unsupported: {e}"))?;
        let params = crate::es::parse_params(init).map_err(|e| e.to_string())?;
        let dec = crate::es::EsDecoder::open_push(params).map_err(|e| e.to_string())?;
        unsafe { *out = Box::into_raw(Box::new(SenaEsHandle { dec })) };
        Ok(())
    }));
    finish(result, err, err_len, |_| {})
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_push(
    es: *mut SenaEsHandle,
    block: *const u8,
    block_len: usize,
    ts_ns: u64,
) -> i32 {
    if es.is_null() || block.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &mut *es };
        let data = unsafe { std::slice::from_raw_parts(block, block_len) };
        d.dec.push(ts_ns, data).map_err(|e| map_decode_error(&e))?;
        Ok(SENA_DEC_OK)
    })
}

/// Pull decoded PCM. Returns SENA_DEC_OK when frames were produced,
/// SENA_DEC_EOF at end of stream, and SENA_DEC_OK with *out_frames == 0
/// when the decoder is starved (push more blocks, then pull again).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_pull(
    es: *mut SenaEsHandle,
    interleaved: *mut f32,
    frames: u64,
    out_frames: *mut u64,
) -> i32 {
    if es.is_null() || interleaved.is_null() || out_frames.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &mut *es };
        let slice = unsafe { pcm_out_slice(interleaved, frames) }?;
        match d.dec.pull(slice, frames) {
            Ok(n) => {
                unsafe { *out_frames = n };
                Ok(if n == 0 && d.dec.at_eof() {
                    SENA_DEC_EOF
                } else {
                    SENA_DEC_OK
                })
            }
            Err(e) => Err(map_decode_error(&e)),
        }
    })
}

/// Signal end of stream on the push feed (the host delivers no more blocks).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_finish(es: *mut SenaEsHandle) {
    if !es.is_null() {
        unsafe { &mut *es }.dec.finish();
    }
}

/// Flush/reset (the host is about to seek; the next push re-anchors the
/// timeline from its timestamp).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_reset(es: *mut SenaEsHandle) {
    if !es.is_null() {
        unsafe { &mut *es }.dec.reset();
    }
}

/// Discard output before `frame` on subsequent pulls (a demuxer delivers
/// from a cue point earlier than the seek target).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_set_output_start(es: *mut SenaEsHandle, frame: u64) {
    if !es.is_null() {
        unsafe { &mut *es }.dec.set_output_start(frame);
    }
}

/// Seek warmup from a shadow copy of the same file: decodes the warmup
/// window off the host's delivery path so the state at the seek target
/// equals the continuous decode (same quality as a container seek). Fails
/// on non-local/unreadable paths; the host then falls back to the
/// cold-start ramp.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_warmup_seek(
    es: *mut SenaEsHandle,
    path: *const c_char,
    target_frame: u64,
) -> i32 {
    if es.is_null() || path.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &mut *es };
        let path = unsafe { CStr::from_ptr(path) }
            .to_str()
            .map_err(|_| SENA_DEC_ERR_INVALID)?;
        let store = match shadow_store(path, &d.dec) {
            Ok(s) => s,
            Err(e) => return Err(map_decode_error(&e)),
        };
        d.dec
            .warmup_seek(store, target_frame)
            .map_err(|e| map_decode_error(&e))?;
        Ok(SENA_DEC_OK)
    })
}

/// Open `path` as a shadow block source for warmup, verifying it carries
/// the same content (same SENA-INIT) as the decoder's stream.
fn shadow_store(
    path: &str,
    dec: &crate::es::EsDecoder,
) -> Result<crate::es::BlockStore, crate::pipeline::DecodeError> {
    use crate::pipeline::DecodeError;
    let file = std::fs::File::open(path).map_err(|e| DecodeError::Io(e.to_string()))?;
    let file = std::sync::Mutex::new(file);
    let mut read_at = move |pos: u64, len: usize| -> Result<Vec<u8>, String> {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = file.lock().map_err(|_| "io: shadow lock poisoned".to_string())?;
        f.seek(SeekFrom::Start(pos))
            .map_err(|e| format!("io: shadow seek: {e}"))?;
        let mut buf = vec![0u8; len];
        let mut off = 0usize;
        while off < len {
            match f.read(&mut buf[off..]) {
                Ok(0) => break,
                Ok(n) => off += n,
                Err(e) => return Err(format!("io: shadow read: {e}")),
            }
        }
        buf.truncate(off);
        Ok(buf)
    };
    let mut indexed = crate::demux::index_container(&mut read_at, true)
        .map_err(DecodeError::Format)?;
    if indexed.init != *dec.init() {
        return Err(DecodeError::Format(
            "shadow file carries a different SENA-INIT".into(),
        ));
    }
    let blocks = std::mem::take(&mut indexed.blocks);
    Ok(crate::es::BlockStore::Lazy {
        blocks,
        read_at: Box::new(read_at),
        window: (0, Vec::new()),
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_get_stats(es: *mut SenaEsHandle, stats: *mut SenaEsStats) -> i32 {
    if es.is_null() || stats.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*es };
        let (lf, mid, hf) = d.dec.layer_bytes();
        unsafe {
            *stats = SenaEsStats {
                lf_bytes: lf,
                mid_bytes: mid,
                hf_bytes: hf,
                blocks_decoded: d.dec.blocks_decoded(),
            }
        };
        Ok(SENA_DEC_OK)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_get_read_info(
    es: *mut SenaEsHandle,
    info: *mut SenaDecReadInfo,
) -> i32 {
    if es.is_null() || info.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*es };
        let i = d.dec.read_info();
        unsafe {
            *info = i.into();
        };
        Ok(SENA_DEC_OK)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_get_info(es: *mut SenaEsHandle, info: *mut SenaDecInfo) -> i32 {
    if es.is_null() || info.is_null() {
        return SENA_DEC_ERR_INVALID;
    }
    guard(SENA_DEC_ERR_DECODE, || {
        let d = unsafe { &*es };
        unsafe {
            *info = to_c_info(d.dec.info());
        };
        Ok(SENA_DEC_OK)
    })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_es_close(es: *mut SenaEsHandle) {
    unsafe { close_box(es) }
}
