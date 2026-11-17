/*
 * Direct NVENC encoder (NVIDIA Video Codec SDK API, header from FFmpeg's
 * nv-codec-headers, MIT licence), used by the host instead of FFmpeg when
 * reference frame invalidation (RFI) is needed: FFmpeg's nvenc wrapper cannot
 * invalidate reference frames. libcuda and libnvidia-encode are loaded at
 * runtime, so nothing is required at build time but this header.
 *
 * Synchronous use from one thread: open, then encode / invalidate, then close.
 */
#include <dlfcn.h>
#include <stdarg.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>

#include "nvEncodeAPI.h"

typedef int CUresult;
typedef int CUdevice;
typedef void *CUcontext;

typedef struct MoonNvencConfig {
    int codec;          /* 0 H.264, 1 HEVC, 2 AV1 */
    int width, height, fps;
    int bitrate_kbps;
    int preset;         /* 1 (fastest) .. 7 (best) */
    int dpb;            /* reference frames kept (>1 makes RFI possible) */
    int yuv444;         /* 1: 4:4:4 input and output (H.264 / HEVC) */
    int matrix;         /* 0 BT.601, 1 BT.709, 2 BT.2020 */
    int full_range;
    int hdr10;          /* 1: 10-bit P010 input, BT.2020 + PQ (HEVC Main10 / AV1 10-bit) */
} MoonNvencConfig;

typedef struct MoonNvenc {
    void *cuda_lib, *enc_lib;
    CUcontext ctx;
    CUresult (*cuCtxDestroy)(CUcontext);
    NV_ENCODE_API_FUNCTION_LIST fn;
    void *enc;
    NV_ENC_INPUT_PTR input;
    NV_ENC_OUTPUT_PTR output;
    NV_ENC_BUFFER_FORMAT fmt;
    int width, height, yuv444, hdr10;
    unsigned char *out;
    size_t out_cap;
} MoonNvenc;

static int submit(MoonNvenc *e, int pitch, uint64_t ts, int force_idr, const unsigned char **out, uint32_t *out_len, int *is_idr, char *err, int errlen);

static void set_err(char *err, int len, const char *fmt, ...) {
    if (!err || len <= 0)
        return;
    va_list ap;
    va_start(ap, fmt);
    vsnprintf(err, (size_t)len, fmt, ap);
    va_end(ap);
}

void moon_nvenc_close(MoonNvenc *e) {
    if (!e)
        return;
    if (e->enc) {
        if (e->input)
            e->fn.nvEncDestroyInputBuffer(e->enc, e->input);
        if (e->output)
            e->fn.nvEncDestroyBitstreamBuffer(e->enc, e->output);
        e->fn.nvEncDestroyEncoder(e->enc);
    }
    if (e->ctx && e->cuCtxDestroy)
        e->cuCtxDestroy(e->ctx);
    if (e->enc_lib)
        dlclose(e->enc_lib);
    if (e->cuda_lib)
        dlclose(e->cuda_lib);
    free(e->out);
    free(e);
}

static GUID preset_guid(int p) {
    switch (p) {
    case 1: return NV_ENC_PRESET_P1_GUID;
    case 2: return NV_ENC_PRESET_P2_GUID;
    case 3: return NV_ENC_PRESET_P3_GUID;
    case 5: return NV_ENC_PRESET_P5_GUID;
    case 6: return NV_ENC_PRESET_P6_GUID;
    case 7: return NV_ENC_PRESET_P7_GUID;
    default: return NV_ENC_PRESET_P4_GUID;
    }
}

/* Colour description for the VUI / sequence header. */
static void colour(int matrix, NV_ENC_VUI_COLOR_PRIMARIES *prim, NV_ENC_VUI_TRANSFER_CHARACTERISTIC *trc, NV_ENC_VUI_MATRIX_COEFFS *mat) {
    switch (matrix) {
    case 1: *prim = NV_ENC_VUI_COLOR_PRIMARIES_BT709; *trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT709; *mat = NV_ENC_VUI_MATRIX_COEFFS_BT709; break;
    case 2: *prim = NV_ENC_VUI_COLOR_PRIMARIES_BT2020; *trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_BT2020_10; *mat = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL; break;
    default: *prim = NV_ENC_VUI_COLOR_PRIMARIES_SMPTE170M; *trc = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE170M; *mat = NV_ENC_VUI_MATRIX_COEFFS_SMPTE170M; break;
    }
}

