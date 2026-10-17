use super::*;
use crate::Demuxed;
use std::cell::UnsafeCell;
use std::ffi::{CStr, c_void};

struct MemFile {
    data: UnsafeCell<Vec<u8>>,
    pos: UnsafeCell<usize>,
}

impl MemFile {
    fn new(data: Vec<u8>) -> Self {
        Self {
            data: UnsafeCell::new(data),
            pos: UnsafeCell::new(0),
        }
    }
}

unsafe extern "C" fn mem_read(user: *mut c_void, buf: *mut c_void, len: u64) -> i64 {
    let f = unsafe { &*(user as *const MemFile) };
    let data = unsafe { &*f.data.get() };
    let pos = unsafe { &mut *f.pos.get() };
    let n = (len as usize).min(data.len().saturating_sub(*pos));
    unsafe {
        std::ptr::copy_nonoverlapping(data[*pos..*pos + n].as_ptr(), buf.cast::<u8>(), n)
    };
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

// Test double: every seek is absolute (WHENCE_CUR is ignored), so the
// no-op reposition probe in `range_reader` passes trivially here.
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
    assert_eq!(
        error_code(&"unsupported: x".to_string()),
        SENA_DEC_ERR_UNSUPPORTED
    );
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
    assert_eq!(
        unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
    assert!(!handle.is_null());
    let mut info = SenaDecInfo {
        sample_rate: 0,
        channels: 0,
        playable_frames: 0,
        profile: 0,
        sena_version: 0,
        audio_sha256: [0; 65],
        audio_span_bytes: 0,
        three_track: 0,
        mid_codec: 0,
    hf_codec: 0,
    };
    assert_eq!(unsafe { sena_dec_get_info(handle, &mut info) }, SENA_DEC_OK);
    assert_eq!(
        (
            info.sample_rate,
            info.channels,
            info.playable_frames,
            info.profile
        ),
        (48000, 2, 960000, 300)
    );

    let mut buf = vec![0.0f32; 4800 * 2];
    let mut got = 0u64;
    assert_eq!(
        unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) },
        SENA_DEC_OK
    );
    assert_eq!(got, 4800);
    let mut ri = SenaDecReadInfo {
        start_frame: 0,
        frames: 0,
        payload_bits: 0,
    };
    assert_eq!(
        unsafe { sena_dec_get_read_info(handle, &mut ri) },
        SENA_DEC_OK
    );
    assert_eq!(ri.frames, 4800);
    assert!(ri.payload_bits > 0);

    assert_eq!(unsafe { sena_dec_seek(handle, 959_900) }, SENA_DEC_OK);
    assert_eq!(
        unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) },
        SENA_DEC_OK
    );
    assert_eq!(got, 100);
    assert_eq!(
        unsafe { sena_dec_read_f32(handle, buf.as_mut_ptr(), 4800, &mut got) },
        SENA_DEC_EOF
    );
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
    assert_eq!(
        unsafe {
            sena_file_write_tags(&io, entries.as_ptr(), 1, err.as_mut_ptr().cast(), err.len())
        },
        SENA_DEC_OK
    );
    let data = unsafe { &*mem.data.get() };
    let demux = Demuxed::parse(data.clone()).unwrap();
    assert_eq!(demux.tags.last().unwrap().get("TITLE"), Some("test song"));
    assert_eq!(demux.init.profile, sena_core::Profile::At300);

    // The C ABI read path now scans EBML headers with seek+read and must
    // see the freshly appended tail Tags without reading the clusters.
    let mut handle: *mut SenaTagsHandle = std::ptr::null_mut();
    assert_eq!(
        unsafe { sena_file_read_tags(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
    assert_eq!(unsafe { sena_tags_count(handle) }, 1);
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(sena_tags_key(handle, 0)) }.to_bytes(),
        b"TITLE"
    );
    assert_eq!(
        unsafe { std::ffi::CStr::from_ptr(sena_tags_value(handle, 0)) }.to_bytes(),
        b"test song"
    );
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
    assert_eq!(
        unsafe {
            sena_file_write_tags(&io, entries.as_ptr(), 1, err.as_mut_ptr().cast(), err.len())
        },
        SENA_DEC_OK
    );
    let data = unsafe { &*mem.data.get() };
    // The file grew by exactly the appended tail Tags element; everything
    // else was written in place.
    assert!(data.len() > before_len);
    let demux = Demuxed::parse(data.clone()).unwrap();
    assert_eq!(demux.tags.last().unwrap().get("TITLE"), Some("test song"));
    assert_eq!(demux.init.profile, sena_core::Profile::At300);
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
    let mut info = SenaDecInfo {
        sample_rate: 0,
        channels: 0,
        playable_frames: 0,
        profile: 0,
        sena_version: 0,
        audio_sha256: [0; 65],
        audio_span_bytes: 0,
        three_track: 0,
        mid_codec: 0,
    hf_codec: 0,
    };
    let mut err = [0u8; 256];
    assert_eq!(
        unsafe { sena_dec_probe_info(&io, &mut info, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
    assert_eq!(
        (
            info.sample_rate,
            info.channels,
            info.playable_frames,
            info.profile
        ),
        (48000, 2, 960000, 300)
    );
}

/// Regression: `audio_span_bytes` (the source for the fb2k static
/// average bitrate) covers only Cluster elements, so a large cover
/// attachment and user-tag rewrites must not change it, and the
/// open-decoder path must agree with the head-only probe.
#[test]
fn abi_audio_span_excludes_cover_and_tags() {
    let bytes = std::fs::read(format!(
        "{}/../../assets/e2e/01__p300__lfa__hf144.sena",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
    let art = vec![crate::attachments::Attachment::new(
        "cover_front.jpg",
        "image/jpeg",
        vec![0xAB; 128 * 1024],
    )];
    let with_art = crate::attachments::rewrite_attachments(&bytes, &art).unwrap();
    assert!(with_art.len() > bytes.len() + 128 * 1024);

    let probe_span = |data: &[u8]| {
        let mem = MemFile::new(data.to_vec());
        let io = SenaDecIo {
            user_data: (&mem as *const MemFile).cast_mut().cast(),
            read: Some(mem_read),
            seek: Some(mem_seek),
            tell: None,
            size: Some(mem_size),
        };
        let mut info = SenaDecInfo {
            sample_rate: 0,
            channels: 0,
            playable_frames: 0,
            profile: 0,
            sena_version: 0,
            audio_sha256: [0; 65],
            audio_span_bytes: 0,
            three_track: 0,
            mid_codec: 0,
        hf_codec: 0,
        };
        let mut err = [0u8; 256];
        assert_eq!(
            unsafe { sena_dec_probe_info(&io, &mut info, err.as_mut_ptr().cast(), err.len()) },
            SENA_DEC_OK
        );
        info.audio_span_bytes
    };

    let span = probe_span(&bytes);
    assert!(span > 0);
    assert!((span as usize) < bytes.len());
    let payload: u64 = Demuxed::parse(bytes.clone())
        .unwrap()
        .blocks
        .iter()
        .map(|b| b.data.len() as u64)
        .sum();
    assert!(payload <= span);
    assert_eq!(probe_span(&with_art), span);

    let mem = MemFile::new(with_art.clone());
    let file_io = SenaFileIo {
        user_data: (&mem as *const MemFile).cast_mut().cast(),
        read: Some(mem_read),
        write: Some(mem_write),
        seek: Some(mem_seek),
        tell: None,
        size: Some(mem_size),
    };
    let entries = [SenaMetaEntry {
        key: c"TITLE".as_ptr(),
        value: c"cover test".as_ptr(),
    }];
    let mut err = [0u8; 256];
    assert_eq!(
        unsafe {
            sena_file_write_tags(
                &file_io,
                entries.as_ptr(),
                1,
                err.as_mut_ptr().cast(),
                err.len(),
            )
        },
        SENA_DEC_OK
    );
    let after: Vec<u8> = unsafe { &*mem.data.get() }.clone();
    assert!(after.len() > with_art.len());
    assert_eq!(probe_span(&after), span);

    let mem2 = MemFile::new(after);
    let io2 = SenaDecIo {
        user_data: (&mem2 as *const MemFile).cast_mut().cast(),
        read: Some(mem_read),
        seek: Some(mem_seek),
        tell: None,
        size: Some(mem_size),
    };
    let mut handle: *mut SenaDecHandle = std::ptr::null_mut();
    assert_eq!(
        unsafe { sena_dec_open(&io2, &mut handle, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
    let mut info = SenaDecInfo {
        sample_rate: 0,
        channels: 0,
        playable_frames: 0,
        profile: 0,
        sena_version: 0,
        audio_sha256: [0; 65],
        audio_span_bytes: 0,
        three_track: 0,
        mid_codec: 0,
    hf_codec: 0,
    };
    assert_eq!(unsafe { sena_dec_get_info(handle, &mut info) }, SENA_DEC_OK);
    unsafe { sena_dec_close(handle) };
    assert_eq!(info.audio_span_bytes, span);
}

/// `sena_file_art_read` via the range-read path must return the same
/// attachments as the buffered parse (playback start queries cover art
/// through this path).
#[test]
fn abi_art_read_range_path() {
    let base = open_mem("assets/e2e/01__p300__lfa__hf144.sena");
    let base_bytes = unsafe { &*base.data.get() }.clone();
    let art = vec![
        crate::attachments::Attachment::new(
            "cover_front.jpg",
            "image/jpeg",
            vec![0xFF, 0xD8, 0xFF, 0xE0, 1, 2, 3, 4, 5],
        ),
        crate::attachments::Attachment::new("artist.jpg", "image/jpeg", vec![9, 8, 7, 6]),
    ];
    let with_art = crate::attachments::rewrite_attachments(&base_bytes, &art).unwrap();

    // Reference through the in-memory parse.
    let reference =
        crate::attachments::parse_attachments(&Demuxed::parse(with_art.clone()).unwrap());
    assert_eq!(reference.len(), 2);

    let mem = MemFile::new(with_art);
    let io = SenaFileIo {
        user_data: (&mem as *const MemFile).cast_mut().cast(),
        read: Some(mem_read),
        write: Some(mem_write),
        seek: Some(mem_seek),
        tell: None,
        size: Some(mem_size),
    };
    let mut handle: *mut SenaArtHandle = std::ptr::null_mut();
    let mut err = [0u8; 256];
    assert_eq!(
        unsafe { sena_file_art_read(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
    assert!(!handle.is_null());
    assert_eq!(unsafe { sena_art_count(handle) }, 2);
    for i in 0..2u32 {
        let name = unsafe { CStr::from_ptr(sena_art_name(handle, i)) }
            .to_string_lossy()
            .into_owned();
        let mime = unsafe { CStr::from_ptr(sena_art_mime(handle, i)) }
            .to_string_lossy()
            .into_owned();
        let len = unsafe { sena_art_data_len(handle, i) };
        let data =
            unsafe { std::slice::from_raw_parts(sena_art_data(handle, i), len) }.to_vec();
        let expect = &reference[i as usize];
        assert_eq!(name, expect.name);
        assert_eq!(mime, expect.mime);
        assert_eq!(data, expect.data);
    }
    unsafe { sena_art_close(handle) };
}

/// The lazy io path must be bit-exact with the in-memory store: same
/// streaming decoder, same bytes, only the payload source differs. (The
/// separate whole-file `pipeline::Decoder` is NOT bit-identical to the
/// streaming decoder by design - the zero-phase resampler sees different
/// context - so it is not a reference here.)
#[test]
fn abi_lazy_decode_matches_reference() {
    let bytes = std::fs::read(format!(
        "{}/../../assets/e2e/01__p300__lfa__hf144.sena",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
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
    assert_eq!(
        unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) },
        SENA_DEC_OK
    );
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
    let bytes = std::fs::read(format!(
        "{}/../../assets/e2e/01__p300__lfa__hf144.sena",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap();
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
            assert_eq!(
                unsafe { sena_dec_open(&io, &mut handle, err.as_mut_ptr().cast(), err.len()) },
                SENA_DEC_OK
            );
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
