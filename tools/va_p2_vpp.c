/* P2: VPP RGB(BGRX, импортированный dmabuf) → NV12 на radeonsi/Carrizo.
 *
 * Второй гейт zero-copy: после P1 (импорт XR24-dmabuf в VA-поверхность
 * проходит) проверяем, что видеопроцессор драйвера реально конвертирует
 * RGB-вход импортированной поверхности в NV12 и что пиксели корректны.
 *
 * Метод: кладём известный паттерн (8 вертикальных цветных полос) в dumb-буфер
 * через mmap, импортируем, прогоняем VPP → NV12-поверхность, скачиваем через
 * vaCreateImage/vaGetImage и сверяем Y каждой полосы с тремя моделями
 * (BT.601 limited, BT.709 limited, BT.601 full) плюс грубую санитацию U/V.
 * Дополнительно: цикл VPP ×30 с замером темпа (нужно ≥60 fps для продакшена).
 *
 * Сборка: gcc -O0 -g -Wall -o /tmp/va_p2 tools/va_p2_vpp.c $(pkg-config --cflags --libs libva libva-drm)
 * Запуск: /tmp/va_p2 [input-std srgb|none|bt601|bt709] [output-std bt601|bt709|none]
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
#include <time.h>
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
    "red", "green", "blue", "yellow", "magenta", "cyan", "gray64", "gray192"
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
    for (int i = 0; i < NBARS; i++) {
        int x = i * (W / NBARS) + (W / NBARS) / 2;
        const uint8_t *px = buf + (size_t)400 * pitch + (size_t)x * 4;
        printf("%s=(%d,%d,%d) ", bar_names[i], px[2], px[1], px[0]);
    }
    printf("\n");
}

/* Скачивание произвольной VA-поверхности в BGRX и печать тех же точек. */
static void va_dump_bgrx(VADisplay dpy, VASurfaceID s, const char *tag) {
    VAImageFormat ifmts[64]; int nif = 0;
    vaQueryImageFormats(dpy, ifmts, &nif);
    VAImageFormat bgrx; memset(&bgrx, 0, sizeof(bgrx));
    int fnd = 0;
    for (int i = 0; i < nif; i++)
        if (ifmts[i].fourcc == VA_FOURCC_BGRX) { bgrx = ifmts[i]; fnd = 1; }
    if (!fnd) { printf("%s: BGRX нет в vaQueryImageFormats (%d)\n", tag, nif); return; }
    VAImage im;
    if (vaCreateImage(dpy, &bgrx, W, H, &im) != VA_STATUS_SUCCESS) {
        printf("%s: vaCreateImage FAIL\n", tag); return;
    }
    if (vaGetImage(dpy, s, 0, 0, W, H, im.image_id) != VA_STATUS_SUCCESS) {
        printf("%s: vaGetImage FAIL\n", tag);
        vaDestroyImage(dpy, im.image_id);
        return;
    }
    void *p = NULL;
    if (vaMapBuffer(dpy, im.buf, &p) == VA_STATUS_SUCCESS && p) {
        print_rgb((const uint8_t *)p + im.offsets[0], im.pitches[0], tag);
        vaUnmapBuffer(dpy, im.buf);
    } else {
        printf("%s: vaMapBuffer FAIL\n", tag);
    }
    vaDestroyImage(dpy, im.image_id);
}

static double now_ms(void) {
    struct timespec ts;
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return ts.tv_sec * 1000.0 + ts.tv_nsec / 1e6;
}

/* ВАЖНО (урок E0/E1-крашей): Begin/Render/End/CreateBuffer принимают VAContextID
 * (после vaCreateContext), а НЕ VAConfigID. Передача config-а как context даёт
 * чтение несуществующей структуры контекста в драйвере → дикий прыжок в мусор
 * (воспроизводилось: PC == si_addr в данных libXau). Эталон последовательности —
 * LIBVA_TRACE живого ffmpeg: CreateConfig → vaCreateContext(config, 1088, 800,
 * flag=0x1, num_render_targets=0) → BeginPicture(context). */
