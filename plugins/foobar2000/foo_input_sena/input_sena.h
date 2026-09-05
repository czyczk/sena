#pragma once

#include "stdafx.h"
#include "sena_dec.h"
#include <helpers/dynamic_bitrate_helper.h>

class input_sena : public input_stubs {
public:
    // The decoder handle is a raw pointer into the Rust staticlib and is NOT
    // freed by close-on-reopen alone: foobar destroys this instance after
    // playback / info read / RG scan, and without a destructor the whole
    // Rust-side decoder (frame index, decode buffers) leaks per track.
    // (input_stubs is non-polymorphic and this class is stored by value in
    // the SDK wrapper, so this is a plain dtor, not an override.)
    ~input_sena();

    void open(service_ptr_t<file> hint, const char *path,
              t_input_open_reason reason, abort_callback &abort);
    void get_info(file_info &info, abort_callback &abort);
    t_filestats2 get_stats2(uint32_t f, abort_callback &abort);
    void decode_initialize(unsigned flags, abort_callback &abort);
    bool decode_run(audio_chunk &chunk, abort_callback &abort);
    void decode_seek(double seconds, abort_callback &abort);
    bool decode_can_seek();
    bool decode_get_dynamic_info(file_info &info, double &timestamp_delta);
    bool decode_get_dynamic_info_track(file_info &info, double &timestamp_delta);
    void decode_on_idle(abort_callback &abort);
    void retag(const file_info &info, abort_callback &abort);
    void remove_tags(abort_callback &abort);

    static bool g_is_our_content_type(const char *content_type);
    static bool g_is_our_path(const char *path, const char *extension);
    static const char *g_get_name();
    static GUID g_get_guid();

private:
    void open_decoder(abort_callback &abort);
    void close_decoder();
    void read_user_tags(file_info &info, abort_callback &abort);
    void ensure_decoder(abort_callback &abort);
    static int64_t io_read(void *ctx, void *buf, uint64_t len);
    static int64_t io_seek(void *ctx, int64_t offset, int whence);
    static int64_t io_tell(void *ctx);
    static uint64_t io_size(void *ctx);
    static int64_t io_write(void *ctx, const void *buf, uint64_t len);

    service_ptr_t<file> m_file;
    SenaDec *m_dec = nullptr;
    SenaDecInfo m_probe_info{};
    bool m_has_probe = false;
    abort_callback *m_abort = nullptr;
    pfc::array_t<float> m_pcm;
    dynamic_bitrate_helper m_bitrate;
    bool m_can_seek = false;
};
