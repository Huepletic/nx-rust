/* P1: импорт dumb-dmabuf (XRGB8888, LINEAR, страйд ядра) в VA-API поверхность
 * на radeonsi/Carrizo. Это первый (решающий) гейт zero-copy: если драйвер не
 * принимает dmabuf входом для дальнейшего VPP/encode — вся архитектура «наш
 * libva-кодировщик» отпадает.
 *
 * Что проверяем:
 *  - dumb-буфер создаётся на /dev/dri/cardN (как в src/dma.rs), fd через PRIME;
 *  - VA-дисплей на /dev/dri/renderD128;
 *  - vaCreateSurfaces с VASurfaceAttribMemoryType=DRM_PRIME_2 +
 *    VASurfaceAttribExternalBufferDescriptor (VADRMPRIMESurfaceDescriptor);
 *  - перебор комбинаций (rt_format, fourcc, flags атрибутов).
 *
 * Сборка: gcc -O0 -g -o /tmp/va_p1 tools/va_p1_import.c $(pkg-config --cflags --libs libva libva-drm)
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
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>
#include <va/va_drmcommon.h>

/* ioctl-числа дословно из src/dma.rs (проверены замером: pitch=4352 на 1088) */
#define DRM_IOCTL_BASE 0x64u
#define IOWR(nr, sz) ((3u << 30) | ((sz) << 16) | (DRM_IOCTL_BASE << 8) | (nr))
#define DRM_IOCTL_MODE_CREATE_DUMB IOWR(0xB2, 24)
#define DRM_IOCTL_PRIME_HANDLE_TO_FD IOWR(0x2D, 12)

struct create_dumb {
    uint32_t height, width, bpp, flags, handle, pitch;
};
struct prime_handle {
    uint32_t handle, flags;
    int32_t fd;
};

/* drm_fourcc.h: DRM_FORMAT_XRGB8888 = 'XR24' (LE-число; в памяти B,G,R,X) */
#define DRM_FORMAT_XRGB8888 0x34325258u

#define W 1088
#define H 800

