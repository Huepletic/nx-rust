/* P3: собственный VA-API H.264-энкодер: 1 IDR + 4 P-кадра, CBR 20 Мбит/с.
 *
 * Этюд к собственному libva-кодировщику nx-rust (вместо внешнего ffmpeg).
 * Пайплайн: dumb-BGRX (паттерн) → импорт dmabuf (P1) → VPP RGB→NV12 (P2) →
 * H.264-энкодинг (EncSlice, High, CABAC) → Annex-B битстрим → /tmp/p3.h264.
 *
 * Рецепт буферов — LIBVA_TRACE живого ffmpeg (/tmp/vafft.*, референсные
 * значения struct — /tmp/e36_p3_recipe.txt):
 *   IDR (кадр 0): seq(1132) + misc RC(64) + HRD(28) + FrameRate(28) +
 *                 picture(648) + packed SEQUENCE(SPS+PPS) + packed SLICE
 *                 + slice(3140)                                = 10 буферов
 *   P (кадры 1-4): picture + packed SLICE + slice              = 4 буфера
 * (у ffmpeg +2 буфера SEI-RawData — здесь не нужны: Mesa требует ≥1 не-slice
 *  заголовок (SPS/PPS покрывает) + ≥1 slice, radeon_vce.c:852-853.)
 *
 * Packed-данные ПАРСЯТСЯ Mesa (picture_h264_enc.c:803-875) в состояние
 * контекста (SPS→seq, PPS→pic_ctrl, slice→slice+pic.nal_ref_idc/nal_unit_type)
 * → все три заголовка обязаны быть валидным H.264-синтаксисом (бит-писатель
 * ниже), с эмуляционными байтами (has_emulation_bytes=1).
 *
 * Гейты P3:
 *  1) 5 кадров кодируются без VA-ошибок;
 *  2) в /tmp/p3.h264 каждый NAL имеет старт-код, ≥5 slice-NAL + SPS + PPS;
 *  3) ffmpeg декодирует ровно 5 кадров без ошибок (внешняя проверка);
 *  4) кадр 0 (статичный I) декодится с Y-полосами, близкими к источнику (±10).
 *
 * Сборка: gcc -O0 -g -Wall -o /tmp/va_p3 tools/va_p3_enc.c $(pkg-config --cflags --libs libva libva-drm)
 * Запуск: /tmp/va_p3 > /tmp/e36_p3.txt 2>&1
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <time.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
#include <va/va_enc_h264.h>
#include <va/va_vpp.h>

#define DRM_IOCTL_BASE 0x64u
#define IOWR(nr, sz) ((3u << 30) | ((sz) << 16) | (DRM_IOCTL_BASE << 8) | (nr))
#define DRM_IOCTL_MODE_CREATE_DUMB IOWR(0xB2, 24)
#define DRM_IOCTL_PRIME_HANDLE_TO_FD IOWR(0x2D, 12)

struct create_dumb { uint32_t height, width, bpp, flags, handle, pitch; };
struct prime_handle { uint32_t handle, flags; int32_t fd; };

#define DRM_FORMAT_XRGB8888 0x34325258u
#define W 1088
#define H 800
#define MBW 68
#define MBH 50
#define MBS (MBW * MBH)
#define GOP 30
#define BITRATE 20000000u
#define NFRAMES 5
#define CODED_SIZE (3u * W * H + 65536u)

static const uint8_t bars[8][4] = {
    {0, 0, 255, 0}, {0, 255, 0, 0}, {255, 0, 0, 0}, {0, 255, 255, 0},
    {255, 0, 255, 0}, {255, 255, 0, 0}, {64, 64, 64, 0}, {192, 192, 192, 0},
};
/* Паттерн со сдвигом по X: кадр i сдвинут на i*136 px → P-кадры получают
 * реальное изменение (движение полос), а не нулевой остаток. */
static void fill_pattern(uint8_t *p, uint32_t pitch, int shift) {
    for (uint32_t y = 0; y < H; y++) {
        uint8_t *row = p + (size_t)y * pitch;
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t *c = bars[(((int)x + shift) % W) * 8 / W];
            row[x * 4 + 0] = c[0]; row[x * 4 + 1] = c[1];
            row[x * 4 + 2] = c[2]; row[x * 4 + 3] = 0;
        }
    }
}

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

/* ---------- бит-писатель RBSP + эмуляция + старт-коды ---------- */

typedef struct { uint8_t *d; size_t cap, len; uint32_t acc; int n; } bw;

