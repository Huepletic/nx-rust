# Корень лага ~1.1с: CPU-голодание хоста (runaway wireplumber)

Дата: 2026-10-16 01:15-01:31 MSK. Хост: 4 ядра, Hyprland 0.56.2, планшет Ri0T1100PK0008473 (Moonlight 12.2) по USB RNDIS.

## Симптомы до
- Сквозная задержка видео ~1.06-1.17с (6 проб по часам nxclock на HEADLESS-4), ввод ~1.1с.
- Захват ~3fps при target 60; ffmpeg молча умирал (зомби, пустой /tmp/nx-ffmpeg.log);
  сессии Moonlight падали «network isn't performing well».

## Диагностика
1. `nx-server copybench HEADLESS-4` (новый probe, изолирует compositor от ffmpeg/RTP):
   copy-ready med 76-97мс на HEADLESS-4 (2176x1600) и med 80мс на eDP-1 (1920x1080) —
   НЕ пропорционально байтам, а фиксированное ожидание → compositor не успевает.
2. `/proc/pressure/cpu`: some avg10≈50-60 (постоянное CPU-голодание).
   `vmstat`: id 1-5%, us+sy 85-98%. Load average 10-12 на 4 ядра.
3. `top`: wireplumber pid 2596 (orphan, PPID=1, /bin/wireplumber) — 349:19 CPU-минут
   за 8.5ч работы (~68% ядра впустую) + pid 671 (service-инстанс) — 6:44 CPU в D/R.
   Три экземпляра opencode (~140% суммарно) + Hyprland 35% — остальное добивало CPU.

## Лечение
- `kill -9 2596` (orphan) + `systemctl --user restart wireplumber` → новый 497096, 4% CPU.
- CPU idle 11% → 34%. PSI cpu some avg10 49.6 → ~40.
- copybench после: HEADLESS-4(720p конфиг юзера) copy med 35мс (min 19, max 46).
- Сервер перезапущен с NX_ENC_EXTRA="-bufsize 1000k" (был 5000k = 0.5с VBV-окно).

## Замеры после (живые, часы nxclock: прямой захват вывода vs screencap планшета)
| Режим | FPS (stats Moonlight) | decode | сеть | лаг |
|---|---|---|---|---|
| 1280x720 @10M | 31.07 | 66мс | 1мс | ~0.3с |
| Native 2176x1600 @37M | 12.77 | 156мс | 1мс | ~1.0с |

Проба 01:31:02: прямой захват HEADLESS-4 = 01:31:02.57 (свежий), экран планшета =
01:31:02.26 при snapshot ≈01:31:03.2 → ~1.0с. 0% dropped frames, network latency 1мс.

## Вывод
- Корень лага — НЕ сеть (USB 1мс) и НЕ инъекция ввода, а CPU-голодание хоста:
  screencopy и конвейер не получали CPU, ffmpeg умирал, энкод-очередь копилась.
- Остаточный потолок при Native — на стороне планшета: Moonlight использует
  СОФТОВЫЙ декодер c2.android.avc.decoder (железного avc не нашёл) → 156мс/кадр
  на 2176x1600 → 13fps и ~1с. На 720p тот же декодер: 66мс → 31fps, лаг ~0.3с.
- Для отзывчивого тачпада: разрешение 1280x720 или 1920x1080 в настройках Moonlight.
