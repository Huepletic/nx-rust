# Z4a: собственный VA-API H.264-кодировщик на Rust — PASS

Дата: 2026-10-18. Артефакт: `nx-rust/src/vaapi.rs` (~1250 строк), standalone-пробник `nx-server vaprobe N`.

## Что сделано

Порт проверенного C-пробника `tools/va_p3_enc.c` (P3 PASS, чекпоинт #18) в Rust:
- FFI extern "C" libva (20 функций) + libva-drm (`vaGetDisplayDRM` — только там, `nm -D /usr/lib/libva-drm.so.2`).
- repr(C)-структуры с compile-time size/offset-ассертами против C-эталона:
  seq=1132, pic=648, slc=3140, packed_param=28, coded_seg=48, picH264=36, prime_desc=312, generic_value=16, surface_attrib=24; offset-проверки: seq_fields=28, vui_fields=1092, sar_width=1100, num_units_in_tick=1108, coded_buf=612, pic_fields=628, cabac_init_idc=3118, buf=16.
- Rust-бит-писатель (UE/SE/trailing), EBSP-эскейп, NAL/SPS/PPS/slice-генераторы.
- `VaEncoder`: dumb-dmabuf → PRIME_2 импорт → VPP sRGB→BT601 → EncSlice H.264 High/CABAC/CBR → Annex-B AU через `vaSyncBuffer` + VACodedBufferSegment-цепочка.
- Рецепт буферов идентичен P3: IDR=10 (seq+RC+HRD+FrameRate+pic+packedSEQ+packedSLC+slice+coded), P=4.

## Найденные и исправленные баги FFI-порта (каждый — против строки заголовка)

| Баг | Было → стало | Источник |
|---|---|---|
| sar_width/sar_height | u16 → **u32** (итог seq 1128→1132) | va_enc_h264.h:263-265 |
| vui_fields fixed_frame_rate | бит 3 → **бит 13** (3-12 = log2_max_mv_h/v по 5 бит) | va_enc_h264.h:238-256 |
| VA_PROFILE_NONE | 0 → **-1** (0 = MPEG2Simple) | va.h:504 |
| VA_RT_FORMAT_YUV420 | 0x10 → **0x1** (0x10 = greyscale) | va.h:1083 |
| VPP-константы | SRGB 4→**8**, BT601 2→**1** (4=BT470BG, 2=BT709) | va_vpp.h:307-377 |
| open("/dev/dri/card1") | `str::as_ptr()` без NUL → ENOENT → **CString** | проверено A/B rustc-пробником |
| DRM ioctl cmd | голый nr → **_IOWR(3<<30 \| size<<16 \| 'd'<<8 \| nr)** | tools/va_p3_enc.c:47-50 |

Проявление VPP-бага: Y-полосы декода [63,173,32,219,78,188,78,185] не совпадали с C-эталоном; после фикса — бит-в-бит.

## Гейты Z4a (все PASS)

1. **5 кадров, 0 VA-ошибок** (`/tmp/e40_z4a.txt`): IDR 600 B, P 577-662 B.
2. **90-кадровая серия GOP=30** (`/tmp/e44_z4a90.txt`): NAL-скан = **96 NAL: SPS=3, PPS=3, IDR=3, P-slices=87** — ровно 3 IDR-периода, DPB-пинг-понг устойчив.
3. **ffmpeg -v error exit=0**, stderr 0 байт (`/tmp/e45_decerr90.txt` — пуст).
4. **ffprobe nb_read_frames=90**, profile=High.
5. **Y-полосы кадра 0 бит-в-бит равны C-пробнику**: [85,149,41,214,108,171,78,185].

## Темп (грязный, под контендией)

90 кадров: avg 42.49 мс, p50=38.3, p95=108, p99=136, max=136.4 мс.
Условия: прод-ffmpeg (PID 2294863, CBR 20M, тот же renderD128) активен.
C-пробник в тех же условиях: 36.77 мс/кадр (evidence /tmp/e38_p3_90.txt) — Rust сопоставим (разница ~15% внутри шума контендии). Изолированный замер — на этапе Z4b-интеграции (прод нельзя останавливать).

## Сырые артефакты

- `evidence/z4a-run-raw.txt` — вывод 5- и 90-кадрового прогонов.
- `evidence/z4a-90.h264` — битстрим серии (90 кадров, 51642 B).
- `/tmp/e39_va_off.txt` — C-офсет-дамп; `/tmp/sz`, `/tmp/sz2.c` — контрольные размеры.
- `/tmp/e36_p3.txt` — эталонный C-прогон с vpp-frame0-y.

## Интеграция (Z4b) — следующий шаг

Точка врезки: `src/stream/pipeline.rs` — вместо spawn ffmpeg: `VaEncoder::new(slot_fd, w, h, pitch, bitrate, GOP, fps)` в потоке capture, `encode_frame()` → AU → существующий AuParser/RTP. Переключатель NX_ENCODER=vaapi|ffmpeg (дефолт ffmpeg). Watchdog-детектор `ps|grep ffmpeg` (watchdog.py:110-149,467-470) заменить на внутренний счётчик живости энкодера.

## Z4b: интеграция в pipeline (2026-10-19) — PASS

- `src/stream/pipeline.rs`: `NX_ENCODER=vaapi` → `start_vaapi()`: dmabuf-захват (форс), импорт fd слотов в VaEncoder (`import_slot(slot)`), encode в capture-потоке, Annex-B через os-пайп → существующий `rtp_sender_loop` (AU-парсер и RTP без изменений). Фоллбек на ffmpeg при любой ошибке старта vaapi.
- Скейл: VPP делает И конверсию, И скейл (vdst размера потока 1280x720/1088x800, surface_region/output_region NULL = full-surface).
- Watchdog: `stream_active()`/`live_ffmpeg_pid()` умеют heartbeat `/tmp/nx-vaapi-alive` (обновление 1с, возраст <5с = жив; sentinel pid=-1). Лог writer в формате "(всего N)" — last_writer_total работает без правок.
- Дедлок: 4 раздельных `st.launch.lock()` при построении launch_s после spawn capture-потока → заменены на один lock+clone. Симптом: capture писал в пайп, RTP-поток не спавнился, ANNOUNCE-ответ не уходил.
- stream_sim: **ALL PASS** (RTP кадр 0 = 21 чанк, NAL [7,8,5]).
- Живой планшет (Moonlight, adb-автозапуск PcView → nx-host → Desktop): стрим идёт, картинка живая (скрин 2176x1600, 54 градаций яркости), ввод идёт (input-фреймы в логе).
- Устойчивые метрики под load: capture 59.8 fps, sink(VPP+encode+pipe) 6.66 мс/кадр, rtp 1.4 Мбит/с на статике (у ffmpeg на статике 1.0–1.5 — паритет).
- Debug-принты убраны, release пересобран (вступит при следующем рестарте; текущий стрим не рвался).

## Z4c (2026-10-19, ночь): конвейер энкодера + правки тача

### Лаг «пиздец» — корень найден и починен
- Симптом: vaapi-ветка жутко лагала. Замер (writer-лог, прод): sync(GPU-кодир)
  рос 96→178 мс линейно = переполнение очереди VCE; вытеснено 74% кадров.
  Причина: блокирующий vaSyncBuffer после КАЖДОГО кадра сериализовал GPU-кодирование
  с захватом.
- Фикс: конвейер глубины 2 — кадр N засабмичен (EndPicture) и НЕ ждём; sync+чтение
  coded-буфера кадра N-1 (`prev: Option<(u32, Vec<u32>)>`, методы sync_read_coded/read_coded).
  Drop дожидается незасинхроненного кадра (3 c таймаут).
- Верификация: битстрим бит-в-бит с эталоном (vaprobe 90 → 51642 B, ffmpeg-декод
  90/90 чисто). Прод: sync стабильно 4–36 мс (не растёт), capture 37–58 fps под
  AyuGram-контендией (ffmpeg при той же нагрузке давал 18–35 fps).
- Вдобавок encode вынесен из capture-потока в отдельный поток (слот глубины 1,
  FrameRef (slot,w,h,stride) через Mutex<Option<...>>); fd слотов — через
  probes::SLOT_DMABUF (глобальный реестр, заполняется при создании dumb-буфера).
- Профилировщик: per-frame (vpp, setup, submit, sync) в writer-строке лога.

### Тач: «как попало» ЛКМ — причина
- Трейсы input: 13/13 касаний подряд по 165–306 мс уходили в ветку
  «удержание → ЛКМ-тап не синтезируем, ПКМ даст клиент». Т.е. обычные планшетные
  тапы дольше 150 мс молча превращались в «ничего» (ПКМ от клиента приходила
  только на настоящий long-press). Отсюда «клики как попало».
- Фикс: ветка «удержание» в left_button(UP) УДАЛЕНА — ЛКМ синтезируется при
  отпускании всегда (кроме свайпа/drag-пути settle_left). ПКМ не пострадала:
  long-press клиент шлёт отдельной кнопкой btn3 (подтверждено трейсами), drag —
  через settle_left (порог DRAG_HOLD_MS снижен 150→100 мс по запросу).
- Подтверждение пользователя: «идеально» (после фикс + рестарта 01:21).

### Инфра-состояние на момент Z4c
- launch_nx.sh: NX_ENCODER=vaapi (основной), ffmpeg — fallback.
- Watchdog-контракт /tmp/nx-vaapi-alive сохранён (heartbeat из encode-потока).