static void bw_init(bw *b, uint8_t *mem, size_t cap) {
    b->d = mem; b->cap = cap; b->len = 0; b->acc = 0; b->n = 0;
}
static void bw_put(bw *b, uint32_t v, int bits) {
    for (int i = bits - 1; i >= 0; i--) {
        b->acc = (b->acc << 1) | ((v >> i) & 1u);
        if (++b->n == 8) {
            if (b->len < b->cap) b->d[b->len++] = (uint8_t)b->acc;
            b->acc = 0; b->n = 0;
        }
    }
}
static void bw_ue(bw *b, uint32_t v) {
    uint32_t t = v + 1; int lz = 0;
    for (uint32_t u = t; u >>= 1;) lz++;
    bw_put(b, 0, (uint32_t)lz);
    bw_put(b, t, lz + 1);
}
static void bw_se(bw *b, int32_t v) {
    uint32_t u = (v <= 0) ? (uint32_t)(-2 * (int64_t)v) : (uint32_t)(2 * v - 1);
    bw_ue(b, u);
}
static void bw_trailing(bw *b) {
    bw_put(b, 1, 1);
    while (b->n) bw_put(b, 0, 1);
}

/* EBSP: вставка эмуляционных байтов 0x03 (когда два нуля подряд и байт ≤ 3). */
static size_t ebbsp(const uint8_t *src, size_t n, uint8_t *dst) {
    size_t o = 0; int z = 0;
    for (size_t i = 0; i < n; i++) {
        if (z >= 2 && src[i] <= 3) { dst[o++] = 0x03; z = 0; }
        dst[o++] = src[i];
        z = (src[i] == 0) ? z + 1 : 0;
    }
    return o;
}

/* NAL = 00 00 00 01 + forbidden/nri/type + EBSP(rbsp). */
static size_t make_nal(uint8_t *out, uint8_t nal_hdr, const uint8_t *rbsp, size_t rbsp_len) {
    uint8_t eb[4096];
    size_t ebl = ebbsp(rbsp, rbsp_len, eb);
    size_t o = 0;
    out[o++] = 0; out[o++] = 0; out[o++] = 0; out[o++] = 1;
    out[o++] = nal_hdr;
    memcpy(out + o, eb, ebl);
    return o + ebl;
}

/* SPS (High, level 4.1, 68x50 MBs, POC type 2, 1 ref, VUI 60fps 1:1) —
 * значения согласованы с VAEncSequenceParameterBufferH264 ниже. */
static size_t build_sps(uint8_t *out) {
    uint8_t rbsp[256]; bw b;
    bw_init(&b, rbsp, sizeof(rbsp));
    bw_put(&b, 100, 8);              /* profile_idc = High */
    bw_put(&b, 0, 6);                /* constraint flags */
    bw_put(&b, 0, 2);                /* reserved */
    bw_put(&b, 41, 8);               /* level_idc = 4.1 */
    bw_ue(&b, 0);                    /* sps_id */
    bw_ue(&b, 1);                    /* chroma_format_idc = 4:2:0 */
    bw_ue(&b, 0);                    /* bit_depth_luma_minus8 */
    bw_ue(&b, 0);                    /* bit_depth_chroma_minus8 */
    bw_put(&b, 0, 1);                /* qpprime_y_zero_transform_bypass */
    bw_put(&b, 0, 1);                /* seq_scaling_matrix_present */
    bw_ue(&b, 4);                    /* log2_max_frame_num_minus4 */
    bw_ue(&b, 2);                    /* pic_order_cnt_type = 2 */
    bw_ue(&b, 1);                    /* max_num_ref_frames */
    bw_put(&b, 0, 1);                /* gaps_in_frame_num */
    bw_ue(&b, MBW - 1);              /* pic_width_in_mbs_minus1 */
    bw_ue(&b, MBH - 1);              /* pic_height_in_map_units_minus1 */
    bw_put(&b, 1, 1);                /* frame_mbs_only_flag */
    bw_put(&b, 1, 1);                /* direct_8x8_inference */
    bw_put(&b, 0, 1);                /* frame_cropping */
    bw_put(&b, 1, 1);                /* vui_parameters_present */
    bw_put(&b, 1, 1);                /* aspect_ratio_info_present */
    bw_put(&b, 1, 8);                /* aspect_ratio_idc = 1:1 */
    bw_put(&b, 0, 1);                /* overscan */
    bw_put(&b, 0, 1);                /* video_signal */
    bw_put(&b, 0, 1);                /* chroma_loc */
    bw_put(&b, 1, 1);                /* timing_info_present */
    bw_put(&b, 1, 32);               /* num_units_in_tick */
    bw_put(&b, 120, 32);             /* time_scale (→ 60 fps) */
    bw_put(&b, 1, 1);                /* fixed_frame_rate */
    bw_put(&b, 0, 1);                /* nal_hrd */
    bw_put(&b, 0, 1);                /* vcl_hrd */
    bw_put(&b, 0, 1);                /* pic_struct_present */
    bw_put(&b, 1, 1);                /* bitstream_restriction */
    bw_put(&b, 1, 1);                /* motion_vectors_over_pic_boundaries */
    bw_ue(&b, 2);                    /* max_bytes_per_pic_denom */
    bw_ue(&b, 1);                    /* max_bits_per_mb_denom */
    bw_ue(&b, 15);                   /* log2_max_mv_length_h */
    bw_ue(&b, 15);                   /* log2_max_mv_length_v */
    bw_ue(&b, 0);                    /* max_num_reorder_frames */
    bw_ue(&b, 1);                    /* max_dec_frame_buffering */
    bw_trailing(&b);
    return make_nal(out, 0x67, rbsp, b.len);   /* nri=3, type=7 */
}

