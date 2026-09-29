/*
 * Sena composite audio decoder (ES push API front-end)
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * A Sena track is a single composite elementary stream: Matroska demuxers
 * hand over one packet per 20 ms block (the track is `A_MS/ACM` with
 * wFormatTag 0x534E; the WAVEFORMATEX extra bytes = SENA-INIT blob land in
 * extradata). This decoder pushes blocks into the Rust sena-dec core and
 * pulls mixed 48 kHz stereo float PCM back out.
 *
 * Callback contract (FF_CODEC_DECODE_CB): the return value is the number
 * of bytes consumed from the packet; draining is signalled by a packet
 * with pkt->data == NULL.
 */

#include <string.h>

#include "libavutil/channel_layout.h"
#include "libavutil/common.h"
#include "libavutil/frame.h"
#include "libavutil/log.h"
#include "libavutil/opt.h"

#include "avcodec.h"
#include "codec_internal.h"
#include "decode.h"

#include "sena_dec_dl.h"

typedef struct SenaDecContext {
    const SenaDecAPI *api;
    SenaEs *es;
    int finished;        /* sena_es_finish sent (drain mode) */
    uint64_t next_ts_ns; /* synthetic block timestamp fallback (20 ms grid) */
    int have_origin;     /* a pts anchor was seen since init/flush */
    int64_t pts_origin;  /* pts of the first packet after init/flush; the
                          * demuxer may shift timestamps (CLI -ss), so the
                          * composite grid is anchored on packet pts deltas */
} SenaDecContext;

static av_cold int sena_decode_init(AVCodecContext *avctx)
{
    SenaDecContext *s = avctx->priv_data;
    char err[256];
    int ret;

    s->api = ff_sena_dec_api();
    if (!s->api) {
        av_log(avctx, AV_LOG_ERROR, "sena decoder core unavailable: %s\n",
               ff_sena_dec_load_error());
        return AVERROR_EXTERNAL;
    }
    if (!avctx->extradata || avctx->extradata_size == 0) {
        av_log(avctx, AV_LOG_ERROR, "sena: missing SENA-INIT extradata\n");
        return AVERROR_INVALIDDATA;
    }
    err[0] = 0;
    ret = s->api->sena_es_open(avctx->extradata, avctx->extradata_size, &s->es,
                               err, sizeof(err));
    if (ret != SENA_DEC_OK) {
        av_log(avctx, AV_LOG_ERROR, "sena_es_open: %s\n", err);
        return AVERROR_INVALIDDATA;
    }

    s->next_ts_ns = 0;
    s->have_origin = 0;
    s->pts_origin = 0;
    avctx->sample_fmt = AV_SAMPLE_FMT_FLT;
    avctx->sample_rate = 48000;
    av_channel_layout_uninit(&avctx->ch_layout);
    avctx->ch_layout = (AVChannelLayout)AV_CHANNEL_LAYOUT_STEREO;
    return 0;
}

static av_cold void sena_decode_flush(AVCodecContext *avctx)
{
    SenaDecContext *s = avctx->priv_data;

    if (s->es)
        s->api->sena_es_reset(s->es);
    s->finished = 0;
    s->next_ts_ns = 0;
    s->have_origin = 0;
    s->pts_origin = 0;
}

static av_cold int sena_decode_close(AVCodecContext *avctx)
{
    SenaDecContext *s = avctx->priv_data;

    if (s->es) {
        s->api->sena_es_close(s->es);
        s->es = NULL;
    }
    return 0;
}

static int sena_decode_frame(AVCodecContext *avctx, AVFrame *frame,
                             int *got_frame_ptr, AVPacket *pkt)
{
    SenaDecContext *s = avctx->priv_data;
    uint64_t got = 0;
    int ret;

    if (pkt && pkt->data) {
        /* Block timestamp: anchor the 20 ms grid on the first packet after
         * init/flush — deltas stay exact even when the demuxer shifts
         * timestamps to a non-grid origin (CLI -ss tsfixup). A pts-less
         * stream starts at 0 and walks the 20 ms grid. */
        uint64_t ts_ns;
        if (pkt->pts != AV_NOPTS_VALUE && avctx->pkt_timebase.num > 0) {
            int64_t delta;
            if (!s->have_origin) {
                s->pts_origin = pkt->pts;
                s->have_origin = 1;
            }
            delta = pkt->pts - s->pts_origin;
            if (delta >= 0) {
                ts_ns = av_rescale_q(delta, avctx->pkt_timebase,
                                     (AVRational){1, 1000000000});
                s->next_ts_ns = ts_ns + 20000000;
            } else {
                ts_ns = s->next_ts_ns;
                s->next_ts_ns += 20000000;
            }
        } else {
            ts_ns = s->next_ts_ns;
            s->next_ts_ns += 20000000;
        }
        ret = s->api->sena_es_push(s->es, pkt->data, pkt->size, ts_ns);
        if (ret != SENA_DEC_OK) {
            av_log(avctx, AV_LOG_ERROR, "sena_es_push failed (%d)\n", ret);
            return AVERROR_INVALIDDATA;
        }
    } else if (!s->finished) {
        s->api->sena_es_finish(s->es);
        s->finished = 1;
    }

    /* Pull whatever the core has ready (it gates internally on a ~100 ms
     * resampler window, so a block does not map 1:1 to a frame). */
    frame->nb_samples = 4096;
    if ((ret = ff_get_buffer(avctx, frame, 0)) < 0)
        return ret;
    ret = s->api->sena_es_pull(s->es, (float *)frame->data[0], 4096, &got);
    if (ret != SENA_DEC_OK && ret != SENA_DEC_EOF) {
        av_frame_unref(frame);
        av_log(avctx, AV_LOG_ERROR, "sena_es_pull failed (%d)\n", ret);
        return AVERROR_EXTERNAL;
    }
    if (got == 0) {
        /* Starved (more blocks pending) or drained: no frame this round. */
        av_frame_unref(frame);
        *got_frame_ptr = 0;
        return pkt && pkt->data ? pkt->size : 0;
    }

    frame->nb_samples = got;
    /* Exact absolute position from the core (48 kHz frames), rescaled to
     * the demuxer's timebase; the frame's own timebase must be set for the
     * downstream timestamp bookkeeping (audio_ts_process). */
    frame->time_base = avctx->pkt_timebase;
    {
        SenaDecReadInfo ri;
        if (s->api->sena_es_get_read_info(s->es, &ri) == SENA_DEC_OK &&
            avctx->pkt_timebase.num > 0)
            frame->pts = av_rescale_q(ri.start_frame, (AVRational){1, 48000},
                                      avctx->pkt_timebase);
    }
    *got_frame_ptr = 1;
    return pkt && pkt->data ? pkt->size : 0;
}

const FFCodec ff_sena_decoder = {
    .p.name         = "sena",
    CODEC_LONG_NAME("Sena multilayer audio"),
    .p.type         = AVMEDIA_TYPE_AUDIO,
    .p.id           = AV_CODEC_ID_SENA,
    .priv_data_size = sizeof(SenaDecContext),
    .init           = sena_decode_init,
    .close          = sena_decode_close,
    .flush          = sena_decode_flush,
    FF_CODEC_DECODE_CB(sena_decode_frame),
    .p.capabilities = AV_CODEC_CAP_DELAY | AV_CODEC_CAP_CHANNEL_CONF,
    .caps_internal  = FF_CODEC_CAP_INIT_CLEANUP,
    CODEC_SAMPLEFMTS(AV_SAMPLE_FMT_FLT),
};
