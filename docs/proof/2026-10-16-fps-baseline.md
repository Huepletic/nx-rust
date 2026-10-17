# Baseline параметров стрима — 2026-10-16 (до работы над 60 FPS)

Цель зафиксировать ВСЕ текущие параметры системы до попытки поднять FPS с ~35 до 60
без их изменения.

## Видеорежим (экранный сервер)

| Параметр | Значение | Источник |
|---|---|---|
| Физический режим HEADLESS-4 | `1088x800@60` | `~/.config/hypr/monitors.lua` |
| Масштаб | `0.8` | `monitors.lua` |
| Логический размер | `1360x1000` | `hyprctl monitors` |
| Позиция | `0x900` | `monitors.lua` |
| Refresh | `60.0` | `hyprctl -j monitors` |
| Панель планшета | `2176x1600` ландшафт | Moonlight |
| Отображение на планшете | full-bleed ×1.6 (логика) / ×2 (физика) | — |

## Параметры потока (RTSP ANNOUNCE от клиента)

| Параметр | Значение |
|---|---|
| fps (запрошен) | `60` |
| viewport | `1088x800` |
| bitrate | `9000k` |
| packetSize | `1392` |
| fec_min | `2` |
| reliableUdp | `13` |
| encryption | `1` (CONTROL_V2, AES-128-GCM) |

## Энкодер (ffmpeg, `src/stream/pipeline.rs`)

```
-y -loglevel error
-f rawvideo -pixel_format bgra -video_size 1088x800 -framerate 60 -i pipe:0
-vaapi_device /dev/dri/renderD128
-vf hwupload,scale_vaapi=format=nv12
-c:v h264_vaapi
-rc_mode CBR -b:v 9000k -maxrate 9000k -bufsize 4500k
-async_depth 1 -g 30 -bf 0
-f h264 pipe:1
```

## Замеры производительности (эталон)

### Сторона сервера

| Метрика | Значение | Метод |
|---|---|---|
| capture FPS | **33–36** | счётчик `capture: N кадров` за 60 с |
| rtp FPS | **35** | счётчик `rtp: N кадров` за 60 с |
| Цикл capture_loop | **27.9–28.3 мс** | инструментированный тайминг в `probes.rs` |

Разбивка цикла `capture_loop` (мс/кадр, среднее):

| Стадия | с ffmpeg (стрим) | без ffmpeg (capturebench) |
|---|---|---|
| params (ждём буфер) | 1.0–1.2 | 0.5–0.7 |
| alloc | 0.004 | 0.004 |
| **copy/ready** | **19.3–20.3** | **17.3–18.5** |
| **sink (запись в stdin)** | **7.2** | **0.01** |
| всего | 27.6–28.9 → **35 fps** | 17.8–19.2 → **52–56 fps** |

### Сторона клиента (Moonlight perf overlay)

| Метрика | Значение |
|---|---|
| Video stream | 1088x800, **36.73 FPS** |
| Decoder | `c2.android.avc.decoder` |
| Incoming frame rate | **36.73 FPS** |
| Rendering frame rate | **36.73 FPS** |
| Frames dropped by network | **0.00%** |
| Average network latency | **1 ms** (variance 0 ms) |
| Average decoding time | **59.12 ms** |

`incoming == render` и `0.00% dropped` → бутылочное горлышко на сервере, клиент всё успевает.

### Ресурсы

| Ресурс | Значение |
|---|---|
| GPU busy, без стрима | **61–64 %** |
| GPU busy, со стримом | **79–82 %** |
| ffmpeg CPU | 25–27 % |
| Hyprland CPU | ~50 % |
| RAM | 4271 used / 7345 total, swap 1794 used |

## Изолированные тесты энкодера (300 кадров, `h264_vaapi`, те же параметры)

| Источник | FPS |
|---|---|
| `testsrc2` (движение) | **148** |
| статичный реальный кадр (loop) | **78** |
| `mandelbrot` | 10.7 (упирается в генерацию источника, 100 % CPU — не показателен) |

Вывод: `hwupload + scale_vaapi + h264_vaapi` сами по себе не ограничены на 35 FPS.

## Главный вывод из замеров A/B

Запуск ffmpeg меняет три величины одновременно:

1. `sink` растёт с **0.01 → 7.2 мс** — `stdin.write_all` блокирует capture-поток, пока
   ffmpeg не прочитает кадр (3.48 МБ на кадр при BGRA 1088x800).
2. `copy/ready` растёт с **17.3 → 19.9 мс** — GPU-конкуренция VAAPI-энкодера с
   compositor-ом и захватом.
3. Итог: 18.5 мс (52–56 fps) → 28.3 мс (**35 fps**).

Все три стадии сейчас идут **последовательно в одном потоке**
(`pipeline.rs: capture_loop → stdin.write_all`), поэтому медленная стадия останавливает все.
