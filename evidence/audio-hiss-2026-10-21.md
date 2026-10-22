# Шипение в аудио: разбор и починка

Дата: 2026-10-21 (03:00). Хост: archpizdetc, Arch, 4 ядра, AMD Carrizo APU,
Hyprland, PipeWire 1.6 + WirePlumber, мониторы eDP-1 (экран хоста) и
HEADLESS-1 1088x800 (стрим). Клиент: планшет GSI, com.limelight 12.2 (315).

Жалоба: «аудио все ещё шипит».

## Итог в одной строке

Шипение давали **четыре** независимые причины, все на нашей стороне:
недобор аудио-темпа (3–12%), отсутствие FEC (клиент не переживал потери на
USB-канале), «дышащий» виртуальный sink (когда он в IDLE, monitor отдаёт
тишину рывками) и форвардер, который сам резал темп до 193 pkt/s.

После правок: **200 data + 100 FEC пакетов/с, 0 пропусков на проводе,
61 fps видео**, клиент перестал терять блоки (было 15 «Unable to recover
audio data block» в минуту — стало 1 за 19 минут).

---

## 1. Как вообще выглядит «шипение» у клиента

Клиент (moonlight-common-c) собирает аудио в FEC-блоки по 4 пакета
(`RTPA_DATA_SHARDS`). Если блок не собрался — он вставляет «пустышку» и
делает PLC (packet loss concealment). Логи клиента:

```
moonlight-common-c: Unable to recover audio data block 38396 to 38399 (2+0=2 received < 4 needed)
moonlight-common-c: Network dropped audio data (expected 38397, but received 38399)
```

За сессию до починки: **445** «Unable to recover» и **361** «Network dropped».
Плюс «Audio packet queue overflow» — клиент сбрасывал очередь целиком.

## 2. Причина №1: темп аудио не 200 pkt/s, а 118–193

Клиент ждёт 48 кГц × 5 мс = 240 сэмплов на пакет, 200 пакетов в секунду.
Меньше 200 → у клиента AudioTrack пустеет → щелчки/шипение.

Замерялось тремя способами (все сходились):

| участок | темп |
|---|---|
| ffmpeg → форвардер (lo) | 195.3 pkt/s, ts/s 48054 |
| форвардер → планшет (USB) | 192.9 pkt/s, **пропусков seq 18–31 за 11 с** |
| клиентский счётчик | 193 pkt/s |

Причины недобора:

**(а) `adelay` в ffmpeg при nice 15.** Замер по 10 с на каждый вариант:

| вариант | pkt/s | ts/s |
|---|---|---|
| adelay + nice 15 (было в проде) | **192.9** | 46429 |
| adelay + nice 0 | 201.1 | 48757 |
| без adelay + nice 15 | 199.3 | 48036 |
| без adelay + nice 0 | 198.6 | 48067 |

Фильтр `adelay` поднимает лишний поток, и при nice 15 энкодер аудио не
выбирается планировщиком — конвейер физически замедляется. Задержку перенесли
из ffmpeg в наш форвардер (очередь с `send_at = now + NX_AUDIO_DELAY_MS`).

**(б) фиксированный sleep(5 мс) в форвардере.** Цикл «sleep 5 мс + приём +
обработка» давал 5.2 мс на пакет = 193 pkt/s. Паейсинг убран: темп теперь
задаёт источник.

**(в) `nx_stream` в состоянии IDLE.** Когда в виртуальный sink ничего не
играет, он проваливается в IDLE, и его monitor отдаёт тишину рывками:
**118 pkt/s** вместо 200 (длинные паузы >50 мс в гистограмме интервалов).
Решение — keepalive: `module-null-source` → `module-loopback` → `nx_stream`.
Sink всегда RUNNING, monitor идёт ровно.

**(г) rtkit не был запущен.** Потоки PipeWire были SCHED_OTHER
(`data-loop.0` → TS), при load 13–15 на 4 ядрах аудио-граф отставал по
часам (замер: monitor отдаёт 88–95 % реального времени). После
`systemctl enable --now rtkit-daemon.service` и рестарта pipewire
`data-loop.0` стал `RR 20` (реальное время).

Итог: **200 data-пакетов/с** (было 118–193).

## 3. Причина №2: FEC не отправлялся

