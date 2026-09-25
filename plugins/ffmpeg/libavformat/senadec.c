/*
 * Sena dual-track audio demuxer
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * A Sena file is Matroska with a private low-band xHE-AAC track (A_SENALF)
 * plus a high-band Opus track; the playable stream is the mixed, trimmed
 * 48 kHz stereo sum of both. The whole chain lives in the sena-dec Rust
 * core (loaded at runtime, see sena_dec_dl.c); this demuxer is a thin
 * adapter that presents the decoded output as a single PCM f32 stream,
 * the same way the libopenmpt/libgme demuxers expose their decoders.
 */

#include "libavutil/avstring.h"
#include "libavutil/buffer.h"
#include "libavutil/channel_layout.h"
#include "libavutil/dict.h"
#include "libavutil/intreadwrite.h"
#include "libavutil/macros.h"
#include "libavutil/mathematics.h"
#include "libavutil/mem.h"

#include "avformat.h"
#include "demux.h"
#include "internal.h"

#include "sena_dec_dl.h"
#include "sena_probe.h"

#define SENA_SAMPLE_RATE 48000
#define SENA_CHANNELS 2
/* Frames per emitted packet: 1024 = ~21 ms at 48 kHz. */
#define SENA_PACKET_FRAMES 1024

typedef struct SenaDemuxContext {
    const AVClass *class;
    const SenaDecAPI *api;   /* resolved once in read_header */
    SenaDec *dec;            /* Rust decoder handle */
    uint64_t cur_frame;      /* pts bookkeeping, in 48 kHz frames */
    uint64_t playable_frames;
} SenaDemuxContext;

/* ------------------------------------------------- AVIOContext trampolines
 * whence follows the C ABI contract: 0 = set, 1 = current, 2 = end. */

static int64_t sena_io_read(void *opaque, void *buf, uint64_t len)
{
    AVIOContext *pb = opaque;
    int ret = avio_read(pb, buf, (int)FFMIN(len, INT_MAX));
    if (ret == AVERROR_EOF)
        return 0;
    return ret; /* partial count or negative error, both match the ABI */
}

static int64_t sena_io_seek(void *opaque, int64_t offset, int whence)
{
    AVIOContext *pb = opaque;
    int w = whence == 0 ? SEEK_SET : whence == 1 ? SEEK_CUR : SEEK_END;
    int64_t ret = avio_seek(pb, offset, w);
    return ret < 0 ? ret : 0; /* the core treats < 0 as failure */
}

static int64_t sena_io_tell(void *opaque)
{
    return avio_tell((AVIOContext *)opaque);
}

static uint64_t sena_io_size(void *opaque)
{
    int64_t size = avio_size((AVIOContext *)opaque);
    return size > 0 ? (uint64_t)size : 0;
}

/* --------------------------------------------------------------- demuxer */

/* Image mime -> codec mapping for Matroska attachments (cover art). */
static enum AVCodecID sena_art_codec_id(const char *mime)
{
    static const struct {
        const char *mime;
        enum AVCodecID id;
    } map[] = {
        { "image/jpeg", AV_CODEC_ID_MJPEG },
        { "image/png",  AV_CODEC_ID_PNG   },
        { "image/webp", AV_CODEC_ID_WEBP  },
        { "image/bmp",  AV_CODEC_ID_BMP   },
        { "image/gif",  AV_CODEC_ID_GIF   },
    };
    int i;

    for (i = 0; i < FF_ARRAY_ELEMS(map); i++)
        if (av_strstart(mime, map[i].mime, NULL))
            return map[i].id;
    return AV_CODEC_ID_NONE;
}

