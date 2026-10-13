//! Album art (Matroska Attachments) read/write entry points.

use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use super::io::{range_reader, read_all_file, rewrite_via_plan};
use super::{SenaFileIo, close_box, cstr_vec, finish};
use crate::demux::Demuxed;

/// Album art is a few MB in practice; beyond this the container is corrupt.
const MAX_ART_ELEMENT_BYTES: u64 = 64 * 1024 * 1024;

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
    let result = catch_unwind(AssertUnwindSafe(
        || -> Result<*mut SenaArtHandle, String> {
            if io.is_null() || out.is_null() {
                return Err("invalid argument: NULL io/out".into());
            }
            let io = unsafe { &*io };
            let read = io.read.ok_or("io: read callback is NULL")?;
            let arts = match io
                .seek
                .and_then(|seek| range_reader(io.user_data, read, seek))
            {
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
            Ok(Box::into_raw(Box::new(SenaArtHandle {
                names,
                mimes,
                datas,
            })))
        },
    ));
    finish(result, err, err_len, |h| unsafe { *out = h })
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_count(art: *const SenaArtHandle) -> u32 {
    if art.is_null() {
        return 0;
    }
    let t = unsafe { &*art };
    t.names.len() as u32
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_name(art: *const SenaArtHandle, index: u32) -> *const c_char {
    if art.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*art };
    t.names
        .get(index as usize)
        .map(|v| v.as_ptr().cast())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_mime(art: *const SenaArtHandle, index: u32) -> *const c_char {
    if art.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*art };
    t.mimes
        .get(index as usize)
        .map(|v| v.as_ptr().cast())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_data(art: *const SenaArtHandle, index: u32) -> *const u8 {
    if art.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*art };
    t.datas
        .get(index as usize)
        .map(|v| v.as_ptr())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_data_len(art: *const SenaArtHandle, index: u32) -> usize {
    if art.is_null() {
        return 0;
    }
    let t = unsafe { &*art };
    t.datas.get(index as usize).map(|v| v.len()).unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_art_close(art: *mut SenaArtHandle) {
    unsafe { close_box(art) }
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
                let name = unsafe { CStr::from_ptr(e.name) }
                    .to_string_lossy()
                    .into_owned();
                let mime = unsafe { CStr::from_ptr(e.mime) }
                    .to_string_lossy()
                    .into_owned();
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
    finish(result, err, err_len, |_| {})
}
