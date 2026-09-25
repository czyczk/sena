/*
 * Runtime loader for the sena-dec core library (sena demuxer backend)
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * Search order for the core library:
 *   1. $SENA_DEC_LIBRARY        - full path override (development, testing)
 *   2. <dir of this module>     - libavformat/avformat-*.dll location, which
 *                                 is also where LAV Filters keeps its DLLs
 *   3. platform default search  - dlopen(soname) / LoadLibrary(name)
 */

#ifndef _GNU_SOURCE
#define _GNU_SOURCE /* dladdr */
#endif

#include <stdarg.h>
#include <stddef.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "libavutil/macros.h"
#include "libavutil/thread.h"

#include "sena_dec_dl.h"

#ifdef _WIN32
#include <windows.h>
#else
#include <dlfcn.h>
#endif

#define SENA_DEC_ENV_VAR "SENA_DEC_LIBRARY"
#ifdef _WIN32
#define SENA_DEC_LIBNAME "sena_dec.dll"
#elif defined(__APPLE__)
#define SENA_DEC_LIBNAME "libsena_dec.dylib"
#else
#define SENA_DEC_LIBNAME "libsena_dec.so"
#endif

static SenaDecAPI sena_api;
static char sena_load_error[256];
static AVOnce sena_load_once = AV_ONCE_INIT;

static void sena_set_error(const char *fmt, ...)
{
    va_list ap;

    if (sena_load_error[0])
        return;
    va_start(ap, fmt);
    vsnprintf(sena_load_error, sizeof(sena_load_error), fmt, ap);
    va_end(ap);
}

static void *sena_open_module(const char *path)
{
#ifdef _WIN32
    return (void *)LoadLibraryA(path);
#else
    return dlopen(path, RTLD_NOW | RTLD_LOCAL);
#endif
}

static void *sena_sym(void *mod, const char *name)
{
#ifdef _WIN32
    return (void *)GetProcAddress((HMODULE)mod, name);
#else
    return dlsym(mod, name);
#endif
}

/* Try <dir of the module containing this code>/<libname>. This makes the
 * demuxer work when the core library is deployed next to libavformat (the
 * LAV Filters layout) without touching PATH / LD_LIBRARY_PATH. */
static void *sena_open_module_relative(void)
{
    char path[4096];
#ifdef _WIN32
    HMODULE hm = NULL;
    DWORD n;
    char *slash;

    if (!GetModuleHandleExA(GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS |
                            GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
                            (LPCSTR)&sena_open_module_relative, &hm))
        return NULL;
    n = GetModuleFileNameA(hm, path, sizeof(path));
    if (!n || n >= sizeof(path))
        return NULL;
    slash = strrchr(path, '\\');
    if (!slash)
        return NULL;
    slash[1] = '\0';
    if (strlen(path) + strlen(SENA_DEC_LIBNAME) >= sizeof(path))
        return NULL;
    strcat(path, SENA_DEC_LIBNAME);
#else
    Dl_info di;
    char *sep;

    if (!dladdr((void *)&sena_open_module_relative, &di) || !di.dli_fname)
        return NULL;
    if (strlen(di.dli_fname) + strlen(SENA_DEC_LIBNAME) + 2 > sizeof(path))
        return NULL;
    strcpy(path, di.dli_fname);
    sep = strrchr(path, '/');
    if (!sep)
        return NULL;
    sep[1] = '\0';
    strcat(path, SENA_DEC_LIBNAME);
#endif
    return sena_open_module(path);
}

static void sena_load_once_fn(void)
{
    static const struct {
        const char *name;
        size_t offset;
    } syms[] = {
#define SENA_SYM(field) { #field, offsetof(SenaDecAPI, field) }
        SENA_SYM(sena_dec_open),
        SENA_SYM(sena_dec_close),
        SENA_SYM(sena_dec_get_info),
        SENA_SYM(sena_dec_read_f32),
        SENA_SYM(sena_dec_seek),
        SENA_SYM(sena_dec_version),
        SENA_SYM(sena_file_read_tags),
        SENA_SYM(sena_tags_count),
        SENA_SYM(sena_tags_key),
        SENA_SYM(sena_tags_value),
        SENA_SYM(sena_tags_close),
        SENA_SYM(sena_file_art_read),
        SENA_SYM(sena_art_count),
        SENA_SYM(sena_art_name),
        SENA_SYM(sena_art_mime),
        SENA_SYM(sena_art_data),
        SENA_SYM(sena_art_data_len),
        SENA_SYM(sena_art_close),
#undef SENA_SYM
    };
    void *mod = NULL;
    const char *env = getenv(SENA_DEC_ENV_VAR);
    size_t i;

    memset(&sena_api, 0, sizeof(sena_api));

    if (env && env[0]) {
        mod = sena_open_module(env);
        if (!mod)
            sena_set_error("cannot load " SENA_DEC_ENV_VAR "=%s", env);
    }
    if (!mod)
        mod = sena_open_module_relative();
    if (!mod)
        mod = sena_open_module(SENA_DEC_LIBNAME);
    if (!mod) {
        sena_set_error("%s not found (set " SENA_DEC_ENV_VAR " or install it next to libavformat)", SENA_DEC_LIBNAME);
        return;
    }

    for (i = 0; i < FF_ARRAY_ELEMS(syms); i++) {
        void *sym = sena_sym(mod, syms[i].name);
        if (!sym) {
            sena_set_error("%s: missing symbol %s", SENA_DEC_LIBNAME, syms[i].name);
            memset(&sena_api, 0, sizeof(sena_api));
            return;
        }
        memcpy((char *)&sena_api + syms[i].offset, &sym, sizeof(sym));
    }
}

const SenaDecAPI *ff_sena_dec_api(void)
{
    ff_thread_once(&sena_load_once, sena_load_once_fn);
    return sena_api.sena_dec_open ? &sena_api : NULL;
}

const char *ff_sena_dec_load_error(void)
{
    ff_thread_once(&sena_load_once, sena_load_once_fn);
    return sena_load_error;
}