static void sena_read_attachments(AVFormatContext *s)
{
    SenaDemuxContext *c = s->priv_data;
    SenaArtHandle *art = NULL;
    SenaFileIo io = {
        .user_data = s->pb,
        .read      = sena_io_read,
        .write     = NULL,
        .seek      = sena_io_seek,
        .tell      = sena_io_tell,
        .size      = sena_io_size,
    };
    char err[128] = { 0 };
    uint32_t i, n;

    if (!(s->pb->seekable & AVIO_SEEKABLE_NORMAL))
        return;
    if (c->api->sena_file_art_read(&io, &art, err, sizeof(err)) != SENA_DEC_OK || !art)
        return;
    n = c->api->sena_art_count(art);
    for (i = 0; i < n; i++) {
        const char *mime  = c->api->sena_art_mime(art, i);
        const char *name  = c->api->sena_art_name(art, i);
        const uint8_t *data = c->api->sena_art_data(art, i);
        size_t len          = c->api->sena_art_data_len(art, i);
        enum AVCodecID id   = mime ? sena_art_codec_id(mime) : AV_CODEC_ID_NONE;
        AVBufferRef *bref;
        AVStream *st;
        int ret;

        if (id == AV_CODEC_ID_NONE || !data || !len)
            continue;
        st = avformat_new_stream(s, NULL);
        if (!st)
            break;
        st->codecpar->codec_id = id;
        if (name)
            av_dict_set(&st->metadata, "filename", name, 0);
        av_dict_set(&st->metadata, "mimetype", mime, 0);
        bref = av_buffer_allocz(len + AV_INPUT_BUFFER_PADDING_SIZE);
        if (!bref)
            break;
        memcpy(bref->data, data, len);
        ret = ff_add_attached_pic(s, st, NULL, &bref, 0);
        if (ret < 0) {
            av_buffer_unref(&bref);
            break;
        }
    }
    c->api->sena_art_close(art);
}

static int sena_probe(const AVProbeData *p)
{
    /* Content probe only: a .sena/.mka extension alone is not enough, and
     * plain Matroska files must keep going to the matroska demuxer. */
    if (!ff_sena_probe_match(p->buf, p->buf_size))
        return 0;
    return AVPROBE_SCORE_MAX;
}

static int sena_error(int rc)
{
    switch (rc) {
    case SENA_DEC_ERR_IO:          return AVERROR(EIO);
    case SENA_DEC_ERR_UNSUPPORTED: return AVERROR_PATCHWELCOME;
    default:                       return AVERROR_INVALIDDATA;
    }
}

static void sena_read_metadata(AVFormatContext *s, const SenaDecInfo *info)
{
    SenaDemuxContext *c = s->priv_data;
    SenaTags *tags = NULL;
    SenaFileIo io = {
        .user_data = s->pb,
        .read      = sena_io_read,
        .write     = NULL,
        .seek      = sena_io_seek,
        .tell      = sena_io_tell,
        .size      = sena_io_size,
    };
    char err[128] = { 0 };
    uint32_t i, n;

    av_dict_set_int(&s->metadata, "SENA_PROFILE", info->profile, 0);
    av_dict_set_int(&s->metadata, "SENA_VERSION", info->sena_version, 0);
    av_dict_set_int(&s->metadata, "SENA_PLAYABLE_SAMPLES", (int64_t)info->playable_frames, 0);
    if (info->audio_sha256[0])
        av_dict_set(&s->metadata, "SENA_AUDIO_SHA256", info->audio_sha256, 0);

    /* User tags live at the Segment tail: they need positioned reads. */
    if (!(s->pb->seekable & AVIO_SEEKABLE_NORMAL))
        return;
    if (c->api->sena_file_read_tags(&io, &tags, err, sizeof(err)) != SENA_DEC_OK || !tags)
        return;
    n = c->api->sena_tags_count(tags);
    for (i = 0; i < n; i++) {
        const char *key = c->api->sena_tags_key(tags, i);
        const char *value = c->api->sena_tags_value(tags, i);
        if (key && value)
            av_dict_set(&s->metadata, key, value, 0);
    }
    c->api->sena_tags_close(tags);
}

