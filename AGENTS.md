# AGENTS.md — nx-rust

Контракт для агента OpenCode (и любого автономного помощника), который
работает с этим репозиторием. Читается автоматически при старте в корне.

## Что это

Экспериментальный GameStream/NX-сервер на Rust: захват экрана
(`wlr-screencopy` / `xdg-desktop-portal` ScreenCast / `x11grab`) →
H.264 (VA-API или libx264) → RTP, звук Opus + FEC. Плюс watchdog USB-RNDIS
и модуль питания планшета. Это **не продукт** и не библиотека: основной
проверенный сценарий — Hyprland + AMD VA-API + Android-планшет + Moonlight;
KDE Plasma Wayland/X11 поддержан кодом и проверяется на KDE-хосте.

Главный документ — `README.md`. Если README и код расходятся, сначала
выясни, что устарело; не подгоняй README под случайную правку.

## Железные правила

1. **Не трогай чужие сервисы.** Sunshine, NoMachine (`/usr/NX/`), чужие
   pipewire/wireplumber-процессы, личные приложения пользователя — только
   чтение. Не перезапускай и не убивай их.
2. **Не перезапускай композитор.** Hyprland/KWin и пользовательская сессия
   — не твои. Остановка композитора = потеря всех окон пользователя.
3. **Не убивай пользовательские приложения.** Исключение — `com.limelight`
   в `tablet_power.py`, и только когда стрим не идёт (это его штатная
   работа, не твоя инициатива).
4. **Никаких записей в БД.** `opencode.db`, `computers4.db` и любые другие
   базы — только `mode=ro`, без `VACUUM`/`DELETE`/`wal_checkpoint` без
   явного разрешения владельца.
5. **Никакого `adb root`/`su` на чужом устройстве** без явного разрешения.
   На планшете без root большинство функций и так работает; не ломай
   гарантию.
6. **Не публикуй секреты.** Ключи, `cert.pem`, `key.pem`, PIN, личные IP,
   MAC, токены — не в README, не в коммит, не в evidence.
7. **«Физически невозможно» без цифр не принимается.** Нужна команда, лог
   или замер. Если не измерил — скажи «не проверено».
8. **Одна задача — одна причина.** Не делай широкий рефакторинг «заодно».
   Сначала минимальная правка, потом проверка, потом вывод.

## Первый запуск (разведка)

```bash
cd nx-rust
./scripts/doctor.sh          # read-only: backend, портал/x11, VA-API, звук, порты
./target/release/nx-server backend   # wlr-screencopy | xdg-desktop-portal/PipeWire | x11grab
git status --short
git log --oneline -5
```

`backend` не открывает портал и не захватывает экран — это безопасная
проверка. Если он говорит `xdg-desktop-portal/PipeWire` (KDE/GNOME
Wayland), это штатный путь: не пытайся «включить» `zwlr_screencopy` в KWin
и не перезапускай композитор. Если `x11grab` — X11-сессия. Если
`wlr-screencopy` — Hyprland/sway, проверь:

```bash
hyprctl monitors -j | head -c 2000
hyprctl clients -j | head -c 2000
echo "$XDG_RUNTIME_DIR $WAYLAND_DISPLAY $HYPRLAND_INSTANCE_SIGNATURE"
```

На KDE перед первым `serve` предупреди владельца: портал покажет диалог
«Поделиться экраном», и это нормально. Не запускай `capture`/`encode` на
KDE без разрешения — они тоже открывают портал и снимают экран.

## Сборка и проверки

```bash
cargo build --release
cargo test
python3 -m py_compile watchdog.py scripts/*.py
bash -n launch_nx.sh scripts/*.sh
./scripts/doctor.sh
```

`cargo test` должен проходить без новых падений. Предупреждения (`dead_code`,
unused FFI) — не повод ломать сборку, но новые `warning` лучше не добавлять.

## Карта репозитория

