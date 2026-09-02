#include "stdafx.h"
#include "input_sena.h"

namespace {

constexpr unsigned kChannels = 2;
constexpr unsigned kSampleRate = 48000;
constexpr uint64_t kFramesPerChunk = 4096;

void throw_sena_error(int code, const char *what) {
    switch (code) {
        case SENA_DEC_ERR_IO:
        case SENA_DEC_ERR_DECODE:
        case SENA_DEC_ERR_FORMAT:
            throw exception_io_data();
        case SENA_DEC_ERR_UNSUPPORTED:
            throw exception_io_unsupported_format();
        default:
            throw exception_io_data();
    }
}

} // namespace

void input_sena::open(service_ptr_t<file> hint, const char *path,
                      t_input_open_reason reason, abort_callback &abort) {
    m_file = hint;
    input_open_file_helper(m_file, path, reason, abort);
    m_abort = &abort;
    // Info/tag-read must not decode the file. Decode is opened lazily by
    // decode_initialize(); get_info() uses the lightweight probe ABI.
    if (reason == input_open_decode) {
        open_decoder(abort);
    }
}

void input_sena::get_info(file_info &info, abort_callback &abort) {
    SenaDecInfo di{};
    if (m_dec) {
        if (sena_dec_get_info(m_dec, &di) != SENA_DEC_OK) {
            throw exception_io_data();
        }
    } else {
        if (!m_has_probe) {
            m_abort = &abort;
            m_file->reopen(abort);
            SenaDecIo io{};
            io.user_data = this;
            io.read = io_read;
            io.seek = io_seek;
            io.tell = io_tell;
            io.size = io_size;
            char errbuf[256];
            int rc = sena_dec_probe_info(&io, &m_probe_info, errbuf, sizeof(errbuf));
            if (rc != SENA_DEC_OK) {
                throw_sena_error(rc, errbuf);
            }
            m_has_probe = true;
        }
        di = m_probe_info;
    }
    info.set_length((double)di.playable_frames / (double)di.sample_rate);
    info.info_set_int("samplerate", di.sample_rate);
    info.info_set_int("channels", di.channels);
    // Lossy codecs do not report a fixed "bitspersample": decoded samples
    // stay in float32 internally, like the other lossy foobar inputs.
    info.info_set("encoding", "lossy");
    info.info_set("codec", "Sena");
    info.info_set("codec_profile", di.profile == 300 ? "xAAC-Opus@300" : "xAAC-Opus@600");
    if (di.audio_sha256[0]) {
        info.info_set("Audio SHA256", di.audio_sha256);
    }

    if (m_file->can_seek()) {
        t_filesize size = m_file->get_size_ex(abort);
        if (size != filesize_invalid && di.playable_frames > 0) {
            double seconds = (double)di.playable_frames / (double)di.sample_rate;
            info.info_set_bitrate((t_int64)((double)size * 8.0 / seconds / 1000.0 + 0.5));
        }
    }
    read_user_tags(info, abort);
}

t_filestats2 input_sena::get_stats2(uint32_t f, abort_callback &abort) {
    return m_file->get_stats2_(f, abort);
}

void input_sena::decode_initialize(unsigned, abort_callback &abort) {
    close_decoder();
    m_file->reopen(abort);
    open_decoder(abort);
    m_bitrate.reset();
    m_can_seek = m_file->can_seek();
}

bool input_sena::decode_run(audio_chunk &chunk, abort_callback &abort) {
    m_abort = &abort;
    if (!m_dec) {
        open_decoder(abort);
    }
    m_pcm.set_size(kFramesPerChunk * kChannels);
    uint64_t got = 0;
    int rc = sena_dec_read_f32(m_dec, m_pcm.get_ptr(), kFramesPerChunk, &got);
    if (rc == SENA_DEC_EOF) {
        return false;
    }
    if (rc != SENA_DEC_OK) {
        throw_sena_error(rc, "sena_dec_read_f32");
    }
    if (got == 0) {
        return false;
    }
    audio_chunk::spec_t spec;
    spec.sampleRate = kSampleRate;
    spec.chanCount = kChannels;
    spec.chanMask = audio_chunk::channel_config_stereo;
    chunk.set_data_32(m_pcm.get_ptr(), (t_size)got, spec);

    SenaDecReadInfo ri{};
    if (sena_dec_get_read_info(m_dec, &ri) == SENA_DEC_OK) {
        m_bitrate.on_frame((double)ri.frames / (double)kSampleRate, (t_size)ri.payload_bits);
    }
    return true;
}

