/* P2b: почему импорт dumb-dmabuf даёт «success», но VA видит нули?
 *
 * Базовый факт из va_p2_vpp.c: cpu-after-fill показывает паттерн (в т.ч. после
 * инверсии), а vaGetImage(импортированной поверхности) и VPP-выход — всегда
 * нули. Здесь перебираются варианты атрибутов импорта, а контрольный опыт
 * (plain-поверхность + vaPutImage + VPP) отделяет «VPP вообще работает»
 * от «импорт не биндится».
 *
 * Варианты импорта:
 *   A: baseline (rt=RGB32, flags=0) — нули (известно);
 *   B: +VASurfaceAttribPixelFormat=BGRX;
 *   C: flags=SETTABLE на обоих атрибутах;
 *   D: B+C;
 *   E: B + fourcc=VA_FOURCC_BGRA в дескрипторе;
 *   F: rt_format=0 + B.
 *
 * Сборка: gcc -O0 -g -o /tmp/va_imp tools/va_p2_impvar.c $(pkg-config --cflags --libs libva libva-drm) -lm
 */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <math.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>
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
#define NBARS 8
static const uint8_t bars[NBARS][4] = {
    {0, 0, 255, 0}, {0, 255, 0, 0}, {255, 0, 0, 0}, {0, 255, 255, 0},
    {255, 0, 255, 0}, {255, 255, 0, 0}, {64, 64, 64, 0}, {192, 192, 192, 0},
};
static const char *bar_names[NBARS] = {
    "red", "green", "blue", "cyan", "magenta", "yellow", "gray64", "gray192"
};

static void fill_pattern(uint8_t *p, uint32_t pitch) {
    for (uint32_t y = 0; y < H; y++) {
        uint8_t *row = p + (size_t)y * pitch;
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t *c = bars[(x * NBARS) / W];
            row[x * 4 + 0] = c[0]; row[x * 4 + 1] = c[1];
            row[x * 4 + 2] = c[2]; row[x * 4 + 3] = 0;
        }
    }
}

static void print_rgb(const uint8_t *buf, uint32_t pitch, const char *tag) {
    printf("%s: ", tag);
    int nz = 0;
    for (int i = 0; i < NBARS; i++) {
        int x = i * (W / NBARS) + (W / NBARS) / 2;
        const uint8_t *px = buf + (size_t)400 * pitch + (size_t)x * 4;
        printf("%s=(%d,%d,%d) ", bar_names[i], px[2], px[1], px[0]);
        if (px[0] || px[1] || px[2]) nz++;
    }
    printf("%s\n", nz >= 6 ? "[CONTENT=YES]" : "[CONTENT=NO]");
}

static int find_fmt(VADisplay dpy, uint32_t fourcc, VAImageFormat *out) {
    VAImageFormat f[64]; int n = 0;
    vaQueryImageFormats(dpy, f, &n);
    memset(out, 0, sizeof(*out));
    for (int i = 0; i < n; i++)
        if (f[i].fourcc == fourcc) { *out = f[i]; return 1; }
    return 0;
}

static void va_dump_bgrx(VADisplay dpy, VASurfaceID s, const char *tag) {
    VAImageFormat fmt;
    if (!find_fmt(dpy, VA_FOURCC_BGRX, &fmt)) { printf("%s: нет BGRX-формата\n", tag); return; }
    VAImage im;
    if (vaCreateImage(dpy, &fmt, W, H, &im) != VA_STATUS_SUCCESS) { printf("%s: create FAIL\n", tag); return; }
    if (vaGetImage(dpy, s, 0, 0, W, H, im.image_id) != VA_STATUS_SUCCESS) {
        printf("%s: get FAIL\n", tag); vaDestroyImage(dpy, im.image_id); return;
    }
    void *p = NULL;
    if (vaMapBuffer(dpy, im.buf, &p) == VA_STATUS_SUCCESS && p)
        print_rgb((const uint8_t *)p + im.offsets[0], im.pitches[0], tag);
    else printf("%s: map FAIL\n", tag);
    vaDestroyImage(dpy, im.image_id);
}

