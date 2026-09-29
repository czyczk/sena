/*
 * Runtime loader for the sena-dec core library (Sena decoder backend)
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * The Rust decoder core is loaded at runtime (dlopen / LoadLibrary) instead
 * of being linked into libavcodec: the FFmpeg build needs no Rust toolchain
 * and a sena-dec update is a drop-in library replacement.
 *
 * Search order for the core library:
 *   1. $SENA_DEC_LIBRARY        - full path override (development, testing)
 *   2. <dir of this module>     - the avcodec-*.dll location, which is also
 *                                 where LAV Filters keeps its DLLs
 *   3. platform default search  - dlopen(soname) / LoadLibrary(name)
 */

#ifndef AVCODEC_SENA_DEC_DL_H
#define AVCODEC_SENA_DEC_DL_H

#include "sena_dec.h"

typedef struct SenaDecAPI {
    const char *(*sena_dec_version)(void);
    int  (*sena_es_open)(const uint8_t *init_blob, size_t init_blob_len, SenaEs **out, char *err, size_t err_len);
    int  (*sena_es_push)(SenaEs *es, const uint8_t *block, size_t block_len, uint64_t ts_ns);
    int  (*sena_es_pull)(SenaEs *es, float *interleaved, uint64_t frames, uint64_t *out_frames);
    void (*sena_es_finish)(SenaEs *es);
    void (*sena_es_reset)(SenaEs *es);
    void (*sena_es_set_output_start)(SenaEs *es, uint64_t frame);
    int  (*sena_es_warmup_seek)(SenaEs *es, const char *path, uint64_t target_frame);
    int  (*sena_es_get_info)(SenaEs *es, SenaDecInfo *info);
    int  (*sena_es_get_read_info)(SenaEs *es, SenaDecReadInfo *info);
    int  (*sena_es_get_stats)(SenaEs *es, SenaEsStats *stats);
    void (*sena_es_close)(SenaEs *es);
} SenaDecAPI;

/* Process-wide, thread-safe access to the loaded core. Returns NULL when the
 * library could not be found; ff_sena_dec_load_error() then describes why.
 * The library is never unloaded: decoder handles may outlive any single
 * decoder instance. */
const SenaDecAPI *ff_sena_dec_api(void);
const char *ff_sena_dec_load_error(void);

#endif /* AVCODEC_SENA_DEC_DL_H */