static void set_vui(NV_ENC_CONFIG_H264_VUI_PARAMETERS *v, const MoonNvencConfig *c) {
    v->videoSignalTypePresentFlag = 1;
    v->videoFormat = NV_ENC_VUI_VIDEO_FORMAT_UNSPECIFIED;
    v->videoFullRangeFlag = c->full_range ? 1 : 0;
    v->colourDescriptionPresentFlag = 1;
    colour(c->matrix, &v->colourPrimaries, &v->transferCharacteristics, &v->colourMatrix);
    if (c->hdr10) {
        v->videoFullRangeFlag = 0;
        v->colourPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
        v->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
        v->colourMatrix = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
    }
}

MoonNvenc *moon_nvenc_open(const MoonNvencConfig *c, char *err, int errlen) {
    MoonNvenc *e = calloc(1, sizeof(*e));
    if (!e) {
        set_err(err, errlen, "out of memory");
        return NULL;
    }
    e->width = c->width;
    e->height = c->height;
    e->yuv444 = c->yuv444;
    e->hdr10 = c->hdr10;
    if (c->hdr10 && (c->codec == 0 || c->yuv444)) {
        set_err(err, errlen, "10-bit HDR needs HEVC or AV1 in 4:2:0");
        goto fail;
    }

    /* CUDA context */
    e->cuda_lib = dlopen("libcuda.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!e->cuda_lib) {
        set_err(err, errlen, "no CUDA driver (libcuda.so.1)");
        goto fail;
    }
    CUresult (*cuInit)(unsigned) = dlsym(e->cuda_lib, "cuInit");
    CUresult (*cuDeviceGet)(CUdevice *, int) = dlsym(e->cuda_lib, "cuDeviceGet");
    CUresult (*cuCtxCreate)(CUcontext *, unsigned, CUdevice) = dlsym(e->cuda_lib, "cuCtxCreate_v2");
    CUresult (*cuCtxPopCurrent)(CUcontext *) = dlsym(e->cuda_lib, "cuCtxPopCurrent_v2");
    e->cuCtxDestroy = dlsym(e->cuda_lib, "cuCtxDestroy_v2");
    if (!cuInit || !cuDeviceGet || !cuCtxCreate || !cuCtxPopCurrent || !e->cuCtxDestroy) {
        set_err(err, errlen, "CUDA driver lacks required symbols");
        goto fail;
    }
    CUdevice dev;
    if (cuInit(0) || cuDeviceGet(&dev, 0) || cuCtxCreate(&e->ctx, 0, dev)) {
        set_err(err, errlen, "cannot create a CUDA context");
        goto fail;
    }
    CUcontext popped;
    cuCtxPopCurrent(&popped); /* NVENC uses the context explicitly */

    /* NVENC API */
    e->enc_lib = dlopen("libnvidia-encode.so.1", RTLD_NOW | RTLD_LOCAL);
    if (!e->enc_lib) {
        set_err(err, errlen, "no NVENC library (libnvidia-encode.so.1)");
        goto fail;
    }
    NVENCSTATUS (*getMax)(uint32_t *) = dlsym(e->enc_lib, "NvEncodeAPIGetMaxSupportedVersion");
    NVENCSTATUS (*create)(NV_ENCODE_API_FUNCTION_LIST *) = dlsym(e->enc_lib, "NvEncodeAPICreateInstance");
    uint32_t maxv = 0;
    if (!getMax || !create || getMax(&maxv) != NV_ENC_SUCCESS) {
        set_err(err, errlen, "NVENC library is unusable");
        goto fail;
    }
    if (maxv < ((NVENCAPI_MAJOR_VERSION << 4) | NVENCAPI_MINOR_VERSION)) {
        set_err(err, errlen, "NVIDIA driver too old for NVENC API %d.%d (driver supports %u.%u)", NVENCAPI_MAJOR_VERSION, NVENCAPI_MINOR_VERSION, maxv >> 4, maxv & 0xf);
        goto fail;
    }
    e->fn.version = NV_ENCODE_API_FUNCTION_LIST_VER;
    if (create(&e->fn) != NV_ENC_SUCCESS) {
        set_err(err, errlen, "NvEncodeAPICreateInstance failed");
        goto fail;
    }

    NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS op = {0};
    op.version = NV_ENC_OPEN_ENCODE_SESSION_EX_PARAMS_VER;
    op.deviceType = NV_ENC_DEVICE_TYPE_CUDA;
    op.device = e->ctx;
    op.apiVersion = NVENCAPI_VERSION;
    NVENCSTATUS st = e->fn.nvEncOpenEncodeSessionEx(&op, &e->enc);
    if (st != NV_ENC_SUCCESS) {
        e->enc = NULL;
        set_err(err, errlen, "nvEncOpenEncodeSessionEx failed (%d)", st);
        goto fail;
    }

    GUID codec = c->codec == 1 ? NV_ENC_CODEC_HEVC_GUID : c->codec == 2 ? NV_ENC_CODEC_AV1_GUID : NV_ENC_CODEC_H264_GUID;
    GUID preset = preset_guid(c->preset);
    NV_ENC_PRESET_CONFIG pc = {0};
    pc.version = NV_ENC_PRESET_CONFIG_VER;
    pc.presetCfg.version = NV_ENC_CONFIG_VER;
    st = e->fn.nvEncGetEncodePresetConfigEx(e->enc, codec, preset, NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY, &pc);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "this GPU cannot encode the requested codec (preset query failed: %d)", st);
        goto fail;
    }
    NV_ENC_CONFIG cfg = pc.presetCfg;
    cfg.version = NV_ENC_CONFIG_VER;
    cfg.gopLength = NVENC_INFINITE_GOPLENGTH;
    cfg.frameIntervalP = 1;
    /* CBR with a one-frame VBV: every frame fits in its time slot (low latency). */
    cfg.rcParams.rateControlMode = NV_ENC_PARAMS_RC_CBR;
    cfg.rcParams.averageBitRate = (uint32_t)c->bitrate_kbps * 1000u;
    cfg.rcParams.maxBitRate = cfg.rcParams.averageBitRate;
    cfg.rcParams.vbvBufferSize = cfg.rcParams.averageBitRate / (uint32_t)(c->fps > 0 ? c->fps : 60);
    cfg.rcParams.vbvInitialDelay = cfg.rcParams.vbvBufferSize;
    cfg.rcParams.zeroReorderDelay = 1;
    int dpb = c->dpb > 0 ? c->dpb : 1;
    int chroma = c->yuv444 ? 3 : 1;
    if (c->codec == 1) {
        NV_ENC_CONFIG_HEVC *h = &cfg.encodeCodecConfig.hevcConfig;
        cfg.profileGUID = c->yuv444 ? NV_ENC_HEVC_PROFILE_FREXT_GUID : c->hdr10 ? NV_ENC_HEVC_PROFILE_MAIN10_GUID : NV_ENC_HEVC_PROFILE_MAIN_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        h->chromaFormatIDC = chroma;
        h->maxNumRefFramesInDPB = dpb;
        h->numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
        h->outputBitDepth = c->hdr10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        h->inputBitDepth = c->hdr10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        set_vui(&h->hevcVUIParameters, c);
    } else if (c->codec == 2) {
        NV_ENC_CONFIG_AV1 *a = &cfg.encodeCodecConfig.av1Config;
        cfg.profileGUID = NV_ENC_AV1_PROFILE_MAIN_GUID;
        if (c->yuv444) {
            set_err(err, errlen, "NVENC cannot encode 4:4:4 AV1");
            goto fail;
        }
        a->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        a->repeatSeqHdr = 1;
        a->chromaFormatIDC = 1;
        a->maxNumRefFramesInDPB = dpb > 7 ? 7 : dpb; /* 8 slots, one kept for the frame being coded */
        a->numFwdRefs = NV_ENC_NUM_REF_FRAMES_1;
        a->outputBitDepth = c->hdr10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        a->inputBitDepth = c->hdr10 ? NV_ENC_BIT_DEPTH_10 : NV_ENC_BIT_DEPTH_8;
        colour(c->matrix, &a->colorPrimaries, &a->transferCharacteristics, &a->matrixCoefficients);
        a->colorRange = c->full_range ? 1 : 0;
        if (c->hdr10) {
            a->colorPrimaries = NV_ENC_VUI_COLOR_PRIMARIES_BT2020;
            a->transferCharacteristics = NV_ENC_VUI_TRANSFER_CHARACTERISTIC_SMPTE2084;
            a->matrixCoefficients = NV_ENC_VUI_MATRIX_COEFFS_BT2020_NCL;
            a->colorRange = 0;
        }
    } else {
        NV_ENC_CONFIG_H264 *h = &cfg.encodeCodecConfig.h264Config;
        cfg.profileGUID = c->yuv444 ? NV_ENC_H264_PROFILE_HIGH_444_GUID : NV_ENC_H264_PROFILE_HIGH_GUID;
        h->idrPeriod = NVENC_INFINITE_GOPLENGTH;
        h->repeatSPSPPS = 1;
        h->chromaFormatIDC = chroma;
        h->maxNumRefFrames = dpb;
        h->numRefL0 = NV_ENC_NUM_REF_FRAMES_1;
        h->outputBitDepth = NV_ENC_BIT_DEPTH_8;
        h->inputBitDepth = NV_ENC_BIT_DEPTH_8;
        set_vui(&h->h264VUIParameters, c);
    }

    NV_ENC_INITIALIZE_PARAMS ip = {0};
    ip.version = NV_ENC_INITIALIZE_PARAMS_VER;
    ip.encodeGUID = codec;
    ip.presetGUID = preset;
    ip.encodeWidth = ip.darWidth = ip.maxEncodeWidth = (uint32_t)c->width;
    ip.encodeHeight = ip.darHeight = ip.maxEncodeHeight = (uint32_t)c->height;
    ip.frameRateNum = (uint32_t)c->fps;
    ip.frameRateDen = 1;
    ip.enablePTD = 1;
    ip.encodeConfig = &cfg;
    ip.tuningInfo = NV_ENC_TUNING_INFO_ULTRA_LOW_LATENCY;
    st = e->fn.nvEncInitializeEncoder(e->enc, &ip);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncInitializeEncoder failed (%d)", st);
        goto fail;
    }

    e->fmt = c->hdr10 ? NV_ENC_BUFFER_FORMAT_YUV420_10BIT : c->yuv444 ? NV_ENC_BUFFER_FORMAT_YUV444 : NV_ENC_BUFFER_FORMAT_IYUV;
    NV_ENC_CREATE_INPUT_BUFFER ib = {0};
    ib.version = NV_ENC_CREATE_INPUT_BUFFER_VER;
    ib.width = (uint32_t)c->width;
    ib.height = (uint32_t)c->height;
    ib.bufferFmt = e->fmt;
    st = e->fn.nvEncCreateInputBuffer(e->enc, &ib);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncCreateInputBuffer failed (%d)", st);
        goto fail;
    }
    e->input = ib.inputBuffer;
    NV_ENC_CREATE_BITSTREAM_BUFFER bb = {0};
    bb.version = NV_ENC_CREATE_BITSTREAM_BUFFER_VER;
    st = e->fn.nvEncCreateBitstreamBuffer(e->enc, &bb);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncCreateBitstreamBuffer failed (%d)", st);
        goto fail;
    }
    e->output = bb.bitstreamBuffer;
    return e;

