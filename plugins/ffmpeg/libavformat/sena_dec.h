#pragma once

#ifndef SENA_DEC_H
#define SENA_DEC_H

#include <stdint.h>
#include <stddef.h>

#ifndef SENA_DEC_API
#ifdef _WIN32
#define SENA_DEC_API __declspec(dllimport)
#else
#define SENA_DEC_API __attribute__((visibility("default")))
#endif
#endif

#ifdef __cplusplus
extern "C" {
#endif

#define SENA_DEC_ABI_VERSION 1

typedef struct SenaDec SenaDec;
typedef struct SenaTags SenaTags;
typedef struct SenaArtHandle SenaArtHandle;

typedef struct {
    void    *user_data;
    int64_t (*read)(void *user_data, void *buf, uint64_t len);
    int64_t (*seek)(void *user_data, int64_t offset, int whence);
    int64_t (*tell)(void *user_data);
    uint64_t (*size)(void *user_data);
} SenaDecIo;

typedef struct {
    void    *user_data;
    int64_t (*read)(void *user_data, void *buf, uint64_t len);
    int64_t (*write)(void *user_data, const void *buf, uint64_t len);
    int64_t (*seek)(void *user_data, int64_t offset, int whence);
    int64_t (*tell)(void *user_data);
    uint64_t (*size)(void *user_data);
} SenaFileIo;

typedef struct {
    uint32_t sample_rate;
    uint32_t channels;
    uint64_t playable_frames;
    uint32_t profile;
    uint32_t sena_version;
    char audio_sha256[65]; /* SENA_AUDIO_SHA256, 64 hex + NUL; empty if absent */
    /* Encoded size of all Cluster elements (codec payloads plus block
       framing). Tags, Attachments (cover art) and Void filler are excluded:
       average-bitrate displays must use this, not the raw file size. */
    uint64_t audio_span_bytes;
} SenaDecInfo;

typedef struct {
    uint64_t start_frame;
    uint64_t frames;
    uint64_t payload_bits;
} SenaDecReadInfo;

typedef struct {
    const char *key;
    const char *value;
} SenaMetaEntry;

typedef struct {
    const char *name;      /* attachment file name, e.g. "cover_front.jpg" */
    const char *mime;      /* e.g. "image/jpeg" */
    const uint8_t *data;   /* binary payload */
    size_t data_len;
} SenaArtInput;

#define SENA_DEC_OK 0
#define SENA_DEC_ERR_IO (-1)
#define SENA_DEC_ERR_FORMAT (-2)
#define SENA_DEC_ERR_UNSUPPORTED (-3)
#define SENA_DEC_ERR_DECODE (-4)
#define SENA_DEC_ERR_INVALID (-5)
#define SENA_DEC_EOF (-6)

int  sena_dec_open(const SenaDecIo *io, SenaDec **out, char *err, size_t err_len);
void sena_dec_close(SenaDec *dec);
int  sena_dec_get_info(SenaDec *dec, SenaDecInfo *info);
int  sena_dec_probe_info(const SenaDecIo *io, SenaDecInfo *info, char *err, size_t err_len);
int  sena_dec_read_f32(SenaDec *dec, float *interleaved, uint64_t frames, uint64_t *out_frames);
int  sena_dec_get_read_info(SenaDec *dec, SenaDecReadInfo *info);
int  sena_dec_seek(SenaDec *dec, uint64_t frame);
const char *sena_dec_version(void);

int  sena_file_write_tags(const SenaFileIo *io, const SenaMetaEntry *entries, uint32_t count, char *err, size_t err_len);
int  sena_file_remove_tags(const SenaFileIo *io, char *err, size_t err_len);
int  sena_file_read_tags(const SenaFileIo *io, SenaTags **out, char *err, size_t err_len);
uint32_t sena_tags_count(const SenaTags *tags);
const char *sena_tags_key(const SenaTags *tags, uint32_t index);
const char *sena_tags_value(const SenaTags *tags, uint32_t index);
void sena_tags_close(SenaTags *tags);

/* ---------------------------------------------------------------- album art
   Matroska Attachments access (foobar2000 album_art_editor backend). */
SENA_DEC_API int  sena_file_art_read(const SenaFileIo *io, SenaArtHandle **out, char *err, size_t err_len);
SENA_DEC_API uint32_t sena_art_count(const SenaArtHandle *handle);
SENA_DEC_API const char *sena_art_name(const SenaArtHandle *handle, uint32_t index);
SENA_DEC_API const char *sena_art_mime(const SenaArtHandle *handle, uint32_t index);
SENA_DEC_API const uint8_t *sena_art_data(const SenaArtHandle *handle, uint32_t index);
SENA_DEC_API size_t sena_art_data_len(const SenaArtHandle *handle, uint32_t index);
SENA_DEC_API void sena_art_close(SenaArtHandle *handle);
/* Replaces the full attachment set (empty list removes all attachments). */
SENA_DEC_API int  sena_file_art_write(const SenaFileIo *io, const SenaArtInput *entries, uint32_t count, char *err, size_t err_len);

#ifdef __cplusplus
}
#endif

#endif
