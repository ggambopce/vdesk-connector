/* vpx_wrap.c -- VP9 encoder C wrapper (no stdint.h dependency) */
#include <stdlib.h>
#include <string.h>
#include <vpx/vpx_encoder.h>
#include <vpx/vp8cx.h>

typedef unsigned char  u8;
typedef unsigned int   u32;
typedef long long      i64;

/* 마지막 오류 메시지 (초기화 실패 시 진단용) */
static char g_last_err[512];

static void set_last_err(const vpx_codec_ctx_t *ctx, const char *stage) {
    const char *e  = ctx ? vpx_codec_error(ctx) : NULL;
    const char *ed = ctx ? vpx_codec_error_detail(ctx) : NULL;
    if (!e)  e  = "unknown";
    if (!ed) ed = "";
    if (!stage) stage = "vpx";
    g_last_err[0] = 0;
    strncat(g_last_err, stage, sizeof(g_last_err) - 1);
    strncat(g_last_err, ": ", sizeof(g_last_err) - strlen(g_last_err) - 1);
    strncat(g_last_err, e, sizeof(g_last_err) - strlen(g_last_err) - 1);
    if (ed[0]) {
        strncat(g_last_err, " | ", sizeof(g_last_err) - strlen(g_last_err) - 1);
        strncat(g_last_err, ed, sizeof(g_last_err) - strlen(g_last_err) - 1);
    }
}

const char* vpx_enc_last_error(void) {
    return g_last_err;
}

typedef struct {
    vpx_codec_ctx_t     ctx;
    vpx_codec_enc_cfg_t cfg;
    vpx_image_t         img;
    int                 initialized;
    i64                 pts;
} VpxEncHandle;

/* 에러 코드 (out_err로 반환): 0=성공, 1=calloc실패, 2=config_default실패, 3=enc_init실패, 4=img_alloc실패 */
VpxEncHandle* vpx_enc_create_ex(int w, int h, int bitrate_kbps, int fps, int *out_err) {
    if (out_err) *out_err = 0;
    g_last_err[0] = 0;

    VpxEncHandle *handle = (VpxEncHandle*)calloc(1, sizeof(VpxEncHandle));
    if (!handle) { if (out_err) *out_err = 1; strncpy(g_last_err, "calloc failed", sizeof(g_last_err)-1); return NULL; }

    const vpx_codec_iface_t *iface = vpx_codec_vp9_cx();
    if (vpx_codec_enc_config_default(iface, &handle->cfg, 0) != VPX_CODEC_OK) {
        set_last_err(&handle->ctx, "vpx_codec_enc_config_default");
        free(handle);
        if (out_err) *out_err = 2;
        return NULL;
    }

    handle->cfg.g_w                = (unsigned)w;
    handle->cfg.g_h                = (unsigned)h;
    handle->cfg.g_threads          = 2;
    handle->cfg.g_lag_in_frames    = 0;
    handle->cfg.g_error_resilient  = 1;
    handle->cfg.rc_end_usage       = VPX_CBR;
    handle->cfg.rc_target_bitrate  = (unsigned)bitrate_kbps;
    handle->cfg.rc_min_quantizer   = 4;
    handle->cfg.rc_max_quantizer   = 56;
    handle->cfg.rc_undershoot_pct  = 95;
    handle->cfg.rc_overshoot_pct   = 5;
    handle->cfg.rc_buf_sz          = 1000;
    handle->cfg.rc_buf_initial_sz  = 500;
    handle->cfg.rc_buf_optimal_sz  = 600;
    handle->cfg.g_timebase.num     = 1;
    handle->cfg.g_timebase.den     = fps;
    handle->cfg.kf_mode            = VPX_KF_AUTO;
    /* VPX_KF_AUTO에서는 kf_min_dist가 지원되지 않음(0만 허용). */
    handle->cfg.kf_min_dist        = 0;
    handle->cfg.kf_max_dist        = (unsigned)(fps * 3);

    if (vpx_codec_enc_init(&handle->ctx, iface, &handle->cfg, 0) != VPX_CODEC_OK) {
        set_last_err(&handle->ctx, "vpx_codec_enc_init");
        free(handle);
        if (out_err) *out_err = 3;
        return NULL;
    }

    vpx_codec_control(&handle->ctx, VP8E_SET_CPUUSED, 6);
    vpx_codec_control(&handle->ctx, VP9E_SET_ROW_MT, 1);
    vpx_codec_control(&handle->ctx, VP9E_SET_TILE_COLUMNS, 1);
    vpx_codec_control(&handle->ctx, VP9E_SET_AQ_MODE, 3);

    if (!vpx_img_alloc(&handle->img, VPX_IMG_FMT_I420, (unsigned)w, (unsigned)h, 1)) {
        set_last_err(&handle->ctx, "vpx_img_alloc");
        vpx_codec_destroy(&handle->ctx);
        free(handle);
        if (out_err) *out_err = 4;
        return NULL;
    }

    handle->initialized = 1;
    handle->pts = 0;
    return handle;
}