fail:
    moon_nvenc_close(e);
    return NULL;
}

/* Encode one picture. Planes are 8-bit Y, U, V; U/V are half size for 4:2:0.
 * On success *out / *out_len point to the bitstream (valid until the next call). */
int moon_nvenc_encode(MoonNvenc *e, const unsigned char *y, const unsigned char *u, const unsigned char *v, int ys, int us, int vs,
                      uint64_t ts, int force_idr, const unsigned char **out, uint32_t *out_len, int *is_idr, char *err, int errlen) {
    NV_ENC_LOCK_INPUT_BUFFER li = {0};
    li.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
    li.inputBuffer = e->input;
    NVENCSTATUS st = e->fn.nvEncLockInputBuffer(e->enc, &li);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncLockInputBuffer failed (%d)", st);
        return -1;
    }
    unsigned char *dst = li.bufferDataPtr;
    int pitch = (int)li.pitch, w = e->width, h = e->height;
    for (int r = 0; r < h; r++)
        memcpy(dst + (size_t)r * pitch, y + (size_t)r * ys, (size_t)w);
    if (e->yuv444) {
        unsigned char *du = dst + (size_t)pitch * h, *dv = du + (size_t)pitch * h;
        for (int r = 0; r < h; r++) {
            memcpy(du + (size_t)r * pitch, u + (size_t)r * us, (size_t)w);
            memcpy(dv + (size_t)r * pitch, v + (size_t)r * vs, (size_t)w);
        }
    } else {
        /* IYUV: chroma planes follow luma, with half the pitch. */
        int cw = (w + 1) / 2, ch = (h + 1) / 2, cp = pitch / 2;
        unsigned char *du = dst + (size_t)pitch * h, *dv = du + (size_t)cp * ch;
        for (int r = 0; r < ch; r++) {
            memcpy(du + (size_t)r * cp, u + (size_t)r * us, (size_t)cw);
            memcpy(dv + (size_t)r * cp, v + (size_t)r * vs, (size_t)cw);
        }
    }
    e->fn.nvEncUnlockInputBuffer(e->enc, e->input);
    return submit(e, pitch, ts, force_idr, out, out_len, is_idr, err, errlen);
}