| Путь | Что там |
|---|---|
| `src/main.rs` | CLI, state dir, выбор/создание `HEADLESS-*`, запуск |
| `src/nvhttp/` | pairing, `/serverinfo`, `/applist`, `/launch`, TLS |
| `src/stream/rtsp.rs` | RTSP-хендшейк, SDP |
| `src/stream/pipeline.rs` | видео: захват → энкодер → RTP, VA-API/x264 |
| `src/stream/audio.rs` | Opus, задержка, RS(6,4) FEC |
| `src/stream/control.rs` | ENet control, ввод |
| `src/stream/input.rs` | uinput, геометрия Hyprland/KDE/X11 |
| `src/portal.rs` | KDE/GNOME Wayland: ScreenCast + PipeWire |
| `src/probes.rs` | backend-детект, wlr/portal/x11-захват |
| `src/vaapi.rs` | собственный VA-API энкодер (dmabuf) |
| `src/mdns.rs` | `_nvstream._tcp` |
| `watchdog.py` | USB-RNDIS: ping, reset, маршруты, переподключение |
| `launch_nx.sh` | sink `nx_stream`, keepalive, роутер; на KDE — monitor дефолтного sink'а, запуск |
| `scripts/tablet_power.py` | питание: idle-stop, заряд, правило 50% |
| `scripts/nx_audio_router.py` | звук по окнам (Hyprland) |
| `scripts/doctor.sh` | read-only диагностика backend/портала/VA-API |
| `evidence/` | замеры с датой; сюда же новые результаты |
| `docs/gamestream-spec.md` | разбор протокола |
| `docs/research/` | копии Sunshine (GPL-3.0), только чтение |

## Безопасная диагностика

Разрешено без спроса (всё read-only):

```bash
./target/release/nx-server backend
ss -ltnup
ss -tan state established '( sport = :49001 )'
pactl info; pactl list short sinks; pactl list short sources; pactl get-default-sink
vainfo
hyprctl monitors -j              # если Hyprland
wayland-info | grep screencopy   # если Wayland
xrandr --query                   # если X11
kscreen-doctor -j                # если KDE
busctl --user introspect org.freedesktop.portal.Desktop \
  /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast
tail -n 100 /tmp/nx-server.log
tail -n 100 /tmp/nx-x11grab.log
tail -n 100 /tmp/nx-watchdog.log
tail -n 100 /var/log/nx-tablet-power.log
cat /run/tablet_power.json
adb devices
adb shell dumpsys battery
adb shell dumpsys power | head -40
ping -c 2 192.168.8.89
```

Осторожно (может прервать стрим/сессию, спроси владельца):

```bash
pkill -x nx-server
systemctl restart nx-watchdog nx-tablet-power
./launch_nx.sh
./target/release/nx-server capture auto /tmp/frame.png   # на KDE откроет портал
./target/release/nx-server serve ...                     # на KDE покажет диалог ScreenCast
adb shell am force-stop com.limelight
USBDEVFS_RESET (watchdog.py --repair-now)
```

Запрещено:

```bash
pkill -f nx-server            # убивает собственный шелл/агента
pkill -f <имя-скрипта>        # то же
systemctl restart hyprland    # или любой композитор
systemctl restart xdg-desktop-portal*  # без разрешения: ломает ScreenCast-сессию
adb root / adb shell su ...   # на чужом планшете без разрешения
setprop sys.usb.config none
wm density reset
svc power stayon true
```

## Типовые задачи → куда смотреть