static void va_dump_y(VADisplay dpy, VASurfaceID s, const char *tag) {
    VAImageFormat fmt;
    if (!find_fmt(dpy, VA_FOURCC_NV12, &fmt)) { printf("%s: нет NV12-формата\n", tag); return; }
    VAImage im;
    if (vaCreateImage(dpy, &fmt, W, H, &im) != VA_STATUS_SUCCESS) { printf("%s: create FAIL\n", tag); return; }
    if (vaGetImage(dpy, s, 0, 0, W, H, im.image_id) != VA_STATUS_SUCCESS) {
        printf("%s: get FAIL\n", tag); vaDestroyImage(dpy, im.image_id); return;
    }
    void *p = NULL;
    if (vaMapBuffer(dpy, im.buf, &p) == VA_STATUS_SUCCESS && p) {
        const uint8_t *b = (const uint8_t *)p;
        printf("%s:", tag);
        int nz = 0;
        for (int i = 0; i < NBARS; i++) {
            int x = i * (W / NBARS) + (W / NBARS) / 2;
            uint8_t Y = b[im.offsets[0] + (size_t)400 * im.pitches[0] + x];
            printf(" %d", Y);
            if (Y > 20) nz++;
        }
        printf(" %s\n", nz >= 5 ? "[NONBLACK=YES]" : "[NONBLACK=NO]");
        vaUnmapBuffer(dpy, im.buf);
    } else printf("%s: map FAIL\n", tag);
    vaDestroyImage(dpy, im.image_id);
}

