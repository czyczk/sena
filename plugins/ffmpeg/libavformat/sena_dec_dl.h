/*
 * Runtime loader for the sena-dec core library (sena demuxer backend)
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * The Rust decoder core is loaded at runtime (dlopen / LoadLibrary) instead
 * of being linked into libavformat: the FFmpeg build needs no Rust toolchain
 * and a sena-dec update is a drop-in library replacement.
 */

#ifndef AVFORMAT_SENA_DEC_DL_H
#define AVFORMAT_SENA_DEC_DL_H

#include "sena_dec.h"

typedef struct SenaDecAPI {
    int  (*sena_dec_open)(const SenaDecIo *io, SenaDec **out, char *err, size_t err_len);
    void (*sena_dec_close)(SenaDec *dec);
    int  (*sena_dec_get_info)(SenaDec *dec, SenaDecInfo *info);
    int  (*sena_dec_read_f32)(SenaDec *dec, float *interleaved, uint64_t frames, uint64_t *out_frames);
    int  (*sena_dec_seek)(SenaDec *dec, uint64_t frame);
    const char *(*sena_dec_version)(void);
    int  (*sena_file_read_tags)(const SenaFileIo *io, SenaTags **out, char *err, size_t err_len);
    uint32_t (*sena_tags_count)(const SenaTags *tags);
    const char *(*sena_tags_key)(const SenaTags *tags, uint32_t index);
    const char *(*sena_tags_value)(const SenaTags *tags, uint32_t index);
    void (*sena_tags_close)(SenaTags *tags);
    int  (*sena_file_art_read)(const SenaFileIo *io, SenaArtHandle **out, char *err, size_t err_len);
    uint32_t (*sena_art_count)(const SenaArtHandle *handle);
    const char *(*sena_art_name)(const SenaArtHandle *handle, uint32_t index);
    const char *(*sena_art_mime)(const SenaArtHandle *handle, uint32_t index);
    const uint8_t *(*sena_art_data)(const SenaArtHandle *handle, uint32_t index);
    size_t (*sena_art_data_len)(const SenaArtHandle *handle, uint32_t index);
    void (*sena_art_close)(SenaArtHandle *handle);
} SenaDecAPI;

/* Process-wide, thread-safe access to the loaded core. Returns NULL when the
 * library could not be found; ff_sena_dec_load_error() then describes why.
 * The library is never unloaded: decoder handles may outlive any single
 * demuxer instance. */
const SenaDecAPI *ff_sena_dec_api(void);
const char *ff_sena_dec_load_error(void);

#endif /* AVFORMAT_SENA_DEC_DL_H */