| Задача | Файлы / проверки |
|---|---|
| Moonlight не находит хост | `src/mdns.rs`, UDP 5353, `ss`, фаервол, `NX_SERVER_NAME` |
| Pairing/TLS | `src/nvhttp/pairing.rs`, `cert.pem`/`key.pem` в state dir, TLS 1.2 |
| Чёрный экран | `doctor.sh`, `nx-server backend`, `hyprctl monitors` (wlr), портал (KDE Wayland), `DISPLAY` (X11), `src/stream/pipeline.rs` |
| KDE Wayland: нет диалога/чёрный экран | `src/portal.rs`, `xdg-desktop-portal`/`-kde`, PipeWire, `busctl ... ScreenCast`, лог `/tmp/nx-server.log` |
| KDE X11: нет захвата | `NX_CAPTURE=x11`, `DISPLAY`, `xrandr`, `ffmpeg -devices \| grep x11grab`, `/tmp/nx-x11grab.log` |
| Нет ввода на KDE | `src/stream/input.rs`, `/dev/uinput`, группа `input`, `kscreen-doctor -j` |
| Низкий fps | `src/vaapi.rs`, `src/stream/pipeline.rs`, загрузка GPU (`radeontop`), `evidence/fps-under-load-2026-10-21.md` |
| Шипение/треск | `src/stream/audio.rs`, sink `nx_stream`, keepalive, 200 pkt/s, `evidence/audio-hiss-2026-10-21.md` |
| Задержка | `NX_AUDIO_DELAY_MS`, `frame_pacing`, `evidence/latency-2026-10-21.md` |
| USB-канал завис | `watchdog.py`, `errs==packets`, ребут планшета, `evidence/xhci-wedge-2026-10-20.md` |
| Планшет не заряжается | `scripts/tablet_power.py`, `evidence/tablet-battery-2026-10-22.md`; USB-A 500 мА — не лечится софтом |
| Звук только на хосте / только на планшете | `scripts/nx_audio_router.py`, `pactl`, workspace на `HEADLESS-*`; на KDE — monitor дефолтного sink'а |

## Правило доказательств

Любое утверждение о причине подкрепляй:

- командой и её выводом (можно сокращённым);
- логом с временем;
- числом (мс, fps, мА, пакеты/с);
- ссылкой на файл/строку.

Результат — новый файл `evidence/YYYY-MM-DD-короткое-имя.md`:
дата, железо, гипотеза, что мерили, сырые числа, вывод, что осталось
непроверенным. Не переписывай старые evidence задним числом; добавляй
новый раздел/файл.

## Definition of Done

- [ ] Правка минимальна и решает заявленную причину, а не симптом.
- [ ] `cargo test` проходит; `python3 -m py_compile` и `bash -n` чистые.
- [ ] `./scripts/doctor.sh` не показывает новых FAIL.
- [ ] Проверено на живом сценарии (или явно написано, что не проверено).
- [ ] Для KDE: проверен `nx-server backend` и первый диалог портала (или явно
      написано, что KDE-хост не предоставлен).
- [ ] Новые команды/переменные отражены в README и `config/nx.env.example`.
- [ ] Если менялось поведение — обновлён этот файл.
- [ ] В диффе нет секретов, личных IP, ключей и мусора (`target/`, `__pycache__`).
- [ ] Коммит на русском, в стиле репозитория: `область: что и зачем`.

## Коммиты

История — русская, по одной законченной теме:

```
audio: FEC RS(6,4) и задержка в форвардере
watchdog: не перезапускать сервер без клиента
docs: README и инструкция для агента
```

Не коммить `target/`, `__pycache__/`, `.jspace/`, логи, `cert.pem`,
`key.pem`, `*.db`. Они уже в `.gitignore`; проверь `git status` перед
коммитом.

## Ограничения, которые не надо «чинить» наугад

- **KDE/GNOME Wayland**: нет `wlr-screencopy` — это архитектура композитора,
  а не баг nx-rust. Используй `xdg-desktop-portal` ScreenCast; не пытайся
  включить `zwlr` в KWin.
- **KDE-портал**: диалог выбора монитора обязателен; это не ошибка и не
  «зависание». Restore-токен может не сработать после обновления портала.
- **KDE/X11**: `x11grab` снимает весь X11 Screen, а не отдельный монитор.
- **NVIDIA**: VA-API нет; работает программный `libx264`
  (`NX_FFMPEG_ENCODER=x264`), NVENC не реализован. `auto` проверяет
  `vainfo` и сам выберет x264, если H.264 encode нет.
- **Carrizo**: 60 fps только на лёгкой сцене; тяжёлая игра упирается в GPU.
  Решение — настройки игры, а не «оптимизация» сервера.
- **USB-A 500 мА**: драйвер MTK/SGM41513 выключает заряд; софтом не
  лечится. Нужен Type-C-источник или сетевой зарядник.
- **RNDIS-зависание**: иногда помогает только ребут планшета; не пытайся
  бесконечно дёргать `USBDEVFS_RESET` (сожжёшь xHCI).