VpxEncHandle* vpx_enc_create(int w, int h, int bitrate_kbps, int fps) {
    return vpx_enc_create_ex(w, h, bitrate_kbps, fps, NULL);
}

void vpx_enc_destroy(VpxEncHandle *handle) {
    if (!handle) return;
    if (handle->initialized) {
        vpx_codec_destroy(&handle->ctx);
        vpx_img_free(&handle->img);
    }
    free(handle);
}

int vpx_enc_encode(VpxEncHandle *handle,
                   const u8 *i420,
                   int force_key,
                   const u8 **out_buf, int *out_len, int *is_key)
{
    if (!handle || !handle->initialized) return -1;

    unsigned w    = handle->img.w;
    unsigned h    = handle->img.h;
    unsigned uv_w = (w + 1) / 2;
    unsigned uv_h = (h + 1) / 2;
    unsigned row;

    /* Y plane */
    {
        const u8 *src = i420;
        u8 *dst = handle->img.planes[VPX_PLANE_Y];
        int stride = handle->img.stride[VPX_PLANE_Y];
        for (row = 0; row < h; row++)
            memcpy(dst + row * stride, src + row * w, w);
    }
    /* U plane */
    {
        const u8 *src = i420 + w * h;
        u8 *dst = handle->img.planes[VPX_PLANE_U];
        int stride = handle->img.stride[VPX_PLANE_U];
        for (row = 0; row < uv_h; row++)
            memcpy(dst + row * stride, src + row * uv_w, uv_w);
    }
    /* V plane */
    {
        const u8 *src = i420 + w * h + uv_w * uv_h;
        u8 *dst = handle->img.planes[VPX_PLANE_V];
        int stride = handle->img.stride[VPX_PLANE_V];
        for (row = 0; row < uv_h; row++)
            memcpy(dst + row * stride, src + row * uv_w, uv_w);
    }

    vpx_enc_frame_flags_t flags = force_key ? VPX_EFLAG_FORCE_KF : 0;
    if (vpx_codec_encode(&handle->ctx, &handle->img, handle->pts++, 1, flags, VPX_DL_REALTIME) != VPX_CODEC_OK)
        return -1;

    *out_buf = NULL;
    *out_len = 0;
    *is_key  = 0;

    {
        vpx_codec_iter_t iter = NULL;
        const vpx_codec_cx_pkt_t *pkt;
        while ((pkt = vpx_codec_get_cx_data(&handle->ctx, &iter)) != NULL) {
            if (pkt->kind == VPX_CODEC_CX_FRAME_PKT) {
                *out_buf = (const u8*)pkt->data.frame.buf;
                *out_len = (int)pkt->data.frame.sz;
                *is_key  = (pkt->data.frame.flags & VPX_FRAME_IS_KEY) ? 1 : 0;
                break;
            }
        }
    }
    return 0;
}
