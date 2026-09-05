// Sena album art: foobar2000 album_art_extractor / album_art_editor backed
// by Matroska Attachments.
//
// foobar2000 exposes attached-picture editing exclusively through the
// album_art_editor service (the "Attached picture editing is not supported
// for this file type" error means no registered editor matched the path);
// the input_info_writer / PICTURE-meta path is not used for pictures. In
// Matroska, pictures live in the top-level Attachments element, not in
// string tags, so this shim maps foobar's art GUIDs to attachment file
// names and reads/writes them through the shared sena-dec C ABI.

#include "stdafx.h"
#include "sena_dec.h"
#include "input_sena.h"
#include <SDK/album_art.h>
#include <SDK/album_art_helpers.h>

namespace {

// ---------------------------------------------------------------- helpers
pfc::string8 guid_art_name(const GUID & g) {
    if (g == album_art_ids::cover_front) return "cover_front.jpg";
    if (g == album_art_ids::cover_back) return "cover_back.jpg";
    if (g == album_art_ids::disc) return "disc.jpg";
    if (g == album_art_ids::icon) return "icon.jpg";
    if (g == album_art_ids::artist) return "artist.jpg";
    const char * n = album_art_ids::name_of(g);
    pfc::string8 out;
    out << "art_" << (n ? n : "other") << ".jpg";
    return out;
}

const char * sniff_mime(const uint8_t * d, size_t n) {
    if (n >= 3 && d[0] == 0xFF && d[1] == 0xD8 && d[2] == 0xFF) return "image/jpeg";
    if (n >= 8 && d[0] == 0x89 && d[1] == 0x50 && d[2] == 0x4E && d[3] == 0x47) return "image/png";
    if (n >= 6 && d[0] == 'G' && d[1] == 'I' && d[2] == 'F') return "image/gif";
    if (n >= 2 && d[0] == 'B' && d[1] == 'M') return "image/bmp";
    if (n >= 12 && d[0] == 'R' && d[1] == 'I' && d[2] == 'F' && d[3] == 'F' && d[8] == 'W' && d[9] == 'E' && d[10] == 'B' && d[11] == 'P') return "image/webp";
    return "application/octet-stream";
}

// ------------------------------------------------------------ file IO glue
struct art_entry {
    pfc::string8 name, mime;
    pfc::array_t<uint8_t> data;
};

struct io_ctx {
    service_ptr_t<file> f;
    abort_callback * abort;
};
int64_t cb_read(void * ctx, void * buf, uint64_t len) {
    auto c = static_cast<io_ctx*>(ctx);
    try { return c->f->read(buf, (t_size)len, *c->abort); } catch (...) { return -1; }
}
int64_t cb_write(void * ctx, const void * buf, uint64_t len) {
    auto c = static_cast<io_ctx*>(ctx);
    try { c->f->write(buf, (t_size)len, *c->abort); return (int64_t)len; } catch (...) { return -1; }
}
int64_t cb_seek(void * ctx, int64_t offset, int whence) {
    auto c = static_cast<io_ctx*>(ctx);
    try { c->f->seek_ex((t_sfilesize)offset, (file::t_seek_mode)whence, *c->abort); return 0; } catch (...) { return -1; }
}
int64_t cb_tell(void * ctx) {
    auto c = static_cast<io_ctx*>(ctx);
    try { return (int64_t)c->f->get_position(*c->abort); } catch (...) { return -1; }
}
uint64_t cb_size(void * ctx) {
    auto c = static_cast<io_ctx*>(ctx);
    try { t_filesize s = c->f->get_size(*c->abort); return s == filesize_invalid ? UINT64_MAX : (uint64_t)s; } catch (...) { return UINT64_MAX; }
}

bool load_entries(service_ptr_t<file> f, abort_callback & abort, pfc::list_t<art_entry> & out) {
    io_ctx ctx = { f, &abort };
    SenaFileIo io{};
    io.user_data = &ctx;
    io.read = cb_read; io.write = cb_write; io.seek = cb_seek; io.tell = cb_tell; io.size = cb_size;
    SenaArtHandle * list = nullptr;
    char err[256] = {};
    int rc = sena_file_art_read(&io, &list, err, sizeof(err));
    if (rc != SENA_DEC_OK) return false;
    uint32_t n = sena_art_count(list);
    for (uint32_t i = 0; i < n; i++) {
        art_entry e;
        const char * name = sena_art_name(list, i);
        const char * mime = sena_art_mime(list, i);
        const uint8_t * data = sena_art_data(list, i);
        size_t len = sena_art_data_len(list, i);
        if (!name) continue;
        e.name = name;
        e.mime = mime ? mime : "application/octet-stream";
        e.data.set_size(len);
        if (len) memcpy(e.data.get_ptr(), data, len);
        out.add_item(e);
    }
    sena_art_close(list);
    return true;
}

bool save_entries(service_ptr_t<file> f, abort_callback & abort, const pfc::list_t<art_entry> & entries) {
    io_ctx ctx = { f, &abort };
    SenaFileIo io{};
    io.user_data = &ctx;
    io.read = cb_read; io.write = cb_write; io.seek = cb_seek; io.tell = cb_tell; io.size = cb_size;
    pfc::list_t<SenaArtInput> inputs;
    for (t_size i = 0; i < entries.get_count(); i++) {
        const art_entry & e = entries[i];
        SenaArtInput in;
        in.name = e.name.get_ptr();
        in.mime = e.mime.get_ptr();
        in.data = e.data.get_ptr();
        in.data_len = e.data.get_size();
        inputs.add_item(in);
    }
    char err[256] = {};
    int rc = sena_file_art_write(&io, inputs.get_ptr(), (uint32_t)inputs.get_count(), err, sizeof(err));
    if (rc != SENA_DEC_OK) {
        pfc::string8 msg;
        msg << "foo_input_sena: album art write failed: " << (err[0] ? err : "unknown");
        console::print(msg);
    }
    return rc == SENA_DEC_OK;
}

// ------------------------------------------------------------ editor/extractor instance
class sena_art_instance : public album_art_editor_instance_v2 {
public:
    service_ptr_t<file> m_file;
    pfc::string8 m_path;
    pfc::list_t<art_entry> m_entries;
    bool m_loaded = false;