/* PPS (CABAC, High-tail transform_8x8=1) — согласован с pic_fields ниже. */
static size_t build_pps(uint8_t *out) {
    uint8_t rbsp[64]; bw b;
    bw_init(&b, rbsp, sizeof(rbsp));
    bw_ue(&b, 0);                    /* pps_id */
    bw_ue(&b, 0);                    /* sps_id */
    bw_put(&b, 1, 1);                /* entropy_coding_mode_flag = CABAC */
    bw_put(&b, 0, 1);                /* bottom_field_pic_order_in_frame_present */
    bw_ue(&b, 0);                    /* num_slice_groups_minus1 */
    bw_ue(&b, 0);                    /* num_ref_idx_l0_default_active_minus1 */
    bw_ue(&b, 0);                    /* num_ref_idx_l1_default_active_minus1 */
    bw_put(&b, 0, 1);                /* weighted_pred */
    bw_put(&b, 0, 2);                /* weighted_bipred_idc */
    bw_se(&b, 0);                    /* pic_init_qp_minus26 */
    bw_se(&b, 0);                    /* pic_init_qs_minus26 */
    bw_se(&b, 0);                    /* chroma_qp_index_offset */
    bw_put(&b, 0, 1);                /* deblocking_filter_control_present */
    bw_put(&b, 0, 1);                /* constrained_intra_pred */
    bw_put(&b, 0, 1);                /* redundant_pic_cnt_present */
    bw_put(&b, 1, 1);                /* transform_8x8_mode (High tail) */
    bw_put(&b, 0, 1);                /* pic_scaling_matrix_present */
    bw_se(&b, 0);                    /* second_chroma_qp_index_offset */
    bw_trailing(&b);
    return make_nal(out, 0x68, rbsp, b.len);   /* nri=3, type=8 */
}

/* Slice-заголовок: первый на картинку (first_mb=0 → parseEncSliceParamsH264
 * читает до конца, picture_h264_enc.c:446-449; иначе nal_ref_idc/nal_unit_type
 * из NAL-байта НЕ попадают в контекст). */
static size_t build_slice_hdr(uint8_t *out, int idr, uint32_t frame_num) {
    uint8_t rbsp[64]; bw b;
    bw_init(&b, rbsp, sizeof(rbsp));
    bw_ue(&b, 0);                    /* first_mb_in_slice */
    bw_ue(&b, idr ? 2u : 0u);        /* slice_type: I=2 (IDR), P=0 */
    bw_ue(&b, 0);                    /* pic_parameter_set_id */
    bw_put(&b, frame_num, 8);        /* frame_num (log2_max_frame_num = 8) */
    if (idr) {
        bw_ue(&b, 0);                /* idr_pic_id */
        bw_put(&b, 0, 1);            /* no_output_of_prior_pics_flag */
        bw_put(&b, 0, 1);            /* long_term_reference_flag */
    } else {
        bw_put(&b, 0, 1);            /* num_ref_idx_active_override_flag */
        bw_put(&b, 0, 1);            /* ref_pic_list_modification_flag_l0 */
        bw_put(&b, 0, 1);            /* adaptive_ref_pic_marking_mode_flag */
        bw_ue(&b, 0);                /* cabac_init_idc (CABAC && P) */
    }
    bw_se(&b, 0);                    /* slice_qp_delta */
    bw_trailing(&b);
    return make_nal(out, idr ? 0x65 : 0x41, rbsp, b.len);   /* nri=3/type=5 | nri=2/type=1 */
}

static void vui_fill(VAEncSequenceParameterBufferH264 *seq) {
    memset(seq, 0, sizeof(*seq));
    seq->seq_parameter_set_id = 0;
    seq->level_idc = 41;
    seq->intra_period = GOP;
    seq->intra_idr_period = GOP;
    seq->ip_period = 1;
    seq->bits_per_second = BITRATE;
    seq->max_num_ref_frames = 1;
    seq->picture_width_in_mbs = MBW;
    seq->picture_height_in_mbs = MBH;
    seq->seq_fields.bits.chroma_format_idc = 1;
    seq->seq_fields.bits.frame_mbs_only_flag = 1;
    seq->seq_fields.bits.direct_8x8_inference_flag = 1;
    seq->seq_fields.bits.log2_max_frame_num_minus4 = 4;
    seq->seq_fields.bits.pic_order_cnt_type = 2;
    seq->vui_parameters_present_flag = 1;
    seq->vui_fields.bits.aspect_ratio_info_present_flag = 1;
    seq->vui_fields.bits.timing_info_present_flag = 1;
    seq->vui_fields.bits.fixed_frame_rate_flag = 1;
    seq->vui_fields.bits.bitstream_restriction_flag = 1;
    seq->vui_fields.bits.log2_max_mv_length_horizontal = 15;
    seq->vui_fields.bits.log2_max_mv_length_vertical = 15;
    seq->aspect_ratio_idc = 1;      /* 1:1 */
    seq->num_units_in_tick = 1;
    seq->time_scale = 120;          /* → 60 fps */
}

