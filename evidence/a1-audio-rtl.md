# A1: аудио-тракт на планшет (opus/RTP)

Дата: 2026-10-19. Задача: «звук бежит вперёд картинки» → оказалось, звука на
планшет вообще нет: SDP без `m=audio`, ffmpeg без аудио-входа, :49002 только
приёмный. Внедрён аудио-конвейер по эталону Sunshine.

## Что сделано

- `src/stream/audio.rs`: PulseAudio-монитор дефолтного sink'а → libopus
  (48 кГц, стерео, CBR 96k, 5-мс фреймы, application=2050 restricted_lowdelay)
  → RTP на локальный UDP → форвард-поток клиенту. Пакет как у Sunshine
  (docs/research/stream.cpp:1874): header 0x80, PT=97, ssrc=0.
- Пир аудио учим из SS_PING клиента на :49002 (peer-learning как у видео).
- SDP DESCRIBE: `m=audio 0 RTP/AVP 97` + `a=rtpmap:97 opus/48000/2` +
  `a=fmtp:97 surround-params=21101`.
- ANNOUNCE: `x-nv-audio.surround.numChannels` (0 = клиент без звука) и
  `x-nv-aqos.packetDuration` (дефолт 5 мс).
- Синхронизация: `NX_AUDIO_DELAY_MS=80` (adelay) — видео-путь ~100 мс,
  аудио-путь ~20 мс.
- `audio_child` в PipelineHandle: stop() конвейера убивает и звук; wait() —
  против зомби.
- mDNS фикс (попутная поломка при рестарте): на wlan0 два IPv4 (.180 +
  secondary .181) → второй join_multicast_v4 по тому же интерфейсу = EADDRINUSE
  и смерть сервера. Теперь join по одному адресу на интерфейс.

## Найденные ловушки (проверено руками)

1. `-application restricted_lowdelay` эта версия ffmpeg не парсит
   («Undefined constant») — только число `2050`.
2. `prctl(PR_SET_PDEATHSIG)` умирает вместе с СОЗДАВШИМ ПОТОКОМ, а spawn шёл
   из RTSP-потока-обработчика → ffmpeg получал SIGKILL сразу после старта
   (лог: «audio: ffmpeg звука завершился», stderr пустой). Фикс: спавн в
   долгоживущем супервизор-потоке (audio-sup).

## Замер (живая сессия, планшет Moonlight)

```
audio: старт (pulse 'alsa_output.pci-0000_00_09.2.analog-stereo.monitor' → opus 96k/5ms → rtp://127.0.0.1:40192)
audio: peer 192.168.178.89:46969 (20B пинг)
audio: 186.6 pkt/s, 107 kbps (peer 192.168.178.89:46969, 5 мс/фрейм)
audio: 200.5 pkt/s, 115 kbps (peer 192.168.178.89:46969, 5 мс/фрейм)
audio: 199.4 pkt/s, 115 kbps (peer 192.168.178.89:46969, 5 мс/фрейм)
```

200 pkt/s = 1000 мс / 5 мс — ровно столько фреймов шлёт encoder, потерь на
форварде нет. 115 kbps CBR (96k payload + RTP/UDP/IP).

Видео при этом не просело: capture 60.0-60.1 fps, sync 3-6 мс (writer-лог).

## Ожидается от владельца

Послушать: звук идёт с планшета, синхрон с картинкой. Если звук отстаёт/
упреждает — крутить NX_AUDIO_DELAY_MS в launch_nx.sh (сейчас 80).