/* Encode one 10-bit P010 picture (16-bit samples, value in the high bits;
 * luma then interleaved chroma), strides in samples. */
int moon_nvenc_encode_p010(MoonNvenc *e, const uint16_t *y, const uint16_t *uv, int ys, int uvs,
                           uint64_t ts, int force_idr, const unsigned char **out, uint32_t *out_len, int *is_idr, char *err, int errlen) {
    if (!e->hdr10) {
        set_err(err, errlen, "encoder not opened for 10-bit input");
        return -1;
    }
    NV_ENC_LOCK_INPUT_BUFFER li = {0};
    li.version = NV_ENC_LOCK_INPUT_BUFFER_VER;
    li.inputBuffer = e->input;
    NVENCSTATUS st = e->fn.nvEncLockInputBuffer(e->enc, &li);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncLockInputBuffer failed (%d)", st);
        return -1;
    }
    unsigned char *dst = li.bufferDataPtr;
    int pitch = (int)li.pitch, w = e->width, h = e->height, ch = (h + 1) / 2, row = ((w + 1) / 2) * 2 * 2;
    for (int r = 0; r < h; r++)
        memcpy(dst + (size_t)r * pitch, y + (size_t)r * ys, (size_t)w * 2);
    for (int r = 0; r < ch; r++)
        memcpy(dst + (size_t)pitch * h + (size_t)r * pitch, uv + (size_t)r * uvs, (size_t)row);
    e->fn.nvEncUnlockInputBuffer(e->enc, e->input);
    return submit(e, pitch, ts, force_idr, out, out_len, is_idr, err, errlen);
}

