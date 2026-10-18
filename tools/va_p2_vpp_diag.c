/* P2-диагностика: локализация краша vaBeginPicture (SEGSEGV в radeonsi→libXau).
 *
 * Три последовательных опыта — каждый печатает свой маркер ДО и ПОСЛЕ:
 *   E1: BeginPicture/EndPicture с конфигом H264 EncSlice на NV12-поверхности
 *       (без буферов вообще) — общий ли крах или только VPP.
 *   E2: VPP-конфиг, BeginPicture/EndPicture с PLAIN источником (vaCreateSurfaces
 *       без импорта) + полный пайплайн — крахится ли сам VPP-путь без dmabuf.
 *   E3: VPP c ИМПОРТИРОВАННЫМ dmabuf-источником (текущее поведение P2).
 *
 * Вывод по E1/E2/E3 локализует: драйвер-баг VPP вообще / импорт ломает
 * состояние / последовательность вызовов пробника отличается от ffmpeg.
 *
 * Сборка: gcc -O0 -g -Wall -o /tmp/va_p2c tools/va_p2_vpp_diag.c $(pkg-config --cflags --libs libva libva-drm) -lm
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

/* Учтённый краш-баг (см. evidence/zc-p2-vpp.md): Begin/Render/End/CreateBuffer
 * требуют VAContextID (vaCreateContext), не VAConfigID. */
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
    if (map != MAP_FAILED) { memset(map, 0x40, sz); munmap(map, sz); }

    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    VADisplay dpy = vaGetDisplayDRM(drm);
    int maj, min;
    vaInitialize(dpy, &maj, &min);

    VASurfaceID nv = VA_INVALID_ID;
    VAStatus st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, &nv, 1, NULL, 0);
    printf("dst NV12 create: %s\n", vaErrorStr(st));

    /* E1: H264 EncSlice config + Begin/End без буферов */
    printf("E1: begin h264...\n");
    VAConfigID hcfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileH264Main, VAEntrypointEncSlice, NULL, 0, &hcfg);
    printf("E1: config %s cfg=%u\n", vaErrorStr(st), hcfg);
    VAContextID hctx = VA_INVALID_ID;
    st = vaCreateContext(dpy, hcfg, W, H, 0x1, NULL, 0, &hctx);
    printf("E1: context %s ctx=%u\n", vaErrorStr(st), hctx);
    st = vaBeginPicture(dpy, hctx, nv);
    printf("E1: BeginPicture %s\n", vaErrorStr(st));
    /* End без буферов = SIGSEGV в radeonsi (факт e34) — пропускаем */
    printf("E1: EndPicture пропущен (End без буферов = SIGSEGV, radeonsi edge)\n");

    /* E2: PLAIN RGB источник (без импорта) → полный VPP */
    printf("E2: plain-src vpp...\n");
    VASurfaceID plain = VA_INVALID_ID;
    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, W, H, &plain, 1, NULL, 0);
    printf("E2: plain RGB create %s surf=%u\n", vaErrorStr(st), plain);
    VAConfigID vcfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileNone, VAEntrypointVideoProc, NULL, 0, &vcfg);
    printf("E2: vpp config %s cfg=%u\n", vaErrorStr(st), vcfg);
    VAContextID vctx = VA_INVALID_ID;
    st = vaCreateContext(dpy, vcfg, W, H, 0x1, NULL, 0, &vctx);
    printf("E2: vpp context %s ctx=%u\n", vaErrorStr(st), vctx);
    int rc = run_vpp(dpy, vctx, plain, nv);
    printf("E2: run_vpp(plain) = %d\n", rc);

    /* E3: ИМПОРТИРОВАННЫЙ dmabuf-источник → VPP */
    printf("E3: import-src vpp...\n");
    VADRMPRIMESurfaceDescriptor desc;
    memset(&desc, 0, sizeof(desc));
    desc.fourcc = VA_FOURCC_BGRX; desc.width = W; desc.height = H;
    desc.num_objects = 1;
    desc.objects[0].fd = dmabuf; desc.objects[0].size = (uint32_t)sz;
    desc.num_layers = 1;
    desc.layers[0].drm_format = DRM_FORMAT_XRGB8888; desc.layers[0].num_planes = 1;
    desc.layers[0].pitch[0] = dumb.pitch;
    VASurfaceAttrib attrs[2];
    memset(attrs, 0, sizeof(attrs));
    attrs[0].type = VASurfaceAttribMemoryType;
    attrs[0].value.type = VAGenericValueTypeInteger;
    attrs[0].value.value.i = VA_SURFACE_ATTRIB_MEM_TYPE_DRM_PRIME_2;
    attrs[1].type = VASurfaceAttribExternalBufferDescriptor;
    attrs[1].value.type = VAGenericValueTypePointer;
    attrs[1].value.value.p = &desc;
    VASurfaceID imp = VA_INVALID_ID;
    st = vaCreateSurfaces(dpy, VA_RT_FORMAT_RGB32, W, H, &imp, 1, attrs, 2);
    printf("E3: import %s surf=%u\n", vaErrorStr(st), imp);
    rc = run_vpp(dpy, vctx, imp, nv);
    printf("E3: run_vpp(import) = %d\n", rc);

    printf("DIAG: все опыта прошли без краша\n");
    return 0;
}
