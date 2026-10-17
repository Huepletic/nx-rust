# Разрешение стрима и железный декодер: итоги (2026-10-16)

Продолжение [`2026-10-16-lag-root-cause.md`](2026-10-16-lag-root-cause.md).
Тот отчёт закрыл корень лага (CPU-голодание, runaway wireplumber) и дал 720p@31fps/~0.3с.
Здесь — доводка: выбор разрешения и почему аппаратный путь на планшете закрыт.

## Итоговая рабочая конфигурация

| Что | Значение |
|---|---|
| Виртуальный монитор | `HEADLESS-4` = **1088x800@60**, scale 1 (`~/.config/hypr/monitors.lua`) |
| Клиент Moonlight 12.2 | `list_resolution=1088x800`, `frame_pacing=latency`, `list_fps=60`, `checkbox_enable_perf_overlay=true` |
| Транспорт | USB RNDIS, ping 0.5-0.7мс |
| Хост-адрес у клиента | `192.168.130.1:48989` (порты 48984 HTTPS / 48989 HTTP заняты Sunshine на 47984/47989) |

### Почему 1088×800, а не 720p и не натив

Панель планшета — 2176×1600, соотношение **1.36:1**. Из него:
- **720p (1280×720, 16:9)** → чёрные полосы сверху/снизу (не full-bleed).
- **натив 2176×1600** → резко и full-bleed, но софт-декодер не тянет (см. ниже).
- **1088×800 = ровно 2176/2 × 1600/2** → то же соотношение, что у панели: full-bleed,
  на планшете масштаб ровно **2×** (целочисленный → без искажений и мыла интерполяции),
  а по числу пикселей (0.87 Мпикс) — практически как 720p (0.92 Мпикс),
  т.е. столько же стоит декодеру.

### Замеры (живой поток, оверлей Moonlight + часы nxclock grim/screencap)

| Режим | FPS | decode | сеть | dropped | лаг (6 проб) |
|---|---|---|---|---|---|
| 1280x720 | 31.97 | 33.18мс | 1мс | 0.00% | ~0.19-0.22с |
| **1088x800** | **34.7-37.7** | **22.9-31.9мс** | 1мс | 0.00% | **~0.08-0.17с** |
| натив 2176x1600 | 12.77 | 156мс | 1мс | 0.00% | ~1.0с (дрейфует) |

Пруфы: `latency/tablet_final.png` (оверлей + full-bleed, окна AyuGram), `latency/tablet_1088x800_overlay.png`, `latency/host_HEADLESS-4.png`.

## Почему не аппаратный декодер (MTK)

Планшет: mt6771 (Helio P60), Mali-G72, GSI **PHH Treble** (`me.phh.treble.app`), Android 11+, Magisk.
Moonlight 12.2 в `MediaCodecHelper.isCodecBlacklisted` глушит **все** `OMX.MTK.*` и `c2.mtk.*`.
Блэклист не «на всякий случай» — он обоснован.

Проверено прямым экспериментом (правка smali → apktool b → zipalign → apksigner → install):
1. В `MediaCodecDecoderRenderer.smali` профиль AVC `const/16 v1, 0x8` (AVCProfileHigh) → `-0x1`.
2. `isCodecBlacklisted` → всегда `false`.
3. Результат: декодер **выбрался** — `Selected AVC decoder: OMX.MTK.VIDEO.DECODER.AVC`,
   формат с `vdec-lowlatency=1`, `qualified=false`.
4. **Но поток не пошёл**: чёрный экран, «Slow connection to PC / Reduce your bitrate»,
   непрерывные IDR-запросы. В logcat:
   - `BufferQueueProducer [SurfaceView - com.limelight/com.limelight.Game#0] dequeueBuffer: createGraphicBuffer failed`
   - `ACodec: dequeueBuffer failed: NO_MEMORY(-12)`
   - `BWC: Failed to switch SMI/EMI profile`
   - `MTK_IOC_SMI_BWC_INFO_SET failed: Inappropriate ioctl for device`

Диагноз: на **GSI** vendor-часть gralloc/BWC (MediaTek bandwidth controller) не работает —
нативные графические буферы под MTK-декодер не создаются. RAM при этом не при чём
(3.8ГБ, ~900МБ свободно), проявляется и на 1088×800, и на нативе.

Вывод: **HW-декод на этом устройстве недоступен**, пока стоит GSI (или пока не починят vendor gralloc).
Оригинальный Moonlight переустановлен (APK-патч откатан); патченые сборки остались в
`/tmp/opencode/apk/` (base.apk, patched2.apk, decoded/, moonkey.keystore) как след эксперимента.

## Почему не HEVC (вдвое меньше битрейта при том же качестве → легче декодеру)

- AMD Wani (GCN3) в VAAPI: `VAProfileHEVCMain: VAEntrypointVLD` — есть только **декод**, энкода нет.
  → сервер не может кодировать HEVC аппаратно, только софт (слишком дорого).
- Со стороны клиента HEVC вайтлист Moonlight покрывает Amazon/PowerVR, а тут **Mali-G72** →
  `omx.mtk` под HEVC не разблокируется. Итого обе стороны закрыты.

## Где остался потолок задержки (~100мс снизу)

Разложение ~0.13с при 34fps (≈29мс/кадр → ~4-5 кадров конвейера):
1. **Захват screencopy** ~28-35мс/кадр — GPU→CPU readback композитора (shm screencopy).
   ГЛАВНЫЙ оставшийся резерв: переход на **dmabuf screencopy** (zero-copy, без readback)
   сократил бы ~30мс. Требует переработки `stream/pipeline.rs` (импорт dmabuf в VAAPI) — не сделано.
2. **Энкод VAAPI** — async_depth=1, GOP 30, CBR (`NX_ENC_EXTRA`).
3. **Софт-декод на планшете** 23-32мс — непреодолимо без HW-декодера.
4. **Вывод** vsync/SurfaceFlinger ~16мс.

Сеть (1мс) и инъекция ввода — не узкое место.

## Что дальше (если нужна ещё меньше задержка)
1. **dmabuf-захват** вместо shm (≈ -30мс) — самый крупный реальный выигрыш.
2. Понизить разрешение ещё ниже (напр. 816×600) — но ценой читаемости.
3. Попробовать другой ром/ядро планшета с рабочим vendor gralloc — тогда вернётся HW AVC-декод.

## Приложения: правки к прошлому отчёту
- `/tmp/nx-clock.sh` использует `date +%H:%M:%S.%2N` (сантисекунды, обновление 20мс).
- Замер лага: `sh /tmp/measure_lat.sh` (grim HEADLESS-4 + adb screencap планшета одновременно,
  затем DS Vision читает оба времени; точность ~±0.05-0.1с).
- Перезапуск сервера сбрасывает список спаренных клиентов (in-memory) → после рестарта
  нужно перепаривать: сервер `NX_PIN_WAIT=1`, в клиенте тапнуть плитку, поймать показанный PIN
  и записать его в `/tmp/nx-pin` в течение 120с.