void input_sena::decode_seek(double seconds, abort_callback &abort) {
    m_abort = &abort;
    ensure_decoder(abort);
    if (!m_can_seek) {
        m_file->ensure_seekable();
    }
    uint64_t target = audio_math::time_to_samples(seconds, kSampleRate);
    SenaDecInfo di{};
    if (sena_dec_get_info(m_dec, &di) != SENA_DEC_OK) {
        throw exception_io_data();
    }
    if (target > di.playable_frames) {
        target = di.playable_frames;
    }
    if (sena_dec_seek(m_dec, target) != SENA_DEC_OK) {
        throw exception_io_data();
    }
    m_bitrate.reset();
}

bool input_sena::decode_can_seek() {
    return m_can_seek;
}

bool input_sena::decode_get_dynamic_info(file_info &info, double &timestamp_delta) {
    return m_bitrate.on_update(info, timestamp_delta);
}

bool input_sena::decode_get_dynamic_info_track(file_info &, double &) {
    return false;
}

void input_sena::decode_on_idle(abort_callback &abort) {
    m_file->on_idle(abort);
}

void input_sena::retag(const file_info &info, abort_callback &abort) {
    m_abort = &abort;
    m_file->reopen(abort);

    pfc::list_t<SenaMetaEntry> entries;
    pfc::list_t<SenaMetaEntry> meta_rg_entries; // RG arriving as plain meta
    t_size skipped = 0;
    t_size rg_written = 0;
    info.meta_enumerate([&](const char *key, const char *value) {
        // Attached pictures reach the tag writer as "PICTURE" meta with a
        // binary payload that Matroska string tags cannot hold; pictures
        // are handled separately through the album_art_editor service
        // (Matroska Attachments). Skip them so the transfer succeeds
        // instead of failing with "error transferring attached pictures".
        if (stricmp_utf8(key, "PICTURE") == 0) {
            skipped++;
            return;
        }
        // ReplayGain may arrive in meta (transferred tags) AND in the info
        // section (fresh scan) at the same time; collect both and dedupe
        // below with the info section winning.
        if (replaygain_info::g_is_meta_replaygain(key, strlen(key))) {
            meta_rg_entries.add_item(SenaMetaEntry{key, value});
            return;
        }
        entries.add_item(SenaMetaEntry{key, value});
    });
    // ReplayGain: foobar stores these in the info section (replaygain_info);
    // read them from there so the values actually reach the file. NOTE:
    // replaygain_info::for_each() reuses one stack text buffer for all four
    // values, so the pointers must be copied immediately - storing the raw
    // pointers produced garbage ("\x15;\x15").
    pfc::list_t<pfc::string8> rg_names, rg_values;
    info.get_replaygain().for_each([&](const char *key, const char *value) {
        rg_names.add_item(key);
        rg_values.add_item(value);
        rg_written++;
    });
    auto rg_key_present = [&](const char *key) {
        for (t_size i = 0; i < rg_names.get_count(); i++) {
            if (stricmp_utf8(rg_names[i], key) == 0) return true;
        }
        return false;
    };
    // The info section is authoritative (fresh scan); meta-form RG is kept
    // only for keys the info section does not provide (pure tag transfers).
    for (t_size i = 0; i < meta_rg_entries.get_count(); i++) {
        const char *key = meta_rg_entries[i].key;
        const char *value = meta_rg_entries[i].value;
        if (!rg_key_present(key)) {
            entries.add_item(SenaMetaEntry{key, value});
        }
    }
    for (t_size i = 0; i < rg_names.get_count(); i++) {
        entries.add_item(SenaMetaEntry{rg_names[i].get_ptr(), rg_values[i].get_ptr()});
    }
    pfc::string8 dbg;
    dbg << "foo_input_sena: retag writing " << entries.get_count() << " entries (skipped " << skipped
        << " pictures, " << rg_written << " replaygain from info)";
    console::print(dbg);

    SenaFileIo io{};
    io.user_data = this;
    io.read = io_read;
    io.write = io_write;
    io.seek = io_seek;
    io.tell = io_tell;
    io.size = io_size;

    pfc::string8 err;
    char errbuf[256];
    int rc = sena_file_write_tags(&io, entries.get_ptr(), (uint32_t)entries.get_count(), errbuf, sizeof(errbuf));
    if (rc != SENA_DEC_OK) {
        throw_sena_error(rc, errbuf);
    }
}

void input_sena::remove_tags(abort_callback &abort) {
    m_abort = &abort;
    m_file->reopen(abort);
    SenaFileIo io{};
    io.user_data = this;
    io.read = io_read;
    io.write = io_write;
    io.seek = io_seek;
    io.tell = io_tell;
    io.size = io_size;

    char errbuf[256];
    int rc = sena_file_remove_tags(&io, errbuf, sizeof(errbuf));
    if (rc != SENA_DEC_OK) {
        throw_sena_error(rc, errbuf);
    }
}