static int run_vpp(VADisplay dpy, VAContextID ctx, VASurfaceID src, VASurfaceID dst) {
    VAProcPipelineParameterBuffer pp;
    memset(&pp, 0, sizeof(pp));
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

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);

    int card = -1;
    const char *cards[] = {"/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2", "/dev/dri/card3"};
    for (unsigned i = 0; i < 4; i++) {
        if (access(cards[i], F_OK) != 0) continue;
        card = open(cards[i], O_RDWR | O_CLOEXEC);
        if (card >= 0) break;
    }
    struct create_dumb dumb = {.height = H, .width = W, .bpp = 32};
    ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &dumb);
    struct prime_handle ph = {.handle = dumb.handle, .flags = O_RDWR | O_CLOEXEC, .fd = -1};
    ioctl(card, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph);
    int dmabuf = ph.fd;
    size_t sz = (size_t)dumb.pitch * H;
    uint8_t *map = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, dmabuf, 0);
    if (map == MAP_FAILED) { printf("FAIL mmap\n"); return 1; }
    fill_pattern(map, dumb.pitch);
    printf("dumb pitch=%u fd=%d\n", dumb.pitch, dmabuf);
    print_rgb(map, dumb.pitch, "cpu-after-fill");

    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    VADisplay dpy = vaGetDisplayDRM(drm);
    int maj, min;
    if (vaInitialize(dpy, &maj, &min) != VA_STATUS_SUCCESS) { printf("FAIL va-init\n"); return 1; }

    /* --- варианты атрибутов импорта --- */
    struct variant {
        unsigned rt;
        uint32_t flags;
        uint32_t fourcc;
        int with_pixelformat;
        const char *name;
    } vars[] = {
        {VA_RT_FORMAT_RGB32, 0, VA_FOURCC_BGRX, 0, "A baseline"},
        {VA_RT_FORMAT_RGB32, 0, VA_FOURCC_BGRX, 1, "B +PixelFormat"},
        {VA_RT_FORMAT_RGB32, VA_SURFACE_ATTRIB_SETTABLE, VA_FOURCC_BGRX, 0, "C SETTABLE"},
        {VA_RT_FORMAT_RGB32, VA_SURFACE_ATTRIB_SETTABLE, VA_FOURCC_BGRX, 1, "D SETTABLE+PixFmt"},
        {VA_RT_FORMAT_RGB32, 0, VA_FOURCC_BGRA, 1, "E BGRA+PixFmt"},
        {0, 0, VA_FOURCC_BGRX, 1, "F rt0+PixFmt"},
    };
    for (unsigned v = 0; v < sizeof(vars) / sizeof(vars[0]); v++) {
        VADRMPRIMESurfaceDescriptor desc;
        memset(&desc, 0, sizeof(desc));
        desc.fourcc = vars[v].fourcc;
        desc.width = W; desc.height = H;
        desc.num_objects = 1;
        desc.objects[0].fd = dmabuf;
        desc.objects[0].size = (uint32_t)sz;
        desc.objects[0].drm_format_modifier = 0; /* LINEAR */
        desc.num_layers = 1;
        desc.layers[0].drm_format = DRM_FORMAT_XRGB8888;
        desc.layers[0].num_planes = 1;
        desc.layers[0].object_index[0] = 0;
        desc.layers[0].offset[0] = 0;
        desc.layers[0].pitch[0] = dumb.pitch;

        VASurfaceAttrib attrs[4];
        int na = 0;
        memset(attrs, 0, sizeof(attrs));
        attrs[na].type = VASurfaceAttribMemoryType;
        attrs[na].flags = vars[v].flags;
        attrs[na].value.type = VAGenericValueTypeInteger;
        attrs[na].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
        na++;
        attrs[na].type = VASurfaceAttribExternalBufferDescriptor;
        attrs[na].flags = vars[v].flags;
        attrs[na].value.type = VAGenericValueTypePointer;
        attrs[na].value.value.p = &desc;
        na++;
        if (vars[v].with_pixelformat) {
            attrs[na].type = VASurfaceAttribPixelFormat;
            attrs[na].flags = vars[v].flags ? vars[v].flags : VA_SURFACE_ATTRIB_SETTABLE;
            attrs[na].value.type = VAGenericValueTypeInteger;
            attrs[na].value.value.i = (int32_t)vars[v].fourcc;
            na++;
        }

        VASurfaceID s = VA_INVALID_ID;
        VAStatus st = vaCreateSurfaces(dpy, vars[v].rt, W, H, &s, 1, attrs, na);
        printf("variant %-18s create=%s ", vars[v].name, vaErrorStr(st));
        if (st == VA_STATUS_SUCCESS) {
            char tag[64];
            snprintf(tag, sizeof(tag), "  va[%s]", vars[v].name);
            va_dump_bgrx(dpy, s, tag);
            vaDestroySurfaces(dpy, &s, 1);
        } else {
            printf("\n");
        }
    }

    /* --- контроль: plain-поверхность через vaPutImage → виден ли контент --- */
    VASurfaceID plain = VA_INVALID_ID;
    VAStatus st = vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, W, H, &plain, 1, NULL, 0);
    printf("plain create=%s\n", vaErrorStr(st));
    VAImageFormat pfmt;
    if (find_fmt(dpy, VA_FOURCC_BGRX, &pfmt)) {
        VAImage pim;
        if (vaCreateImage(dpy, &pfmt, W, H, &pim) == VA_STATUS_SUCCESS) {
            void *p = NULL;
            if (vaMapBuffer(dpy, pim.buf, &p) == VA_STATUS_SUCCESS && p) {
                fill_pattern((uint8_t *)p + pim.offsets[0], pim.pitches[0]);
                vaUnmapBuffer(dpy, pim.buf);
                st = vaPutImage(dpy, plain, pim.image_id, 0, 0, W, H, 0, 0, W, H);
                printf("vaPutImage=%s\n", vaErrorStr(st));
            }
            vaDestroyImage(dpy, pim.image_id);
        }
    }
    va_dump_bgrx(dpy, plain, "plain-after-put");

    /* --- контроль: VPP из plain --- */
    VASurfaceID dst = VA_INVALID_ID;
    vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, &dst, 1, NULL, 0);
    VAConfigID vcfg = VA_INVALID_ID;
    vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vcfg);
    VAContextID vctx = VA_INVALID_ID;
    VAStatus cst = vaCreateContext(dpy, vcfg, W, H, 0x1, NULL, 0, &vctx);
    printf("vpp context=%s\n", vaErrorStr(cst));
    int rc = run_vpp(dpy, vctx, plain, dst);
    printf("run_vpp(plain)=%d\n", rc);
    va_dump_y(dpy, dst, "vpp-plain-y");

    printf("DONE\n");
    vaTerminate(dpy);
    munmap(map, sz);
    close(drm); close(dmabuf); close(card);
    return 0;
}