static int submit(MoonNvenc *e, int pitch, uint64_t ts, int force_idr, const unsigned char **out, uint32_t *out_len, int *is_idr, char *err, int errlen) {
    int w = e->width, h = e->height;
    NVENCSTATUS st;
    NV_ENC_PIC_PARAMS pp = {0};
    pp.version = NV_ENC_PIC_PARAMS_VER;
    pp.inputWidth = (uint32_t)w;
    pp.inputHeight = (uint32_t)h;
    pp.inputPitch = (uint32_t)pitch;
    pp.inputBuffer = e->input;
    pp.outputBitstream = e->output;
    pp.bufferFmt = e->fmt;
    pp.pictureStruct = NV_ENC_PIC_STRUCT_FRAME;
    pp.inputTimeStamp = ts;
    pp.encodePicFlags = force_idr ? (NV_ENC_PIC_FLAG_FORCEIDR | NV_ENC_PIC_FLAG_OUTPUT_SPSPPS) : 0;
    st = e->fn.nvEncEncodePicture(e->enc, &pp);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncEncodePicture failed (%d)", st);
        return -1;
    }
    NV_ENC_LOCK_BITSTREAM lb = {0};
    lb.version = NV_ENC_LOCK_BITSTREAM_VER;
    lb.outputBitstream = e->output;
    st = e->fn.nvEncLockBitstream(e->enc, &lb);
    if (st != NV_ENC_SUCCESS) {
        set_err(err, errlen, "nvEncLockBitstream failed (%d)", st);
        return -1;
    }
    if (lb.bitstreamSizeInBytes > e->out_cap) {
        unsigned char *n = realloc(e->out, lb.bitstreamSizeInBytes);
        if (!n) {
            e->fn.nvEncUnlockBitstream(e->enc, e->output);
            set_err(err, errlen, "out of memory");
            return -1;
        }
        e->out = n;
        e->out_cap = lb.bitstreamSizeInBytes;
    }
    memcpy(e->out, lb.bitstreamBufferPtr, lb.bitstreamSizeInBytes);
    *out = e->out;
    *out_len = lb.bitstreamSizeInBytes;
    *is_idr = lb.pictureType == NV_ENC_PIC_TYPE_IDR;
    e->fn.nvEncUnlockBitstream(e->enc, e->output);
    return 0;
}

/* Stop using the picture encoded with this timestamp as a reference. */
int moon_nvenc_invalidate(MoonNvenc *e, uint64_t ts) {
    return e->fn.nvEncInvalidateRefFrames(e->enc, ts) == NV_ENC_SUCCESS ? 0 : -1;
}