    void ensure_loaded(abort_callback & abort) {
        if (!m_loaded) {
            m_file->reopen(abort);
            m_loaded = load_entries(m_file, abort, m_entries);
        }
    }

    // album_art_extractor_instance
    album_art_data_ptr query(const GUID & what, abort_callback & abort) override {
        ensure_loaded(abort);
        pfc::string8 want = guid_art_name(what);
        for (t_size i = 0; i < m_entries.get_count(); i++) {
            if (stricmp_utf8(m_entries[i].name, want) == 0) {
                return album_art_data_impl::g_create(m_entries[i].data.get_ptr(), m_entries[i].data.get_size());
            }
        }
        throw exception_album_art_not_found();
    }

    // album_art_editor_instance
    void set(const GUID & what, album_art_data_ptr data, abort_callback & abort) override {
        ensure_loaded(abort);
        pfc::string8 name = guid_art_name(what);
        const void * bytes = data->data();
        size_t len = data->size();
        for (t_size i = 0; i < m_entries.get_count(); i++) {
            if (stricmp_utf8(m_entries[i].name, name) == 0) {
                art_entry & e = m_entries[i];
                e.data.set_size(len);
                if (len) memcpy(e.data.get_ptr(), bytes, len);
                e.mime = sniff_mime((const uint8_t*)bytes, len);
                return;
            }
        }
        art_entry e;
        e.name = name;
        e.mime = sniff_mime((const uint8_t*)bytes, len);
        e.data.set_size(len);
        if (len) memcpy(e.data.get_ptr(), bytes, len);
        m_entries.add_item(e);
    }

    void remove(const GUID & what) override {
        abort_callback_impl do_nothing;
        ensure_loaded(do_nothing);
        pfc::string8 name = guid_art_name(what);
        for (t_size i = 0; i < m_entries.get_count(); i++) {
            if (stricmp_utf8(m_entries[i].name, name) == 0) {
                m_entries.remove_by_idx(i);
                return;
            }
        }
    }

    void commit(abort_callback & abort) override {
        m_file->reopen(abort);
        save_entries(m_file, abort, m_entries);
    }

    void remove_all() override {
        m_entries.remove_all();
    }
};

// ------------------------------------------------------------ extractor/editor entrypoints
class sena_art_extractor : public album_art_extractor_v2 {
public:
    bool is_our_path(const char *, const char * extension) override {
        return stricmp_utf8(extension, "sena") == 0 || stricmp_utf8(extension, "mka") == 0;
    }
    album_art_extractor_instance_ptr open(file_ptr hint, const char * path, abort_callback & abort) override {
        auto inst = new service_impl_t<sena_art_instance>();
        inst->m_file = hint;
        input_open_file_helper(inst->m_file, path, input_open_info_read, abort);
        return inst;
    }
    GUID get_guid() override { return input_sena::g_get_guid(); }
};

class sena_art_editor : public album_art_editor_v2 {
public:
    bool is_our_path(const char *, const char * extension) override {
        return stricmp_utf8(extension, "sena") == 0 || stricmp_utf8(extension, "mka") == 0;
    }
    album_art_editor_instance_ptr open(file_ptr hint, const char * path, abort_callback & abort) override {
        auto inst = new service_impl_t<sena_art_instance>();
        inst->m_file = hint;
        input_open_file_helper(inst->m_file, path, input_open_info_write, abort);
        return inst;
    }
    GUID get_guid() override { return input_sena::g_get_guid(); }
};

static service_factory_single_t<sena_art_extractor> g_sena_art_extractor;
static service_factory_single_t<sena_art_editor> g_sena_art_editor;

} // namespace
