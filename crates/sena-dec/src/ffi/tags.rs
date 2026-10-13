//! User tag read/write entry points (`sena_file_*_tags`, `sena_tags_*`).

use std::ffi::{CStr, c_char};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;

use super::io::{read_file_at, rewrite_via_plan};
use super::{
    SENA_DEC_ERR_INVALID, SenaFileIo, SenaMetaEntry, close_box, cstr_vec, finish, write_err,
};
use crate::tags::rewrite_user_tags;

pub struct SenaTagsHandle {
    keys: Vec<Vec<u8>>,
    values: Vec<Vec<u8>>,
}

unsafe fn copy_meta_entries(
    entries: *const SenaMetaEntry,
    count: u32,
) -> Result<Vec<(String, String)>, String> {
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
        let k = unsafe { CStr::from_ptr(e.key) }
            .to_string_lossy()
            .into_owned();
        let v = unsafe { CStr::from_ptr(e.value) }
            .to_string_lossy()
            .into_owned();
        if k.is_empty() {
            return Err("invalid argument: empty metadata key".into());
        }
        out.push((k, v));
    }
    Ok(out)
}

/// Shared tag rewrite body: apply `entries` (empty = remove all user tags).
unsafe fn rewrite_tags(
    io: *const SenaFileIo,
    entries: &[(String, String)],
    err: *mut c_char,
    err_len: usize,
) -> i32 {
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
    finish(result, err, err_len, |_| {})
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
    unsafe { rewrite_tags(io, &entries, err, err_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_remove_tags(
    io: *const SenaFileIo,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    unsafe { rewrite_tags(io, &[], err, err_len) }
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_file_read_tags(
    io: *const SenaFileIo,
    out: *mut *mut SenaTagsHandle,
    err: *mut c_char,
    err_len: usize,
) -> i32 {
    let result = catch_unwind(AssertUnwindSafe(
        || -> Result<*mut SenaTagsHandle, String> {
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
        },
    ));
    finish(result, err, err_len, |h| unsafe { *out = h })
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
    t.keys
        .get(index as usize)
        .map(|v| v.as_ptr().cast())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_value(tags: *const SenaTagsHandle, index: u32) -> *const c_char {
    if tags.is_null() {
        return ptr::null();
    }
    let t = unsafe { &*tags };
    t.values
        .get(index as usize)
        .map(|v| v.as_ptr().cast())
        .unwrap_or(ptr::null())
}

#[unsafe(no_mangle)]
pub unsafe extern "C" fn sena_tags_close(tags: *mut SenaTagsHandle) {
    unsafe { close_box(tags) }
}
