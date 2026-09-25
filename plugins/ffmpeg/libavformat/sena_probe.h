/*
 * Shared Sena probe helper (sena demuxer + the matroskadec deferral patch)
 *
 * This file is part of the Sena plugin for FFmpeg; it is distributed under
 * the same licence as FFmpeg (LGPL 2.1 or later when built into FFmpeg).
 *
 * A Sena file is a Matroska (EBML) container with a mandatory SENA_PROFILE
 * identification tag in the first top-level Tags element, which the muxer
 * places before the first Cluster. It is therefore always inside the first
 * probe window (PROBE_BUF_MIN = 2048 bytes).
 */

#ifndef AVFORMAT_SENA_PROBE_H
#define AVFORMAT_SENA_PROBE_H

#include <string.h>
#include "libavutil/intreadwrite.h"

#define SENA_PROBE_TAG "SENA_PROFILE"
#define SENA_PROBE_TAG_LEN 12

static inline int ff_sena_probe_match(const uint8_t *buf, int size)
{
    int i;

    if (size < 4 || AV_RB32(buf) != 0x1A45DFA3) /* EBML header */
        return 0;
    for (i = 0; i + SENA_PROBE_TAG_LEN <= size; i++)
        if (!memcmp(buf + i, SENA_PROBE_TAG, SENA_PROBE_TAG_LEN))
            return 1;
    return 0;
}

#endif /* AVFORMAT_SENA_PROBE_H */
