/* vpx_wrap.c -- VP9 decoder C wrapper */
#include <stdlib.h>
#include <string.h>
#include <vpx/vpx_decoder.h>
#include <vpx/vp8dx.h>
#include <vpx/vpx_image.h>

typedef unsigned char u8;

typedef struct {
    vpx_codec_ctx_t ctx;
    int             initialized;
} VpxDecHandle;

VpxDecHandle* vpx_dec_create(void) {
    VpxDecHandle *handle = (VpxDecHandle*)calloc(1, sizeof(VpxDecHandle));
    if (!handle) return NULL;

    vpx_codec_dec_cfg_t cfg;
    memset(&cfg, 0, sizeof(cfg));
    cfg.threads = 4;

    if (vpx_codec_dec_init(&handle->ctx, vpx_codec_vp9_dx(), &cfg, 0) != VPX_CODEC_OK) {
        free(handle);
        return NULL;
    }

    handle->initialized = 1;
    return handle;
}

void vpx_dec_destroy(VpxDecHandle *handle) {
    if (!handle) return;
    if (handle->initialized) vpx_codec_destroy(&handle->ctx);
    free(handle);
}

int vpx_dec_decode(VpxDecHandle *handle,
                   const u8 *data, int len,
                   int *out_w, int *out_h,
                   u8 **out_y, u8 **out_u, u8 **out_v,
                   int *stride_y, int *stride_u, int *stride_v)
{
    if (!handle || !handle->initialized) return -1;

    if (vpx_codec_decode(&handle->ctx, data, (unsigned)len, NULL, 0) != VPX_CODEC_OK)
        return -1;

    {
        vpx_codec_iter_t iter = NULL;
        vpx_image_t *img = vpx_codec_get_frame(&handle->ctx, &iter);
        if (!img) return 1;

        *out_w    = (int)img->d_w;
        *out_h    = (int)img->d_h;
        *out_y    = img->planes[VPX_PLANE_Y];
        *out_u    = img->planes[VPX_PLANE_U];
        *out_v    = img->planes[VPX_PLANE_V];
        *stride_y = img->stride[VPX_PLANE_Y];
        *stride_u = img->stride[VPX_PLANE_U];
        *stride_v = img->stride[VPX_PLANE_V];
    }
    return 0;
}
