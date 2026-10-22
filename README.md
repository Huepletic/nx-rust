# nx-rust

Свой GameStream/NX-сервер на Rust: стримит Linux-десктоп на Android-планшет
через [Moonlight](https://moonlight-stream.org/). Захват Wayland
(`wlr-screencopy`) → аппаратный H.264 (VA-API) → RTP; звук PipeWire → Opus →
RTP с FEC. В комплекте — обвязка для USB-канала, watchdog и модуль питания
планшета.

**Статус: экспериментальный личный проект, не продукт.** Проверен на одном
железе: Arch Linux + Hyprland + AMD Carrizo (VA-API) + планшет MediaTek
(Android 11) + Moonlight 12.2. На KDE/GNOME **не работает** — там нет
`zwlr_screencopy_manager_v1` (подробнее в разделе «Совместимость»).

| Клиент (Moonlight) | Хост (HEADLESS-вывод) |
|---|---|
| <img src="docs/proof/moonlight-paired-applist.png" width="300"> | <img src="docs/proof/m3-stream-live-tablet.png" width="420"> |

## Что умеет

- **NVHTTP/HTTPS**: `serverinfo`, `applist`, `launch`, pairing по PIN,
  самоподписанный сертификат, mDNS-объявление `_nvstream._tcp`.
- **Видео**: захват виртуального вывода `HEADLESS-*` через
  `zwlr_screencopy_manager_v1`, собственный VA-API-энкодер (dmabuf → VPP →
  H.264, zero-copy) и ffmpeg-фоллбек. 1088×800@60, 16 Мбит/с — проверенный
  режим; разрешение/битрейт задаёт клиент.
- **Звук**: Opus 300 кбит/с, 200 data-пакетов/с + 100 FEC-пакетов/с,
  RS(6,4) с паритетом над зашифрованными шардами (как в Sunshine). Задержка
  применяется в форвардере, а не в ffmpeg.
- **Ввод**: тачскрин, кнопки, стики — через ENet control-канал.
- **Замеры**: медиана сквозной задержки **44–47 мс** на 1088×800/60 через
  USB RNDIS; методика и сырые данные — `evidence/latency-2026-10-21.md`.
- **Обвязка** (опционально): watchdog USB-RNDIS, модуль питания планшета,
  роутинг звука по окнам Hyprland, скрипты bring-up.

## Архитектура

```
┌──────────────┐   NVHTTP/TLS 48984/48989   ┌───────────────────────────────┐
│  Moonlight   │◄──────────────────────────►│  nx-server (Rust)             │
│  (Android)   │   RTSP 49010               │  ├─ nvhttp/pairing            │
│              │   RTP video 49000          │  ├─ stream/{rtsp,control}     │
│              │   ENet control 49001       │  ├─ vaapi encoder             │
│              │   RTP audio 49002          │  └─ mDNS _nvstream._tcp       │
└──────────────┘                            └──────────┬────────────────────┘
                                                       │ wlr-screencopy
                                                       ▼
                                            ┌───────────────────────┐
                                            │ Hyprland HEADLESS-*   │
                                            │ 1088×800@60           │
                                            └───────────────────────┘
```

Порты нестандартные (489xx), чтобы не конфликтовать с Sunshine (479xx).
Moonlight находит сервер по mDNS; при ручном добавлении нужен HTTP-порт
`48989`.

## Совместимость

| Компонент | Статус |
|---|---|
| Хост | Arch Linux — проверено. Другие дистрибутивы с Wayland и VA-API — вероятно, но не тестировалось. |
| Композитор | **Hyprland** — проверено. Sway — возможно, если создать виртуальный вывод заранее (не тестировалось). **KDE/KWin и GNOME/Mutter — нет**: они не отдают `zwlr_screencopy_manager_v1`. |
| GPU | AMD VA-API (Carrizo) — проверено. Intel VA-API — вероятно. NVIDIA — нет (нужен свой путь энкодера). |
| Клиент | Moonlight Android 12.2 (315) — проверено. Другие клиенты Moonlight — теоретически, протокол общий. |
| Root на планшете | **Не нужен** для WiFi-режима. Нужен только для USB-автоматики конкретного MediaTek-гаджета (см. ниже). |
| Звук | PipeWire или PulseAudio. |

### Если у вас KDE (важно)

KWin не реализует `zwlr_screencopy_manager_v1`. Сервер не сможет захватить
экран, и это не лечится настройками — нужен другой бэкенд захвата.

Варианты:

1. **Отдельная Hyprland/Sway-сессия** (не вложенная в KDE) — проверенный путь.
   Можно держать KDE для работы, а стрим запускать из отдельной TTY-сессии.
2. **Вложенный wlroots-композитор** внутри KDE — теоретически возможно, но
   не тестировалось; не рекомендую как первый шаг.
3. **Патч захвата под xdg-desktop-portal/PipeWire** — в планах нет.

Если вы просто хотите посмотреть код и замеры — README, `evidence/` и
`docs/gamestream-spec.md` самодостаточны; запускать на KDE не обязательно.

### Планшет без root

- **WiFi-режим**: работает всё, что нужно для стрима. Установите Moonlight,
  подключите планшет и хост к одной сети, добавьте хост по mDNS или IP.
- **USB-режим**: включите USB-тетеринг в настройках Android (Настройки →
  Сеть → Точка доступа → USB-тетеринг). Хост увидит RNDIS-интерфейс.
  Наш `fix_gadget3.sh`, статические адреса и fwmark-маршрут — специфика
  конкретного MediaTek-гаджета; для них нужен `su`/Magisk. Без root эти
  шаги пропустите.
- **Модуль питания**: работает без root (adb + `dumpsys battery` + sysfs).
  Без root не читается `dmesg`, поэтому диагностика регистров зарядчика
  будет пустой; остальное доступно.

## Быстрый старт

### 1. Зависимости

Arch Linux:

```bash
sudo pacman -S --needed rust cargo ffmpeg libva libva-utils \
  pipewire pipewire-pulse wireplumber wayland wayland-utils \
  android-tools python
```

Для AMD нужен `mesa` (обычно уже стоит). Для Intel — `intel-media-driver`
или `libva-intel-driver`. Проверка:

```bash
vainfo | grep -A2 H264High
```

Должны быть `VAEntrypointEncSlice` (энкодер) и `VAEntrypointVLD` (декодер).

### 2. Сборка

```bash
git clone <URL> nx-rust
cd nx-rust
cargo build --release
```

### 3. Виртуальный вывод

Сервер сам создаст `HEADLESS-*` через `hyprctl`, если вывода нет. Чтобы
картинка была пиксель-в-пиксель, задайте режим 1088×800@60. В Hyprland:

```conf
# hyprland.conf
monitor=HEADLESS-1,1088x800@60,0x0,1
```

или на лету:

```bash
hyprctl keyword monitor "HEADLESS-1,1088x800@60,0x0,1"
```

Игры/окна запускайте на этом выводе (перетащите окно или используйте
workspace-правило). Захватывается весь вывод, поэтому панель/док тоже
попадут в кадр — для «полного десктопа» это нормально.

### 4. Запуск

Минимальный вариант (звук — monitor дефолтного sink'а, всё аудио хоста
уходит в стрим):

```bash
export NX_BITRATE_KBPS=16000
export NX_ENCODER=vaapi
export NX_AUDIO_BITRATE_KBPS=300
export NX_AUDIO_DELAY_MS=80
./target/release/nx-server serve --name "$(cat /proc/sys/kernel/hostname)" --pin 1234
```

Рекомендуемый вариант — через `launch_nx.sh`: он создаёт sink `nx_stream`,
keepalive, поднимает роутер звука (Hyprland) и передаёт настройки:

```bash
sudo mkdir -p /etc/nx-rust
sudo cp config/nx.env.example /etc/nx-rust/nx.env
sudoedit /etc/nx-rust/nx.env     # NX_SERVER_DIR, NX_WAYLAND_DISPLAY, PIN
./launch_nx.sh
```

`launch_nx.sh` можно запускать и без `/etc/nx-rust/nx.env` — тогда пути
берутся от каталога скрипта, имя — от hostname, остальное — дефолты.

### 5. Сопряжение

1. Откройте Moonlight → «Добавить ПК». Сервер должен найтись сам по mDNS;
   если нет — введите IP хоста (HTTP-порт `48989`).
2. Введите PIN из `--pin` (по умолчанию `1234` — смените).
3. Выберите приложение «Desktop» (или игру, если добавили её в `applist`).
4. Настройки клиента: 1088×800, 60 fps, 16 Мбит/с, HDR off, frame pacing
   «latency», звук stereo.

### 6. Проверка

```bash
./scripts/doctor.sh
```

Скрипт только читает: композитор, `wlr-screencopy`, VA-API, звук, порты,
state dir. Exit 1 — есть блокеры. На KDE он честно скажет, что захват
не поддерживается.

## Конфигурация

Все настройки — переменные окружения. Их можно положить в
`/etc/nx-rust/nx.env` (пример — `config/nx.env.example`); файл читают
`watchdog.py`, `tablet_power.py` и `launch_nx.sh`. Окружение имеет приоритет
над файлом.

`NX_SERVER_DIR`, `NX_SERVER_NAME`, `NX_SERVER_PIN` и `NX_WAYLAND_DISPLAY`
читают скрипты; сам `nx-server` принимает `--name`, `--pin`, `--output`,
`--http-port`, `--https-port`. Остальные `NX_*` (битрейт, энкодер, аудио)
читает сервер.

| Переменная | По умолчанию | Смысл |
|---|---|---|
| `NX_SERVER_DIR` | каталог репозитория | абсолютный путь (нужен systemd-юнитам) |
| `NX_SERVER_NAME` | hostname | имя в Moonlight |
| `NX_SERVER_PIN` | `1234` | PIN сопряжения |
| `NX_WAYLAND_DISPLAY` | `wayland-1` | `echo $WAYLAND_DISPLAY` в сессии |
| `NX_OUTPUT` | `auto` | `auto` = первый `HEADLESS-*`, или имя вывода |
| `NX_BITRATE_KBPS` | клиентский | видео-битрейт, проверено 16000 |
| `NX_ENCODER` | `ffmpeg` | `vaapi` — свой энкодер, иначе ffmpeg (тоже VA-API) |
| `NX_GOP` | `40` | размер GOP |
| `NX_AUDIO_BITRATE_KBPS` | `96` | Opus, проверено 300 |
| `NX_AUDIO_DELAY_MS` | `0` | задержка аудио в форвардере, проверено 80 |
| `NX_AUDIO_SOURCE` | monitor дефолтного sink'а | `nx_stream.monitor` для роутера |
| `NX_STATE_DIR` | `~/.local/state/nx-rust` | `uniqueid`, сертификат, ключ |
| `NX_PIN_WAIT` | — | `0` — не ждать PIN при pairing |
| `NX_HOST_IP` / `NX_TABLET_IP` | `192.168.8.1` / `.89` | RNDIS-адреса |
| `NX_TABLET_SUBNET` | `192.168.8.0/24` | RNDIS-подсеть |
| `NX_USER` / `NX_UID` | владелец репозитория | от кого запускать Wayland/PipeWire |
| `NX_QUICKSHELL_UNIT` | пусто | личный quickshell автора; в чужом окружении не задавать |

Важно: `NX_STATE_DIR` должен переживать рестарты. Если потерять `uniqueid`
или сертификат, Moonlight решит, что это другой ПК («Polling returned the
wrong PC»), и pairing придётся проходить заново.

## Обвязка (опционально)

### `watchdog.py` — USB-RNDIS

Лечит зависший USB-канал: `USBDEVFS_RESET`, восстановление адресов,
fwmark-маршрута, mDNS-членства и переподключение Moonlight. Нужен только
в USB-режиме; для WiFi не требуется.

```bash
sudo cp systemd/nx-watchdog.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nx-watchdog.service
```

Юнит требует `NX_SERVER_DIR` в `/etc/nx-rust/nx.env`. Watchdog работает
root'ом (USB-сброс, адреса), а действия в сессии делает через `sudo -u
$NX_USER`. На планшете для gadget/fwmark нужен `su`; без root эти шаги
деградируют, остальное работает. В юните стоит `KillMode=process`: рестарт
watchdog'а не убивает `nx-server` (он запускается через `setsid`).

Диагностика без восстановления:

```bash
python3 watchdog.py --check
```

### `scripts/tablet_power.py` — питание планшета

Гасит `com.limelight`, когда стрим не идёт (экономия ~150 мА), следит за
зарядом и правилом «пассивный заряд ≥ 50% от пикового активного разряда».
Пишет `/run/tablet_power.json` и `/var/log/nx-tablet-power.log`.

```bash
sudo cp systemd/nx-tablet-power.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now nx-tablet-power.service
```

Модуль не требует root на планшете (adb + battery sysfs + dumpsys).
Подробный разбор, почему USB-A-порт не заряжает этот планшет, — в
`evidence/tablet-battery-2026-10-22.md`.

### `scripts/nx_audio_router.py` — звук по окнам

Hyprland-специфично: приложения, чьи окна на `HEADLESS-*`, уходят в
`nx_stream`; остальные играют в дефолтный sink хоста. Без роутера весь
звук десктопа дублируется на планшет. Запускается из `launch_nx.sh`.

### `scripts/tablet_bringup.sh` — USB bring-up

Поднимает adb/RNDIS после ребута, ставит Magisk-модуль `libui_gralloc_fix`
(опционально), проверяет CPU/декодер. Это скрипт под конкретный
MediaTek-гаджет; для обычного Android с USB-тетерингом не нужен.

### `tools/` — диагностика

Пробники VA-API, dmabuf, захвата. `tools/patch_libui_gralloc.py` патчит
проприетарную `libui.so` на планшете (нужен Magisk) — используйте только
если понимаете, что делаете; к основному стриму это не относится.

## Troubleshooting

| Симптом | Причина | Что делать |
|---|---|---|
| Moonlight: «Offline», хост не находится | mDNS не доходит (фаервол, другой интерфейс, 5353 занят) | `ss -lunp \| grep 5353`; разрешить UDP 5353; добавить хост по IP |
| Ручное добавление по IP не подключается | клиент ждёт стандартный HTTPS-порт 47984 | добавьте по mDNS; либо запустите с `--http-port 47989 --https-port 47984` (если Sunshine не нужен) |
| TLS alert 70 / connection reset | клиент шлёт TLS 1.2, а rustls собрался без tls12 | в `Cargo.toml` уже включён `tls12`; пересобрать `cargo build --release` |
| Чёрный экран, кадров нет | нет `zwlr-screencopy` (KDE/GNOME), нет `HEADLESS-*`, VA-API не стартовал | `./scripts/doctor.sh`; `hyprctl monitors`; лог `/tmp/nx-server.log` |
| Низкий fps, рывки | GPU-сатурация (Carrizo), тяжёлая игра, DDLC/шейдеры | снизить настройки игры/разрешение; проверить `radeontop`; watchdog поднимает nice |
| Шипение/треск в звуке | sink в IDLE (нет keepalive), недобор пакетов, потеря RTP | проверить 200 pkt/s; `launch_nx.sh` создаёт keepalive; FEC RS(6,4) уже в коде |
| Звука нет | не тот monitor, ffmpeg умер, sink пропал после рестарта PipeWire | `pactl list short sources`; `NX_AUDIO_SOURCE=nx_stream.monitor`; перезапустить `launch_nx.sh` |
| USB-канал завис (ping 100% loss, `errs==packets`) | зависшая TX-очередь RNDIS | перезагрузить **планшет**; watchdog делает USB reset, но при `xHCI HW did not halt` помогает только ребут |
| Планшет не заряжается | USB-A-порт 500 мА, драйвер MTK/SGM41513 выключает заряд | Type-C↔Type-C в порт хоста или сетевой зарядник; модуль покажет `USB_PORT_TOO_WEAK` |
| «Polling returned the wrong PC» | потерян `uniqueid`/сертификат в `NX_STATE_DIR` | вернуть state dir; заново пройти pairing |
| Порты заняты Sunshine | Sunshine слушает 479xx | у нас 489xx, конфликта нет; проверить `ss -ltnup` |

Подробные разборы с цифрами:

- `evidence/latency-2026-10-21.md` — медиана 44–47 мс, разложение по слоям.
- `evidence/audio-hiss-2026-10-21.md` — четыре причины шипения и FEC.
- `evidence/fps-under-load-2026-10-21.md` — почему 60 fps несовместимы
  с тяжёлой игрой на Carrizo.
- `evidence/tablet-battery-2026-10-22.md` — зарядка, регистры, правило 50%.
- `evidence/xhci-wedge-2026-10-20.md` — зависание xHCI из-за RNDIS.

## Работа с агентом OpenCode

В корне лежит `AGENTS.md` — это контракт для агента: что можно, что нельзя,
как проверять. OpenCode читает его автоматически.

Типовой старт:

```bash
cd nx-rust
opencode
```

Первый запрос агенту:

> Прочитай `README.md` и `AGENTS.md`. Запусти `./scripts/doctor.sh`.
> Ничего не меняй. Отчитайся: железо, композитор, что упало, какой режим
> (WiFi/USB) и какие логи смотреть.

Дальше — по одной задаче за раз. Агент должен:

1. Сначала собрать факты (доктор, логи, `ss`, `adb`), потом менять код.
2. Делать минимальную правку, проверять `cargo test`, `bash -n`,
   `python3 -m py_compile`.
3. Для каждого утверждения о причине прикладывать команду/лог/цифру.
4. Результат измерения — в `evidence/YYYY-MM-DD-*.md`.
5. Обновлять README/AGENTS, если поменялись команды или поведение.

Чего агенту делать нельзя (см. `AGENTS.md`):

- трогать Sunshine/NoMachine и чужие сервисы;
- перезапускать композитор, убивать пользовательские приложения;
- писать в `opencode.db` и другие БД;
- выполнять `adb root`/`su` на чужом планшете без разрешения;
- публиковать секреты, ключи, PIN и личные IP;
- обещать «физически невозможно» без замеров.

## Структура репозитория

```
src/
  main.rs              CLI, запуск сервера, state dir, HEADLESS
  nvhttp/              pairing, serverinfo/applist/launch, TLS
  stream/              RTSP, control (ENet), видео-пайплайн, аудио+FEC
  vaapi.rs             свой VA-API энкодер
  mdns.rs              _nvstream._tcp
  dma.rs, probes.rs    dmabuf и диагностические пробники
watchdog.py            USB-RNDIS watchdog
launch_nx.sh           запуск сервера + sink + роутер
scripts/
  doctor.sh            read-only проверка окружения
  tablet_power.py      модуль питания
  nx_audio_router.py   роутинг звука (Hyprland)
  tablet_bringup.sh    USB bring-up (MediaTek)
  measure_latency.py   замер сквозной задержки
systemd/               юниты watchdog и питания
config/nx.env.example  шаблон конфигурации
docs/gamestream-spec.md  разбор протокола
docs/research/         референсные исходники Sunshine (GPL-3.0)
evidence/              замеры и разборы
```

## Безопасность

Проект рассчитан на **доверенную локальную сеть**. Не выставляйте порты
48984/48989/49000–49010 в интернет. Pairing защищён PIN и самоподписанным
TLS-сертификатом, но это не замена VPN. Смените PIN по умолчанию, не
запускайте от root без необходимости, не публикуйте `NX_STATE_DIR`
(там приватный ключ сервера).

## Лицензия и благодарности

Код — **GPL-3.0** (см. `LICENSE`). Это осознанно: в `docs/research/`
лежат референсные исходники [Sunshine](https://github.com/LizardByte/Sunshine)
под GPL-3.0, а сам сервер реализует тот же протокол. Подробности — в
`NOTICE` и `docs/research/README.md`.

- [Moonlight](https://moonlight-stream.org/) — клиент и протокол GameStream.
- [Sunshine](https://github.com/LizardByte/Sunshine) — референсная
  реализация сервера; по ней сверялись порты, pairing, RTP и FEC.
- [moonlight-common-c](https://github.com/moonlight-stream/moonlight-common-c) —
  разбор RTSP/control.
- [Hyprland](https://hyprland.org/) — wlr-screencopy и headless-выводы.
- [PipeWire](https://pipewire.org/) — звук.

Проект не связан с NVIDIA, LizardByte или Moonlight; названия использованы
для описания совместимости.