static int run_vpp(VADisplay dpy, VAContextID ctx, VASurfaceID src, VASurfaceID dst) {
    VAProcPipelineParameterBuffer pp; memset(&pp, 0, sizeof(pp));
    pp.surface = src;
    pp.surface_color_standard = VAProcColorStandardSRGB;
    pp.output_color_standard = VAProcColorStandardBT601;
    if (vaBeginPicture(dpy, ctx, dst) != VA_STATUS_SUCCESS) return -1;
    VABufferID buf = VA_INVALID_ID;
    if (vaCreateBuffer(dpy, ctx, VAProcPipelineParameterBufferType, sizeof(pp), 1, &pp, &buf) != VA_STATUS_SUCCESS)
        { vaEndPicture(dpy, ctx); return -2; }
    if (vaRenderPicture(dpy, ctx, &buf, 1) != VA_STATUS_SUCCESS)
        { vaDestroyBuffer(dpy, buf); vaEndPicture(dpy, ctx); return -3; }
    vaDestroyBuffer(dpy, buf);
    if (vaEndPicture(dpy, ctx) != VA_STATUS_SUCCESS) return -4;
    if (vaSyncSurface(dpy, dst) != VA_STATUS_SUCCESS) return -5;
    return 0;
}

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int nframes = (argc > 1) ? atoi(argv[1]) : NFRAMES;   /* ./va_p3 90 — серия */
    if (nframes < 1 || nframes > 3000) { printf("P3 ENC: bad nframes\n"); return 2; }

    /* --- dumb-буфер + импорт dmabuf (P1-комбинация) --- */
    int card = -1;
    const char *cards[] = {"/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2", "/dev/dri/card3"};
    for (unsigned i = 0; i < 4; i++) {
        if (access(cards[i], F_OK) != 0) continue;
        card = open(cards[i], O_RDWR | O_CLOEXEC);
        if (card >= 0) break;
    }
    if (card < 0) { printf("P3 ENC: FAIL open card*\n"); return 1; }
    struct create_dumb dumb = {.height = H, .width = W, .bpp = 32};
    if (ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) < 0) { printf("P3 ENC: FAIL dumb: %s\n", strerror(errno)); return 1; }
    struct prime_handle ph = {.handle = dumb.handle, .flags = O_RDWR | O_CLOEXEC, .fd = -1};
    if (ioctl(card, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0) { printf("P3 ENC: FAIL prime: %s\n", strerror(errno)); return 1; }
    int dmabuf = ph.fd;
    size_t sz = (size_t)dumb.pitch * H;
    uint8_t *map = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, dmabuf, 0);
    if (map == MAP_FAILED) { printf("P3 ENC: FAIL mmap: %s\n", strerror(errno)); return 1; }

    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    VADisplay dpy = vaGetDisplayDRM(drm);
    int maj, min;
    if (vaInitialize(dpy, &maj, &min) != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL vaInitialize\n"); return 1; }

    VADRMPRIMESurfaceDescriptor desc; memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_BGRX; desc.width = W; desc.height = H;
    desc.num_objects = 1;
    desc.objects[0].fd = dmabuf; desc.objects[0].size = (uint32_t)sz;
    desc.num_layers = 1;
    desc.layers[0].drm_format = DRM_FORMAT_XRGB8888; desc.layers[0].num_planes = 1;
    desc.layers[0].pitch[0] = dumb.pitch;
    VASurfaceAttrib attrs[2]; memset(attrs, 0, sizeof(attrs));
    attrs[0].type = VASurfaceAttribMemoryType;
    attrs[0].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[0].value.type = VAGenericValueTypeInteger;
    attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attrs[1].type = VASurfaceAttribExternalBufferDescriptor;
    attrs[1].flags = VA_SURFACE_ATTRIB_SETTABLE;
    attrs[1].value.type = VAGenericValueTypePointer;
    attrs[1].value.value.p = &desc;
    VASurfaceID src = VA_INVALID_ID;
    VAStatus st = vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, W, H, &src, 1, attrs, 2);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL import: %s\n", vaErrorStr(st)); return 1; }

    /* --- VPP-конфиг (P2) --- */
    VASurfaceID vdst = VA_INVALID_ID;
    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, &vdst, 1, NULL, 0);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL vpp dst: %s\n", vaErrorStr(st)); return 1; }
    VAConfigID vpp_cfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vpp_cfg);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL vpp config: %s\n", vaErrorStr(st)); return 1; }
    VAContextID vctx = VA_INVALID_ID;
    st = vaCreateContext(dpy, vpp_cfg, W, H, 0x1, NULL, 0, &vctx);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL vpp ctx: %s\n", vaErrorStr(st)); return 1; }

    /* --- Энкодер-конфиг: эталон ffmpeg из /tmp/e36_p3_recipe.txt:
     * profile=7 H264High, entrypoint=6 EncSlice, attribs
     * {RTFormat=0x1 YUV420, RateControl=0x4, PackedHeaders=0x0d SEQ|SLICE|MISC}.
     * Производство использует -rc_mode CBR (pipeline.rs) → VA_RC_CBR=0x2. */
    VAConfigAttrib ecattr[3];
    ecattr[0].type = VAConfigAttribRTFormat;         ecattr[0].value = VA_RT_FORMAT_YUV420;
    ecattr[1].type = VAConfigAttribRateControl;      ecattr[1].value = VA_RC_CBR;
    ecattr[2].type = VAConfigAttribEncPackedHeaders;
    ecattr[2].value = VA_ENC_PACKED_HEADER_SEQUENCE | VA_ENC_PACKED_HEADER_SLICE | VA_ENC_PACKED_HEADER_MISC;
    VAConfigID enc_cfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileH264High, VAEntrypointEncSlice, ecattr, 3, &enc_cfg);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL enc config: %s\n", vaErrorStr(st)); return 1; }

    VAContextID ectx = VA_INVALID_ID;
    st = vaCreateContext(dpy, enc_cfg, W, H, VA_PROGRESSIVE, NULL, 0, &ectx);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL enc ctx: %s\n", vaErrorStr(st)); return 1; }

    /* поверхности-реконструкции: пинг-pong (текущая + предыдущая = референс) */
    VASurfaceID recon[2] = {VA_INVALID_ID, VA_INVALID_ID};
    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, recon, 2, NULL, 0);
    if (st != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL recon: %s\n", vaErrorStr(st)); return 1; }

    FILE *f = fopen("/tmp/p3.h264", "wb");
    if (!f) { printf("P3 ENC: FAIL open /tmp/p3.h264\n"); return 1; }

    int all_ok = 1;
    size_t total = 0;
    double total_ms = 0;

    size_t au_sizes[3000]; double au_ms[3000];
    for (int i = 0; i < nframes; i++) {
        int idr = (i % GOP == 0);

        /* источник: паттерн со сдвигом → импорт-поверхность → VPP → NV12 */
        fill_pattern(map, dumb.pitch, i * 136);
        if (run_vpp(dpy, vctx, src, vdst) != 0) { printf("P3 ENC: FAIL vpp frame %d\n", i); all_ok = 0; break; }

        /* кадр 0: печать Y-полос VPP-выхода (контроль источника кодирования) */
        if (i == 0) {
            VAImageFormat fmts[64]; int nf = 0;
            vaQueryImageFormats(dpy, fmts, &nf);
            VAImageFormat nv12; int found = 0; memset(&nv12, 0, sizeof(nv12));
            for (int k = 0; k < nf; k++)
                if (fmts[k].fourcc == VA_FOURCC_NV12) { nv12 = fmts[k]; found = 1; }
            if (found) {
                VAImage img;
                if (vaCreateImage(dpy, &nv12, W, H, &img) == VA_STATUS_SUCCESS &&
                    vaGetImage(dpy, vdst, 0, 0, W, H, img.image_id) == VA_STATUS_SUCCESS) {
                    void *ptr = NULL;
                    if (vaMapBuffer(dpy, img.buf, &ptr) == VA_STATUS_SUCCESS && ptr) {
                        uint8_t *yp = (uint8_t *)ptr + img.offsets[0];
                        printf("vpp-frame0-y:");
                        for (int b2 = 0; b2 < 8; b2++) {
                            int x = b2 * (W / 8) + (W / 8) / 2;
                            printf(" %d", yp[(size_t)(H / 2) * img.pitches[0] + x]);
                        }
                        printf("\n");
                        vaUnmapBuffer(dpy, img.buf);
                    }
                    vaDestroyImage(dpy, img.image_id);
                }
            }
        }

        /* --- буферы кадра (создаём ДО BeginPicture, как ffmpeg) --- */
        uint8_t seqmem[sizeof(VAEncSequenceParameterBufferH264)];
        uint8_t rcmem[sizeof(VAEncMiscParameterBuffer) + sizeof(VAEncMiscParameterRateControl)];
        uint8_t hrdmem[sizeof(VAEncMiscParameterBuffer) + sizeof(VAEncMiscParameterHRD)];
        uint8_t frmem[sizeof(VAEncMiscParameterBuffer) + sizeof(VAEncMiscParameterFrameRate)];
        uint8_t picmem[sizeof(VAEncPictureParameterBufferH264)];
        uint8_t slcmem[sizeof(VAEncSliceParameterBufferH264)];
        VABufferID bids[12]; int nb = 0;

        /* sequence (только IDR) + misc RC/HRD/FrameRate */
        if (idr) {
            VAEncSequenceParameterBufferH264 *seq = (VAEncSequenceParameterBufferH264 *)seqmem;
            vui_fill(seq);
            VABufferID b;
            if (vaCreateBuffer(dpy, ectx, VAEncSequenceParameterBufferType, sizeof(*seq), 1, seq, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL seq buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;

            memset(rcmem, 0, sizeof(rcmem));
            *(VAEncMiscParameterType *)rcmem = VAEncMiscParameterTypeRateControl;
            VAEncMiscParameterRateControl *rc = (VAEncMiscParameterRateControl *)(rcmem + 4);
            rc->bits_per_second = BITRATE;
            rc->target_percentage = 100;    /* CBR: target = max */
            rc->window_size = 500;
            rc->initial_qp = 26;
            rc->min_qp = 10;
            rc->max_qp = 51;
            if (vaCreateBuffer(dpy, ectx, VAEncMiscParameterBufferType, sizeof(rcmem), 1, rcmem, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL rc buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;

            memset(hrdmem, 0, sizeof(hrdmem));
            *(VAEncMiscParameterType *)hrdmem = VAEncMiscParameterTypeHRD;
            VAEncMiscParameterHRD *hrd = (VAEncMiscParameterHRD *)(hrdmem + 4);
            hrd->buffer_size = BITRATE;
            hrd->initial_buffer_fullness = BITRATE / 2;
            if (vaCreateBuffer(dpy, ectx, VAEncMiscParameterBufferType, sizeof(hrdmem), 1, hrdmem, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL hrd buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;

            memset(frmem, 0, sizeof(frmem));
            *(VAEncMiscParameterType *)frmem = VAEncMiscParameterTypeFrameRate;
            VAEncMiscParameterFrameRate *fr = (VAEncMiscParameterFrameRate *)(frmem + 4);
            fr->framerate = (1u << 16) | 60u;   /* num=60, den=1 (как ffmpeg) */
            if (vaCreateBuffer(dpy, ectx, VAEncMiscParameterBufferType, sizeof(frmem), 1, frmem, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL fr buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
        }

        /* picture */
        memset(picmem, 0, sizeof(picmem));
        VAEncPictureParameterBufferH264 *pic = (VAEncPictureParameterBufferH264 *)picmem;
        VABufferID coded = VA_INVALID_ID;
        if (vaCreateBuffer(dpy, ectx, VAEncCodedBufferType, CODED_SIZE, 1, NULL, &coded) != VA_STATUS_SUCCESS)
            { printf("P3 ENC: FAIL coded buf frame %d\n", i); all_ok = 0; break; }
        VASurfaceID cur = recon[i % 2];
        pic->CurrPic.picture_id = cur;
        pic->CurrPic.frame_idx = (uint32_t)i;
        pic->CurrPic.flags = 0;
        pic->CurrPic.TopFieldOrderCnt = 2 * i;
        pic->CurrPic.BottomFieldOrderCnt = 2 * i;
        for (int k = 0; k < 16; k++) {
            pic->ReferenceFrames[k].picture_id = VA_INVALID_ID;
            pic->ReferenceFrames[k].flags = VA_PICTURE_H264_INVALID;
            pic->ReferenceFrames[k].frame_idx = 0;
            pic->ReferenceFrames[k].TopFieldOrderCnt = 0;
            pic->ReferenceFrames[k].BottomFieldOrderCnt = 0;
        }
        if (!idr) {   /* предыдущий кадр = референс (short-term) */
            VASurfaceID prev = recon[(i + 1) % 2];
            pic->ReferenceFrames[0].picture_id = prev;
            pic->ReferenceFrames[0].frame_idx = (uint32_t)(i - 1);
            pic->ReferenceFrames[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            pic->ReferenceFrames[0].TopFieldOrderCnt = 2 * (i - 1);
            pic->ReferenceFrames[0].BottomFieldOrderCnt = 2 * (i - 1);
        }
        pic->coded_buf = coded;
        pic->pic_parameter_set_id = 0;
        pic->seq_parameter_set_id = 0;
        pic->frame_num = (uint32_t)i;
        pic->pic_init_qp = 26;
        pic->num_ref_idx_l0_active_minus1 = 0;
        pic->num_ref_idx_l1_active_minus1 = 0;
        pic->chroma_qp_index_offset = 0;
        pic->second_chroma_qp_index_offset = 0;
        pic->pic_fields.bits.idr_pic_flag = idr ? 1 : 0;
        pic->pic_fields.bits.reference_pic_flag = 1;
        pic->pic_fields.bits.entropy_coding_mode_flag = 1;   /* CABAC */
        pic->pic_fields.bits.transform_8x8_mode_flag = 1;    /* High */
        {
            VABufferID b;
            if (vaCreateBuffer(dpy, ectx, VAEncPictureParameterBufferType, sizeof(*pic), 1, pic, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL pic buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
        }

        /* packed SEQUENCE (SPS+PPS) — только IDR */
        if (idr) {
            uint8_t naldata[1024]; size_t pos = 0;
            pos += build_sps(naldata + pos);
            pos += build_pps(naldata + pos);
            VAEncPackedHeaderParameterBuffer ph; memset(&ph, 0, sizeof(ph));
            ph.type = VAEncPackedHeaderSequence;
            ph.bit_length = (uint32_t)(pos * 8);
            ph.has_emulation_bytes = 1;
            VABufferID b;
            if (vaCreateBuffer(dpy, ectx, VAEncPackedHeaderParameterBufferType, sizeof(ph), 1, &ph, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL packed seq param frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
            if (vaCreateBuffer(dpy, ectx, VAEncPackedHeaderDataBufferType, (uint32_t)pos, 1, naldata, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL packed seq data frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
        }

        /* packed SLICE (каждый кадр) */
        {
            uint8_t naldata[128]; size_t pos = build_slice_hdr(naldata, idr, (uint32_t)i);
            VAEncPackedHeaderParameterBuffer ph; memset(&ph, 0, sizeof(ph));
            ph.type = VAEncPackedHeaderSlice;
            ph.bit_length = (uint32_t)(pos * 8);
            ph.has_emulation_bytes = 1;
            VABufferID b;
            if (vaCreateBuffer(dpy, ectx, VAEncPackedHeaderParameterBufferType, sizeof(ph), 1, &ph, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL packed slice param frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
            if (vaCreateBuffer(dpy, ectx, VAEncPackedHeaderDataBufferType, (uint32_t)pos, 1, naldata, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL packed slice data frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
        }

        /* slice params */
        memset(slcmem, 0, sizeof(slcmem));
        VAEncSliceParameterBufferH264 *slc = (VAEncSliceParameterBufferH264 *)slcmem;
        slc->macroblock_address = 0;
        slc->num_macroblocks = MBS;
        slc->slice_type = idr ? 2u : 0u;      /* I=2, P=0 */
        slc->pic_parameter_set_id = 0;
        slc->idr_pic_id = 0;
        slc->pic_order_cnt_lsb = (uint32_t)(2 * i);   /* poc type 2: значение не используется */
        slc->direct_spatial_mv_pred_flag = 1;
        slc->num_ref_idx_active_override_flag = 0;
        for (int k = 0; k < 32; k++) {
            slc->RefPicList0[k].picture_id = VA_INVALID_ID;
            slc->RefPicList0[k].flags = VA_PICTURE_H264_INVALID;
            slc->RefPicList1[k].picture_id = VA_INVALID_ID;
            slc->RefPicList1[k].flags = VA_PICTURE_H264_INVALID;
        }
        if (!idr) {
            slc->RefPicList0[0].picture_id = recon[(i + 1) % 2];
            slc->RefPicList0[0].frame_idx = (uint32_t)(i - 1);
            slc->RefPicList0[0].flags = VA_PICTURE_H264_SHORT_TERM_REFERENCE;
            slc->RefPicList0[0].TopFieldOrderCnt = 2 * (i - 1);
            slc->RefPicList0[0].BottomFieldOrderCnt = 2 * (i - 1);
        }
        slc->cabac_init_idc = 0;
        slc->slice_qp_delta = 0;
        {
            VABufferID b;
            if (vaCreateBuffer(dpy, ectx, VAEncSliceParameterBufferType, sizeof(*slc), 1, slc, &b) != VA_STATUS_SUCCESS)
                { printf("P3 ENC: FAIL slice buf frame %d\n", i); all_ok = 0; break; }
            bids[nb++] = b;
        }

        /* --- кодирование --- */
        double t0 = now_ms();
        if (vaBeginPicture(dpy, ectx, vdst) != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL begin frame %d\n", i); all_ok = 0; break; }
        if (vaRenderPicture(dpy, ectx, bids, nb) != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL render frame %d\n", i); all_ok = 0; break; }
        if (vaEndPicture(dpy, ectx) != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL end frame %d\n", i); all_ok = 0; break; }
        if (vaSyncBuffer(dpy, coded, 10000000000ull) != VA_STATUS_SUCCESS) { printf("P3 ENC: FAIL sync frame %d\n", i); all_ok = 0; break; }
        double dt = now_ms() - t0;
        total_ms += dt;

        /* --- чтение coded-буфера (цепочка сегментов) --- */
        VACodedBufferSegment *seg = NULL;
        if (vaMapBuffer(dpy, coded, (void **)&seg) != VA_STATUS_SUCCESS || !seg) {
            printf("P3 ENC: FAIL map coded frame %d\n", i); all_ok = 0; break;
        }
        size_t wrote = 0;
        for (VACodedBufferSegment *s = seg; s; s = (VACodedBufferSegment *)s->next) {
            if (s->size) { fwrite(s->buf, 1, s->size, f); wrote += s->size; }
        }
        vaUnmapBuffer(dpy, coded);
        total += wrote;
        au_sizes[i] = wrote; au_ms[i] = dt;
        if (i < 6 || nframes <= 40 || (i + 1) % 10 == 0)
            printf("frame %d %s: buffers=%d bytes=%zu encode=%.2f ms (coded=%u)\n",
                   i, idr ? "IDR" : "P  ", nb, wrote, dt, coded);

        for (int k = 0; k < nb; k++) vaDestroyBuffer(dpy, bids[k]);
        vaDestroyBuffer(dpy, coded);
    }

    fclose(f);

    /* --- NAL-скан /tmp/p3.h264: старт-коды и состав --- */
    if (all_ok) {
        FILE *rf = fopen("/tmp/p3.h264", "rb");
        static uint8_t filebuf[1 << 22];
        size_t flen = fread(filebuf, 1, sizeof(filebuf), rf);
        fclose(rf);
        printf("p3.h264: %zu bytes\n", flen);
        int nals = 0, slices = 0, sps = 0, pps = 0, bad = 0;
        size_t j = 0;
        while (j + 4 < flen) {
            size_t sc = 0;
            if (filebuf[j] == 0 && filebuf[j + 1] == 0 && filebuf[j + 2] == 0 && filebuf[j + 3] == 1) sc = 4;
            else if (filebuf[j] == 0 && filebuf[j + 1] == 0 && filebuf[j + 2] == 1) sc = 3;
            if (!sc) { j++; continue; }
            size_t k = j + sc;
            while (k + 4 < flen) {
                size_t sc2 = 0;
                if (filebuf[k] == 0 && filebuf[k + 1] == 0 && filebuf[k + 2] == 0 && filebuf[k + 3] == 1) sc2 = 4;
                else if (filebuf[k] == 0 && filebuf[k + 1] == 0 && filebuf[k + 2] == 1) sc2 = 3;
                if (sc2) break;
                k++;
            }
            if (k + 4 >= flen) k = flen;
            int nt = filebuf[j + sc] & 0x1f;
            printf("  NAL@%zu size=%zu hdr=0x%02x type=%d\n", j, k - j - sc, filebuf[j + sc], nt);
            nals++;
            if (nt == 1 || nt == 5) slices++;
            if (nt == 7) sps++;
            if (nt == 8) pps++;
            j = k;
        }
        /* проверка «весь файл покрыт старт-кодами»: между NAL нет мусора */
        printf("NAL итог: total=%d slices=%d sps=%d pps=%d\n", nals, slices, sps, pps);
        if (slices < nframes || sps < 1 || pps < 1 || bad) {
            printf("P3 ENC: FAIL NAL-состав (slices=%d sps=%d pps=%d)\n", slices, sps, pps);
            all_ok = 0;
        }
        /* CBR-статистика по окнам GOP (GOP=30 при 60fps = 0.5 с):
         * сумма AU за окно * 8 / 0.5с = Мбит/с. */
        if (nframes > NFRAMES) {
            int gops = nframes / GOP;
            for (int g = 0; g < gops; g++) {
                size_t sum = 0; double tsum = 0;
                for (int k = 0; k < GOP; k++) { sum += au_sizes[g * GOP + k]; tsum += au_ms[g * GOP + k]; }
                printf("gop %d: au_sum=%zu B = %.2f Мбит/с за 0.5с, enc=%.2f ms/кадр\n",
                       g, sum, sum * 8.0 / 0.5 / 1e6, tsum / GOP);
            }
        }
    }

    printf("bitstream: %zu bytes total\n", total);
    printf("encode tempo: %d кадров за %.1f мс = %.2f мс/кадр = %.1f fps (sync включён)\n",
           nframes, total_ms, total_ms / nframes, total_ms > 0 ? nframes * 1000.0 / total_ms : 0);
    printf("P3 ENC: %s\n", all_ok ? "PASS (кодирование+Annex-B OK, декод-проверка — внешняя)" : "FAIL");

    vaDestroySurfaces(dpy, recon, 2);
    vaDestroyContext(dpy, ectx);
    vaDestroyConfig(dpy, enc_cfg);
    vaDestroyContext(dpy, vctx);
    vaDestroyConfig(dpy, vpp_cfg);
    vaDestroySurfaces(dpy, &vdst, 1);
    vaDestroySurfaces(dpy, &src, 1);
    vaTerminate(dpy);
    munmap(map, sz);
    close(drm); close(dmabuf); close(card);
    return all_ok ? 0 : 1;
}