static int sena_read_header(AVFormatContext *s)
{
    SenaDemuxContext *c = s->priv_data;
    SenaDecIo io = {
        .user_data = s->pb,
        .read      = sena_io_read,
        .seek      = sena_io_seek,
        .tell      = sena_io_tell,
        .size      = sena_io_size,
    };
    SenaDecInfo info;
    AVStream *st;
    char err[256] = { 0 };
    int rc;

    c->api = ff_sena_dec_api();
    if (!c->api) {
        av_log(s, AV_LOG_ERROR, "sena: decoder core unavailable: %s\n",
               ff_sena_dec_load_error());
        return AVERROR_EXTERNAL;
    }

    rc = c->api->sena_dec_open(&io, &c->dec, err, sizeof(err));
    if (rc != SENA_DEC_OK) {
        av_log(s, AV_LOG_ERROR, "sena: cannot open: %s\n", err);
        return sena_error(rc);
    }
    rc = c->api->sena_dec_get_info(c->dec, &info);
    if (rc != SENA_DEC_OK || info.sample_rate != SENA_SAMPLE_RATE ||
        info.channels != SENA_CHANNELS || !info.playable_frames) {
        av_log(s, AV_LOG_ERROR, "sena: unexpected stream layout\n");
        return AVERROR_INVALIDDATA;
    }
    c->playable_frames = info.playable_frames;
    c->cur_frame       = 0;

    st = avformat_new_stream(s, NULL);
    if (!st)
        return AVERROR(ENOMEM);
    st->codecpar->codec_type = AVMEDIA_TYPE_AUDIO;
    st->codecpar->codec_id   = AV_NE(AV_CODEC_ID_PCM_F32BE, AV_CODEC_ID_PCM_F32LE);
    st->codecpar->sample_rate = SENA_SAMPLE_RATE;
    st->codecpar->ch_layout  = (AVChannelLayout)AV_CHANNEL_LAYOUT_STEREO;
    st->codecpar->bit_rate   = av_rescale(info.audio_span_bytes * 8, SENA_SAMPLE_RATE,
                                          info.playable_frames);
    avpriv_set_pts_info(st, 64, 1, SENA_SAMPLE_RATE);
    st->start_time = 0;
    st->duration   = info.playable_frames;
    s->duration    = av_rescale(info.playable_frames, AV_TIME_BASE, SENA_SAMPLE_RATE);

    sena_read_metadata(s, &info);
    sena_read_attachments(s);
    return 0;
}

static int sena_read_packet(AVFormatContext *s, AVPacket *pkt)
{
    SenaDemuxContext *c = s->priv_data;
    uint64_t got = 0;
    int rc, ret;

    ret = av_new_packet(pkt, SENA_PACKET_FRAMES * SENA_CHANNELS * sizeof(float));
    if (ret < 0)
        return ret;

    rc = c->api->sena_dec_read_f32(c->dec, (float *)pkt->data,
                                   SENA_PACKET_FRAMES, &got);
    if (rc != SENA_DEC_OK && rc != SENA_DEC_EOF)
        return sena_error(rc);
    if (!got)
        return AVERROR_EOF;

    pkt->size         = got * SENA_CHANNELS * sizeof(float);
    pkt->pts          = c->cur_frame;
    pkt->dts          = c->cur_frame;
    pkt->duration     = got;
    pkt->stream_index = 0;
    c->cur_frame     += got;
    return 0;
}

static int sena_read_seek(AVFormatContext *s, int stream_index,
                          int64_t ts, int flags)
{
    SenaDemuxContext *c = s->priv_data;
    uint64_t frame;
    int rc;

    if (flags & AVSEEK_FLAG_BYTE)
        return AVERROR(ENOSYS);
    if (!(s->pb->seekable & AVIO_SEEKABLE_NORMAL))
        return AVERROR(ENOSYS);

    frame = ts < 0 ? 0 : (uint64_t)ts;
    if (frame >= c->playable_frames)
        frame = c->playable_frames - 1;

    rc = c->api->sena_dec_seek(c->dec, frame);
    if (rc != SENA_DEC_OK)
        return sena_error(rc);
    c->cur_frame = frame;
    return 0;
}

static int sena_read_close(AVFormatContext *s)
{
    SenaDemuxContext *c = s->priv_data;
    if (c->dec) {
        c->api->sena_dec_close(c->dec);
        c->dec = NULL;
    }
    return 0;
}

const FFInputFormat ff_sena_demuxer = {
    .p.name         = "sena",
    .p.long_name    = NULL_IF_CONFIG_SMALL("Sena dual-track audio"),
    .p.extensions   = "sena,mka",
    .p.flags        = AVFMT_NOBINSEARCH | AVFMT_NOGENSEARCH | AVFMT_NO_BYTE_SEEK,
    .priv_data_size = sizeof(SenaDemuxContext),
    .flags_internal = FF_INFMT_FLAG_INIT_CLEANUP,
    .read_probe     = sena_probe,
    .read_header    = sena_read_header,
    .read_packet    = sena_read_packet,
    .read_close     = sena_read_close,
    .read_seek      = sena_read_seek,
};