static void fill_pattern(uint8_t *m, uint32_t pitch) {
    /* 8 вертикальных цветных полос + верхняя серая строка-контроль страйда */
    static const uint8_t bars[8][4] = {
        {0, 0, 255, 0}, {0, 255, 0, 0}, {255, 0, 0, 0}, {0, 255, 255, 0},
        {255, 0, 255, 0}, {255, 255, 0, 0}, {64, 64, 64, 0}, {192, 192, 192, 0},
    };
    for (uint32_t y = 0; y < H; y++) {
        uint8_t *row = m + (size_t)y * pitch;
        for (uint32_t x = 0; x < W; x++) {
            const uint8_t *c = bars[(x * 8) / W];
            row[x * 4 + 0] = c[0];
            row[x * 4 + 1] = c[1];
            row[x * 4 + 2] = c[2];
            row[x * 4 + 3] = (y == 0) ? 0x40 : 0x00; /* X-канал: контроль чтения */
        }
    }
}

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);

    /* 1. dumb-буфер на cardN (та же последовательность, что в src/dma.rs) */
    int card = -1;
    const char *card_path = NULL;
    const char *cards[] = {"/dev/dri/card0", "/dev/dri/card1",
                           "/dev/dri/card2", "/dev/dri/card3"};
    for (unsigned i = 0; i < sizeof(cards) / sizeof(cards[0]); i++) {
        if (access(cards[i], F_OK) != 0) continue;
        card = open(cards[i], O_RDWR | O_CLOEXEC);
        if (card >= 0) { card_path = cards[i]; break; }
    }
    if (card < 0) { printf("P1 IMPORT: FAIL open card* %s\n", strerror(errno)); return 1; }

    struct create_dumb dumb = {.height = H, .width = W, .bpp = 32};
    if (ioctl(card, DRM_IOCTL_MODE_CREATE_DUMB, &dumb) < 0) {
        printf("P1 IMPORT: FAIL MODE_CREATE_DUMB %s: %s\n", card_path, strerror(errno));
        return 1;
    }
    struct prime_handle ph = {.handle = dumb.handle, .flags = O_RDWR | O_CLOEXEC, .fd = -1};
    if (ioctl(card, DRM_IOCTL_PRIME_HANDLE_TO_FD, &ph) < 0) {
        printf("P1 IMPORT: FAIL PRIME_HANDLE_TO_FD %s: %s\n", card_path, strerror(errno));
        return 1;
    }
    int dmabuf = ph.fd;
    printf("dumb: card=%s handle=%u pitch=%u size=%u dmabuf_fd=%d\n",
           card_path, dumb.handle, dumb.pitch, dumb.pitch * H, dmabuf);

    /* CPU-доступ к dmabuf (как sink в проде): заодно кладём контрольный паттерн */
    size_t sz = (size_t)dumb.pitch * H;
    uint8_t *map = mmap(NULL, sz, PROT_READ | PROT_WRITE, MAP_SHARED, dmabuf, 0);
    if (map == MAP_FAILED) { printf("P1 IMPORT: FAIL mmap dmabuf: %s\n", strerror(errno)); return 1; }
    fill_pattern(map, dumb.pitch);
    munmap(map, sz);

    /* 2. VA-дисплей на render-ноде */
    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (drm < 0) { printf("P1 IMPORT: FAIL open renderD128: %s\n", strerror(errno)); return 1; }
    VADisplay dpy = vaGetDisplayDRM(drm);
    if (!dpy) { printf("P1 IMPORT: FAIL vaGetDisplayDRM\n"); return 1; }
    int maj = 0, min = 0;
    VAStatus st = vaInitialize(dpy, &maj, &min);
    if (st != VA_STATUS_SUCCESS) {
        printf("P1 IMPORT: FAIL vaInitialize: %s\n", vaErrorStr(st));
        return 1;
    }
    printf("va: %d.%d driver-ok\n", maj, min);

    /* 3. профили/энтрипоинты (context для P2/P3) */
    int maxp = vaMaxNumProfiles(dpy);
    VAProfile *profs = calloc((size_t)maxp, sizeof(VAProfile));
    int np = 0;
    vaQueryConfigProfiles(dpy, profs, &np);
    for (int i = 0; i < np; i++) {
        int maxe = vaMaxNumEntrypoints(dpy);
        VAProfile p = profs[i];
        VAEntrypoint *eps = calloc((size_t)maxe, sizeof(*eps));
        int ne = 0;
        vaQueryConfigEntrypoints(dpy, p, eps, &ne);
        for (int j = 0; j < ne; j++) {
            if ((p == VAProfileH264Main && eps[j] == VAEntrypointEncSlice) ||
                (p == VAProfileNone && eps[j] == VAEntrypointVideoProc) ||
                (p == VAProfileH264ConstrainedBaseline && eps[j] == VAEntrypointEncSlice)) {
                printf("profile ok: %d entrypoint %d\n", (int)p, (int)eps[j]);
            }
        }
        free(eps);
    }
    free(profs);

    /* 4. импорт: перебор комбинаций атрибутов */
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_BGRX;
    desc.width = W;
    desc.height = H;
    desc.num_objects = 1;
    desc.objects[0].fd = dmabuf;
    desc.objects[0].size = dumb.pitch * H;
    desc.objects[0].drm_format_modifier = 0; /* DRM_FORMAT_MOD_LINEAR */
    desc.num_layers = 1;
    desc.layers[0].drm_format = DRM_FORMAT_XRGB8888;
    desc.layers[0].num_planes = 1;
    desc.layers[0].object_index[0] = 0;
    desc.layers[0].offset[0] = 0;
    desc.layers[0].pitch[0] = dumb.pitch;

    struct combo {
        unsigned rt;
        unsigned fourcc;
        uint32_t flags;
        const char *name;
    } combos[] = {
        {VA_RT_FORMAT_RGB32, VA_FOURCC_BGRX, 0, "RGB32/BGRX/flags0"},
        {VA_RT_FORMAT_RGB32, VA_FOURCC_BGRA, 0, "RGB32/BGRA/flags0"},
        {VA_RT_FORMAT_RGB32, VA_FOURCC_BGRX, VA_SURFACE_ATTRIB_SETTABLE, "RGB32/BGRX/SETTABLE"},
        {VA_RT_FORMAT_RGB32, VA_FOURCC_BGRA, VA_SURFACE_ATTRIB_SETTABLE, "RGB32/BGRA/SETTABLE"},
        {0, VA_FOURCC_BGRX, 0, "rt0/BGRX/flags0"},
        {VA_RT_FORMAT_YUV420, VA_FOURCC_BGRX, 0, "YUV420/BGRX/flags0"},
    };

    VASurfaceID surf = VA_INVALID_ID;
    int imported = 0;
    for (unsigned i = 0; i < sizeof(combos) / sizeof(combos[0]); i++) {
        desc.fourcc = combos[i].fourcc;
        VASurfaceAttrib attrs[2];
        memset(attrs, 0, sizeof(attrs));
        attrs[0].type = VASurfaceAttribMemoryType;
        attrs[0].flags = combos[i].flags;
        attrs[0].value.type = VAGenericValueTypeInteger;
        attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
        attrs[1].type = VASurfaceAttribExternalBufferDescriptor;
        attrs[1].flags = combos[i].flags;
        attrs[1].value.type = VAGenericValueTypePointer;
        attrs[1].value.value.p = &desc;

        VASurfaceID s = VA_INVALID_ID;
        VAStatus cs = vaCreateSurfaces(dpy, combos[i].rt, W, H, &s, 1, attrs, 2);
        printf("import %-22s -> %s (surf=%u)\n", combos[i].name, vaErrorStr(cs), s);
        if (cs == VA_STATUS_SUCCESS) {
            surf = s;
            imported = 1;
            /* подтверждение, что поверхность живая: sync */
            VAStatus sy = vaSyncSurface(dpy, surf);
            printf("vaSyncSurface -> %s\n", vaErrorStr(sy));
            if (sy != VA_STATUS_SUCCESS) imported = 0;
            break;
        }
    }

    if (imported) printf("P1 IMPORT: PASS surf=%u\n", surf);
    else printf("P1 IMPORT: FAIL все комбинации отклонены\n");

    if (surf != VA_INVALID_ID) vaDestroySurfaces(dpy, &surf, 1);
    vaTerminate(dpy);
    close(drm);
    close(dmabuf);
    close(card);
    return imported ? 0 : 1;
}