static int run_vpp(VADisplay dpy, VAContextID ctx, VASurfaceID src, VASurfaceID dst,
                   VAProcColorStandardType in_std, VAProcColorStandardType out_std) {
    VAProcPipelineParameterBuffer pp; memset(&pp, 0, sizeof(pp));
    pp.surface = src;
    pp.surface_region = NULL;
    pp.output_region = NULL;
    pp.surface_color_standard = in_std;
    pp.output_color_standard = out_std;
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

static VAProcColorStandardType parse_std(const char *s) {
    if (!s) return (VAProcColorStandardType)-1;
    if (!strcmp(s, "srgb")) return VAProcColorStandardSRGB;
    if (!strcmp(s, "none")) return VAProcColorStandardNone;
    if (!strcmp(s, "bt601")) return VAProcColorStandardBT601;
    if (!strcmp(s, "bt709")) return VAProcColorStandardBT709;
    return (VAProcColorStandardType)-1;
}

/* Y-модели по известным RGB полос */
static double y601l(double r, double g, double b) { return 0.257 * r + 0.504 * g + 0.098 * b + 16; }
static double y709l(double r, double g, double b) { return 0.187 * r + 0.614 * g + 0.071 * b + 16; }
static double y601f(double r, double g, double b) { return 0.299 * r + 0.587 * g + 0.114 * b; }

int main(int argc, char **argv) {
    setvbuf(stdout, NULL, _IONBF, 0);
    VAProcColorStandardType in_std = parse_std(argc > 1 ? argv[1] : "srgb");
    VAProcColorStandardType out_std = parse_std(argc > 2 ? argv[2] : "bt601");
    if ((int)in_std < 0 || (int)out_std < 0) { printf("bad args\n"); return 2; }

    /* --- dumb-буфер + паттерн (как P1) --- */
    int card = -1; const char *card_path = NULL;
    const char *cards[] = {"/dev/dri/card0", "/dev/dri/card1", "/dev/dri/card2", "/dev/dri/card3"};
    for (unsigned i = 0; i < 4; i++) {
        if (access(cards[i], F_OK) != 0) continue;
        card = open(cards[i], O_RDWR | O_CLOEXEC);
        if (card >= 0) { card_path = cards[i]; break; }
    }
    if (card < 0) { printf("P2 VPP: FAIL open card*\n"); return 1; }
    struct create_dumb dumb = {.height = H, .width = W, .bpp = 32};
    if (ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) < 0) { printf("P2 VPP: FAIL dumb: %s\n", strerror(errno)); return 1; }
    struct prime_handle ph = {.handle = dumb.handle, .flags = O_RDWR | O_CLOEXEC, .fd = -1};
    if (ioctl(card, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0) { printf("P2 VPP: FAIL prime: %s\n", strerror(errno)); return 1; }
    int dmabuf = ph.fd;
    size_t sz = (size_t)dumb.pitch * H;
    uint8_t *map = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, dmabuf, 0);
    if (map == MAP_FAILED) { printf("P2 VPP: FAIL mmap: %s\n", strerror(errno)); return 1; }
    for (uint32_t y = 0; y < H; y++) {
        uint8_t *row = map + (size_t)y * dumb.pitch;
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t *c = bars[(x * NBARS) / W];
            row[x * 4 + 0] = c[0]; row[x * 4 + 1] = c[1];
            row[x * 4 + 2] = c[2]; row[x * 4 + 3] = 0;
        }
    }
    /* map держим открытым до конца — нужен для контрольных перечитываний
     * (cpu → va → cpu-rewrite → va): локализация «кто видит контент». */
    print_rgb(map, dumb.pitch, "cpu-after-fill");

    /* --- VA --- */
    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    VADisplay dpy = vaGetDisplayDRM(drm);
    int maj, min;
    if (vaInitialize(dpy, &maj, &min) != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL vaInitialize\n"); return 1; }

    /* импорт (P1-комбинация, прошедшая) */
    VADRMPRIMESurfaceDescriptor desc; memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_BGRX; desc.width = W; desc.height = H;
    desc.num_objects = 1;
    desc.objects[0].fd = dmabuf; desc.objects[0].size = (uint32_t)sz;
    desc.num_layers = 1;
    desc.layers[0].drm_format = DRM_FORMAT_XRGB8888; desc.layers[0].num_planes = 1;
    desc.layers[0].pitch[0] = dumb.pitch;
    /* flags=SETTABLE обязателен: без него radeonsi/26.2.5 МОЛЧА ИГНОРИРУЕТ оба
     * атрибута импорта (vaCreateSurfaces=success, но поверхность пустая, VA
     * видит нули). Факт: пробник va_p2_impvar.c варианты A(flags=0)/B/E →
     * [CONTENT=NO], C/D (SETTABLE) → [CONTENT=YES]. Эталон ffmpeg тоже пишет
     * flags=3(SETTABLE|GETTABLE) на surface-атрибутах (LIBVA_TRACE). */
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
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL import: %s\n", vaErrorStr(st)); return 1; }
    printf("stage: dumb+pattern+va-init ok, import ok surf=%u\n", src);

    /* Контроль видимости контента: cpu-after-fill (выше) → va-import-1 →
     * инверсия записи на живом буфере → cpu-inverted → va-import-2. */
    va_dump_bgrx(dpy, src, "va-import-1");
    for (size_t i = 0; i < sz; i++) map[i] = (uint8_t)(255 - map[i]);
    print_rgb(map, dumb.pitch, "cpu-inverted");
    va_dump_bgrx(dpy, src, "va-import-2");
    /* после инверсии возвращаем исходный паттерн — VPP-сверка считает по нему */
    fill_pattern(map, dumb.pitch);
    print_rgb(map, dumb.pitch, "cpu-restored");

    /* выходная NV12-поверхность + VPP-конфиг */
    VASurfaceID dst = VA_INVALID_ID;
    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, &dst, 1, NULL, 0);
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL dst surface: %s\n", vaErrorStr(st)); return 1; }
    VAConfigID vpp_cfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vpp_cfg);
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL config: %s\n", vaErrorStr(st)); return 1; }
    VAContextID vctx = VA_INVALID_ID;
    st = vaCreateContext(dpy, vpp_cfg, W, H, 0x1, NULL, 0, &vctx);
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL context: %s\n", vaErrorStr(st)); return 1; }

    /* VPP-цикл: 1 прогон с проверкой + 30 прогонов на темп */
    printf("stage: import ok, dst+cfg+ctx ok, run vpp\n");
    int rc = run_vpp(dpy, vctx, src, dst, in_std, out_std);
    if (rc != 0) { printf("P2 VPP: FAIL run_vpp=%d (%s)\n", rc, vaErrorStr(st)); return 1; }

    double t0 = now_ms();
    int ok30 = 1;
    for (int i = 0; i < 30; i++) if (run_vpp(dpy, vctx, src, dst, in_std, out_std) != 0) { ok30 = 0; break; }
    double dt = now_ms() - t0;
    printf("vpp tempo: 30 прогонов за %.1f мс = %.2f мс/кадр = %.1f fps (sync включён)%s\n",
           dt, dt / 30.0, 30000.0 / dt, ok30 ? "" : " [ОШИБКА В ЦИКЛЕ]");

    /* скачивание NV12 */
    VAImageFormat fmts[64]; int nf = 0;
    vaQueryImageFormats(dpy, fmts, &nf);
    VAImageFormat nv12; int found = 0;
    memset(&nv12, 0, sizeof(nv12));
    for (int i = 0; i < nf; i++)
        if (fmts[i].fourcc == VA_FOURCC_NV12) { nv12 = fmts[i]; found = 1; }
    if (!found) { printf("P2 VPP: FAIL NV12 не в vaQueryImageFormats (%d форматов)\n", nf); return 1; }
    VAImage img;
    st = vaCreateImage(dpy, &nv12, W, H, &img);
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL vaCreateImage: %s\n", vaErrorStr(st)); return 1; }
    st = vaGetImage(dpy, dst, 0, 0, W, H, img.image_id);
    if (st != VA_STATUS_SUCCESS) { printf("P2 VPP: FAIL vaGetImage: %s\n", vaErrorStr(st)); return 1; }
    void *ptr = NULL;
    st = vaMapBuffer(dpy, img.buf, &ptr);
    if (st != VA_STATUS_SUCCESS || !ptr) { printf("P2 VPP: FAIL vaMapBuffer: %s\n", vaErrorStr(st)); return 1; }
    uint8_t *base = (uint8_t *)ptr;
    uint8_t *yplane = base + img.offsets[0];
    uint8_t *uvplane = base + img.offsets[1];
    printf("image: pitches Y=%u UV=%u offsets Y=%u UV=%u\n",
           img.pitches[0], img.pitches[1], img.offsets[0], img.offsets[1]);

    int ypass = 0, uverr = 0;
    printf("%-8s %5s | %6s %6s %6s | %5s %5s\n", "bar", "measY", "601L", "709L", "601F", "U", "V");
    for (int i = 0; i < NBARS; i++) {
        int x = i * (W / NBARS) + (W / NBARS) / 2;
        int y = H / 2;
        uint8_t Y = yplane[(size_t)y * img.pitches[0] + x];
        uint8_t U = uvplane[(size_t)(y / 2) * img.pitches[1] + (x & ~1) + 0];
        uint8_t V = uvplane[(size_t)(y / 2) * img.pitches[1] + (x & ~1) + 1];
        double r = bars[i][2], g = bars[i][1], b = bars[i][0];
        double e1 = y601l(r, g, b), e2 = y709l(r, g, b), e3 = y601f(r, g, b);
        double d1 = fabs(Y - e1), d2 = fabs(Y - e2), d3 = fabs(Y - e3);
        double dbest = d1; if (d2 < dbest) dbest = d2; if (d3 < dbest) dbest = d3;
        /* Порог ±8: контрольный эталон (plain+vaPutImage → VPP, тот же драйвер)
         * даёт отклонение от модели BT.601-limited до 7 (драйверное округление):
         * см. /tmp/e29_impvar.txt "vpp-plain-y: 85 149 41 214 108 171 78 185". */
        int pass = dbest <= 8.0;
        ypass += pass;
        printf("%-8s %5d | %6.1f %6.1f %6.1f | %5d %5d %s\n",
               bar_names[i], Y, e1, e2, e3, U, V, pass ? "OK" : "MISS");
        /* санитация U/V (601 limited): red V высокий, green U низкий, blue U высокий */
        if (i == 0 && V < 230) uverr++;
        if (i == 1 && U > 70) uverr++;
        if (i == 2 && U < 235) uverr++;
    }
    printf("Y-сверка: %d/%8d полос в пределах ±8 от лучшей модели; U/V-санитация: %s\n",
           ypass, NBARS, uverr == 0 ? "OK" : "FAIL");

    if (ypass >= 7 && uverr == 0 && ok30) printf("P2 VPP: PASS (in=%d out=%d)\n", (int)in_std, (int)out_std);
    else printf("P2 VPP: FAIL ypass=%d uverr=%d ok30=%d (in=%d out=%d)\n",
                ypass, uverr, ok30, (int)in_std, (int)out_std);

    vaUnmapBuffer(dpy, img.buf);
    vaDestroyImage(dpy, img.image_id);
    munmap(map, sz);
    vaDestroyContext(dpy, vctx);
    vaDestroyConfig(dpy, vpp_cfg);
    vaDestroySurfaces(dpy, &dst, 1);
    vaDestroySurfaces(dpy, &src, 1);
    vaTerminate(dpy);
    close(drm); close(dmabuf); close(card);
    return (ypass >= 7 && uverr == 0 && ok30) ? 0 : 1;
}