Клиент ждёт блоки 4 data + 2 parity. Мы шли только data → блок не
восстанавливался → PLC → щелчки. Потери были на USB-канале (RNDIS): в
замере 11 с на проводе пропадало 18–31 пакет, счётчик интерфейса показывал
0 дропов (теряла сторона планшета).

Сделано по образцу Sunshine (`docs/research/stream.cpp:1874-1960`):

* матрица паритета RS(6,4) — те же байты, что клиент подставляет в
  библиотеку nanors (`RtpAudioQueue.c`: `0x77,0x40,0x38,0x0e,0xc7,0xa7,0x0d,0x6c`),
  `parity[k] = Σ_j M[k][j]·data[j]` над GF(256)/0x11d;
* паритет считается над **зашифрованными** шардами (payload без 12-байтного
  RTP-заголовка) — как в Sunshine и как ждёт клиент (`shards[i] = dataPackets[i] + 1`);
* FEC-пакет: RTP (PT=127) + `AUDIO_FEC_HEADER` (fecShardIndex, payloadType,
  baseSequenceNumber, baseTimestamp, ssrc — многобайтовые в big endian) + паритет;
* база блока = `(seq/4)*4`, `baseTs` = ts пакета с базовым seq.

Проверено тестом `rs_recovers_two_lost_shards`: потерянные шарды 1 и 2
восстанавливаются из паритета (решение системы 2×2 по формулам Крамера).

Результат у клиента: «Unable to recover audio data block» — **1 за 19 минут**
(было 15 в минуту).

## 4. Причина №3: RTP timestamp не по порядку

Клиент считает базу FEC-блока как `baseTs = packet.ts - (seq - blockBase) * 240`
и восстанавливает ts = `baseTs + i*240`. Значит ts обязан расти ровно на 240
за каждый +1 seq. Раньше форвардер считал ts «по счётчику пакетов», а ffmpeg
выдавал дельты 240–249 (дефект его muxer'а). Теперь ts выводится из
sequence number: `ts = ts0 + (seq - seq0) * 240`.

## 5. Причина №4: роутер аудио смотрел на несуществующий монитор

`scripts/nx_audio_router.py` держал `STREAM_MONITOR = "HEADLESS-4"`, а
nx-server захватывает первый существующий HEADLESS-* (сейчас HEADLESS-1).
Имя headless-вывода меняется при пересоздании (было HEADLESS-4 → стало
HEADLESS-1), после чего роутер молча переставал переносить звук в стрим:
игра уходила в динамики хоста, на планшет шла тишина. Роутер теперь ищет
первый HEADLESS-* так же, как сервер (src/main.rs).

## 6. Что ещё починено по ходу

* `rtkit-daemon.service` — включён и запущен (PipeWire получает реальное время).
* `nx_stream` + keepalive создаются в `launch_nx.sh` и восстанавливаются
  watchdog'ом (`ensure_audio_sink()`, раз в 30 с). Модуль null-sink живёт в
  процессе pipewire, поэтому после его рестарта sink'а просто не было, а
  ffmpeg-аудио умирало с «Error during demuxing».
* watchdog перезапускает сервер, если клиент на связи, а процесса
  `ffmpeg -f pulse` нет (видео идёт, звука нет).
* Баг в самом watchdog: проверка keepalive сравнивала **индекс** sink'а с
  **именем** (в short-выводе pactl второе поле — индекс), проверка не
  срабатывала никогда, и keepalive плодился каждые 30 с (набралось 20
  копий) — исправлено, лишние выгружены.

## 7. Как проверить

```bash
# темп и FEC в логе сервера
grep -a "^audio: отправлено" /tmp/nx-server.log | tail -2
#   ожидается ~300 pkt/s (200 data + 100 FEC), «FEC-паритет: ~1000» за 10 с

# потери на проводе (должно быть 0)
sudo tcpdump -i enp0s16u3 -nn -s 0 -w /tmp/a.pcap "udp port 49002"
tshark -r /tmp/a.pcap -d udp.port==49002,rtp -Y "udp.port==49002 && rtp" \
  -T fields -e rtp.p_type -e rtp.seq | sort | uniq -c

# жалобы клиента
adb shell "logcat -d | grep -c 'Unable to recover audio data block'"
adb shell "logcat -d | grep -c 'Network dropped audio data'"
```
