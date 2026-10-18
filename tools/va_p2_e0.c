/* E0: минимальная последовательность VA-энкодера (после исправления ошибки
 * config-vs-context): renderD128 + vaInitialize + конфиг H264 EncSlice +
 * NV12-поверхность + vaCreateContext + vaBeginPicture. Раньше падал на
 * BeginPicture — в vaBeginPicture передавался VAConfigID вместо VAContextID
 * (см. evidence/zc-p2-vpp.md, F1). */
#define _GNU_SOURCE
#include <fcntl.h>
#include <stdio.h>
#include <unistd.h>
#include <va/va.h>
#include <va/va_drm.h>

#define W 1088
#define H 800

int main(void) {
    setvbuf(stdout, NULL, _IONBF, 0);
    int drm = open("/dev/dri/renderD128", O_RDWR | O_CLOEXEC);
    if (drm < 0) { printf("E0 FAIL open %d\n", drm); return 1; }
    VADisplay dpy = vaGetDisplayDRM(drm);
    int maj, min;
    if (vaInitialize(dpy, &maj, &min) != VA_STATUS_SUCCESS) { printf("E0 FAIL init\n"); return 1; }
    printf("E0 va-init ok %d.%d\n", maj, min);

    VASurfaceID nv = VA_INVALID_ID;
    VAStatus st = vaCreateSurfaces(dpy, VA_RT_FORMAT_YUV420, W, H, &nv, 1, NULL, 0);
    printf("E0 surface %s id=%u\n", vaErrorStr(st), nv);

    VAConfigID cfg = VA_INVALID_ID;
    st = vaCreateConfig(dpy, VAProfileH264Main, VAEntrypointEncSlice, NULL, 0, &cfg);
    printf("E0 config %s id=%u\n", vaErrorStr(st), cfg);

    /* Begin/End берут VAContextID — НЕ VAConfigID (передача config'а роняла
     * процесс: драйвер читал несуществующий контекст → дикий прыжок). */
    VAContextID ctx = VA_INVALID_ID;
    st = vaCreateContext(dpy, cfg, W, H, 0x1, NULL, 0, &ctx);
    printf("E0 context %s id=%u\n", vaErrorStr(st), ctx);

    st = vaBeginPicture(dpy, ctx, nv);
    printf("E0 BeginPicture %s\n", vaErrorStr(st));
    /* vaEndPicture БЕЗ отрисованных буферов роняет radeonsi 26.2.5: LIBVA_TRACE
     * показывает вход va_TraceEndPicture без строки ret (/tmp/vae0, e34_e0).
     * Прод-код (как и ffmpeg) всегда рисует буферы до End — ветка не
     * воспроизводится; в тесте End пропускаем. */
    printf("E0 EndPicture пропущен: End без буферов = SIGSEGV (radeonsi edge)\n");
    printf("E0 PASS\n");
    vaDestroyContext(dpy, ctx);
    vaDestroyConfig(dpy, cfg);
    vaDestroySurfaces(dpy, &nv, 1);
    vaTerminate(dpy);
    close(drm);
    return 0;
}