bool input_sena::g_is_our_content_type(const char *) {
    return false;
}

bool input_sena::g_is_our_path(const char *, const char *extension) {
    return stricmp_utf8(extension, "sena") == 0 || stricmp_utf8(extension, "mka") == 0;
}

const char *input_sena::g_get_name() {
    return "Sena Input";
}

GUID input_sena::g_get_guid() {
    // {8C7A6BE0-4C23-4D9E-9D1F-0C2B3E5A91D4}
    static const GUID guid = {0x8c7a6be0, 0x4c23, 0x4d9e, {0x9d, 0x1f, 0x0c, 0x2b, 0x3e, 0x5a, 0x91, 0xd4}};
    return guid;
}

void input_sena::open_decoder(abort_callback &abort) {
    close_decoder();
    m_abort = &abort;
    SenaDecIo io{};
    io.user_data = this;
    io.read = io_read;
    io.seek = io_seek;
    io.tell = io_tell;
    io.size = io_size;
    char errbuf[256];
    int rc = sena_dec_open(&io, &m_dec, errbuf, sizeof(errbuf));
    if (rc != SENA_DEC_OK) {
        throw_sena_error(rc, errbuf);
    }
}

void input_sena::close_decoder() {
    if (m_dec) {
        sena_dec_close(m_dec);
        m_dec = nullptr;
    }
}

void input_sena::read_user_tags(file_info &info, abort_callback &abort) {
    m_abort = &abort;
    if (!m_file->can_seek()) {
        return;
    }
    m_file->reopen(abort);
    SenaFileIo io{};
    io.user_data = this;
    io.read = io_read;
    io.write = io_write;
    io.seek = io_seek;
    io.tell = io_tell;
    io.size = io_size;

    SenaTags *tags = nullptr;
    char errbuf[256];
    if (sena_file_read_tags(&io, &tags, errbuf, sizeof(errbuf)) != SENA_DEC_OK) {
        return;
    }
    uint32_t n = sena_tags_count(tags);
    replaygain_info rg;
    for (uint32_t i = 0; i < n; ++i) {
        const char *key = sena_tags_key(tags, i);
        const char *value = sena_tags_value(tags, i);
        if (!key || !value) continue;
        // ReplayGain is exposed ONLY through the info section
        // (replaygain_info), like every other format's reader: adding the
        // REPLAYGAIN_* keys as meta makes them show up in the Metadata tab,
        // which FLAC/MP3 readers do not do.
        if (replaygain_info::g_is_meta_replaygain(key)) {
            rg.set_from_meta(key, value);
        } else {
            info.meta_add(key, value);
        }
    }
    if (rg.is_track_gain_present() || rg.is_album_gain_present()
        || rg.is_track_peak_present() || rg.is_album_peak_present()) {
        info.set_replaygain(rg);
    }
    pfc::string8 dbg;
    dbg << "foo_input_sena: read back " << n << " user tags";
    console::print(dbg);
    sena_tags_close(tags);
    m_file->reopen(abort);
}

void input_sena::ensure_decoder(abort_callback &abort) {
    if (!m_dec) {
        open_decoder(abort);
    }
}

int64_t input_sena::io_read(void *ctx, void *buf, uint64_t len) {
    auto *self = static_cast<input_sena *>(ctx);
    try {
        return self->m_file->read(buf, (t_size)len, *self->m_abort);
    } catch (...) {
        return -1;
    }
}

int64_t input_sena::io_seek(void *ctx, int64_t offset, int whence) {
    auto *self = static_cast<input_sena *>(ctx);
    try {
        self->m_file->seek_ex((t_sfilesize)offset, (file::t_seek_mode)whence, *self->m_abort);
        return 0;
    } catch (...) {
        return -1;
    }
}

int64_t input_sena::io_tell(void *ctx) {
    auto *self = static_cast<input_sena *>(ctx);
    try {
        return (int64_t)self->m_file->get_position(*self->m_abort);
    } catch (...) {
        return -1;
    }
}

uint64_t input_sena::io_size(void *ctx) {
    auto *self = static_cast<input_sena *>(ctx);
    try {
        t_filesize size = self->m_file->get_size(*self->m_abort);
        return size == filesize_invalid ? UINT64_MAX : (uint64_t)size;
    } catch (...) {
        return UINT64_MAX;
    }
}

int64_t input_sena::io_write(void *ctx, const void *buf, uint64_t len) {
    auto *self = static_cast<input_sena *>(ctx);
    try {
        self->m_file->write(buf, (t_size)len, *self->m_abort);
        return (int64_t)len;
    } catch (...) {
        return -1;
    }
}

static input_singletrack_factory_t<input_sena> g_input_sena_factory;

DECLARE_FILE_TYPE("Sena files", "*.SENA;*.MKA");
