#!/bin/bash
# Поднять nx-server в контексте пользовательской Wayland-сессии.
# Используется watchdog'ом (restart_server) и вручную.
#
# setsid обязателен: без него процесс привязан к вызвавшему шеллу и умирает
# вместе с ним (наблюдалось 2026-10-18 02:57:23).
#
# Настройки берутся из окружения или /etc/nx-rust/nx.env (пример —
# config/nx.env.example). Пути не захардкожены: по умолчанию это каталог,
# где лежит сам скрипт.
set -u

HERE="$(cd "$(dirname "$0")" && pwd)"
[ -r "${NX_CONFIG:-/etc/nx-rust/nx.env}" ] && . "${NX_CONFIG:-/etc/nx-rust/nx.env}"

SERVER_DIR="${NX_SERVER_DIR:-$HERE}"
DEF_NAME="$(cat /proc/sys/kernel/hostname 2>/dev/null || echo nx-host)"
SERVER_NAME="${NX_SERVER_NAME:-${DEF_NAME%%.*}}"
SERVER_PIN="${NX_SERVER_PIN:-1234}"

export XDG_RUNTIME_DIR="${XDG_RUNTIME_DIR:-/run/user/$(id -u)}"
export DBUS_SESSION_BUS_ADDRESS="${DBUS_SESSION_BUS_ADDRESS:-unix:path=$XDG_RUNTIME_DIR/bus}"

# Если WAYLAND_DISPLAY/XDG_SESSION_TYPE не заданы (запуск из systemd/watchdog),
# берём их из окружения живой сессии. Hyprland — старый путь; KDE Plasma живёт
# в plasmashell/ksmserver, GNOME — в gnome-shell. Это нужно только для
# автоопределения backend'а: портал ScreenCast сам находит compositor по D-Bus
# и WAYLAND_DISPLAY ему не требуется.
SESSION_PID=""
for p in Hyprland plasmashell ksmserver gnome-shell sway; do
  SESSION_PID=$(pgrep -x "$p" 2>/dev/null | head -1)
  [ -n "$SESSION_PID" ] && break
done
if [ -n "$SESSION_PID" ] && [ -r "/proc/$SESSION_PID/environ" ]; then
  getenv() { tr '\0' '\n' < "/proc/$SESSION_PID/environ" 2>/dev/null | sed -n "s/^$1=//p" | head -1; }
  [ -z "${XDG_SESSION_TYPE:-}" ] && XDG_SESSION_TYPE=$(getenv XDG_SESSION_TYPE)
  [ -z "${XDG_CURRENT_DESKTOP:-}" ] && XDG_CURRENT_DESKTOP=$(getenv XDG_CURRENT_DESKTOP)
  [ -z "${DISPLAY:-}" ] && DISPLAY=$(getenv DISPLAY)
  [ -z "${WAYLAND_DISPLAY:-}" ] && WAYLAND_DISPLAY=$(getenv WAYLAND_DISPLAY)
  [ -z "${DBUS_SESSION_BUS_ADDRESS:-}" ] && DBUS_SESSION_BUS_ADDRESS=$(getenv DBUS_SESSION_BUS_ADDRESS)
  unset -f getenv
fi
if [ -z "${XDG_SESSION_TYPE:-}" ]; then
  XDG_SESSION_TYPE=$(loginctl show-session "${XDG_SESSION_ID:-}" -p Type --value 2>/dev/null || true)
fi
if [ -n "${NX_WAYLAND_DISPLAY:-}" ]; then
  WAYLAND_DISPLAY="$NX_WAYLAND_DISPLAY"
elif [ -z "${WAYLAND_DISPLAY:-}" ]; then
  for s in wayland-0 wayland-1 wayland-2; do
    if [ -S "$XDG_RUNTIME_DIR/$s" ]; then
      WAYLAND_DISPLAY="$s"
      break
    fi
  done
fi
if [ -n "${WAYLAND_DISPLAY:-}" ] && { [ -S "$WAYLAND_DISPLAY" ] || [ -S "$XDG_RUNTIME_DIR/$WAYLAND_DISPLAY" ]; }; then
  export WAYLAND_DISPLAY
else
  unset WAYLAND_DISPLAY
fi
export XDG_SESSION_TYPE XDG_CURRENT_DESKTOP DISPLAY
export DBUS_SESSION_BUS_ADDRESS

cd "$SERVER_DIR" || exit 1
pkill -x nx-server 2>/dev/null
sleep 1

# Виртуальный sink для стрим-звука: nx-server слушает только его monitor.
# Приложения на стрим-мониторе роутит scripts/nx_audio_router.py, остальные
# играют в дефолтный sink (динамики хоста) и в стрим не попадают.
pactl list short sinks | awk -F'\t' '$2=="nx_stream"{f=1} END{exit !f}' || \
  pactl load-module module-null-sink sink_name=nx_stream \
    sink_properties=device.description=NXStream
# Keepalive: тихий поток в nx_stream, чтобы sink не проваливался в IDLE.
# В IDLE monitor отдаёт тишину рывками (118 pkt/s вместо 200, замерено
# 2026-10-21) → клиентский AudioTrack голодает → шипение. С keepalive sink
# всегда RUNNING, monitor идёт ровно (200 pkt/s).
# Поток создаётся модулем, у него application.process.id нет ("-"), поэтому
# роутер его не трогает (scripts/nx_audio_router.py: pid is None → skip).
# В short-выводе pactl второе поле sink-input'а — ИНДЕКС sink'а, не имя.
NX_IDX=$(pactl list short sinks | awk -F'\t' '$2=="nx_stream"{print $1; exit}')
if [ -n "$NX_IDX" ] && ! pactl list short sink-inputs | awk -F'\t' -v i="$NX_IDX" '$2==i{f=1} END{exit !f}'; then
  pactl load-module module-null-source source_name=nx_keepalive \
    source_properties=device.description=NXKeepAlive >/dev/null 2>&1
  pactl load-module module-loopback source=nx_keepalive sink=nx_stream \
    >/dev/null 2>&1
fi

# KDE/GNOME: Hyprland-роутера нет. Снимаем monitor дефолтного sink'а — планшет
# слышит весь звук хоста (и динамики хоста тоже играют; это ожидаемо). Если
# NX_AUDIO_SOURCE задан явно (например, nx_stream.monitor), не переопределяем.
if ! command -v hyprctl >/dev/null 2>&1 && [ -z "${NX_AUDIO_SOURCE:-}" ]; then
  DEF_SINK=$(pactl get-default-sink 2>/dev/null || true)
  [ -n "$DEF_SINK" ] && NX_AUDIO_SOURCE="${DEF_SINK}.monitor"
fi
NX_AUDIO_SOURCE="${NX_AUDIO_SOURCE:-nx_stream.monitor}"

# Роутер: приложения с окнами на стрим-мониторе → nx_stream, остальные →
# дефолтный sink. Без него весь звук десктопа дублируется на планшете.
# Это Hyprland-специфичная часть; на KDE/другом композиторе её не запускаем.
if [ "${NX_AUDIO_ROUTER:-1}" = "1" ] && command -v hyprctl >/dev/null 2>&1; then
  if ! pgrep -f 'nx_audio_rout[e]r' >/dev/null; then
    setsid python3 scripts/nx_audio_router.py >> /tmp/nx-audio-router.log 2>&1 < /dev/null &
  fi
fi

SIG=$(ls -t "$XDG_RUNTIME_DIR/hypr/" 2>/dev/null | grep _ | head -1)
if command -v hyprctl >/dev/null 2>&1; then
  DEF_ENCODER=vaapi
else
  DEF_ENCODER=ffmpeg
fi
exec setsid env \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  WAYLAND_DISPLAY="${WAYLAND_DISPLAY:-}" \
  DISPLAY="${DISPLAY:-}" \
  XDG_SESSION_TYPE="${XDG_SESSION_TYPE:-}" \
  XDG_CURRENT_DESKTOP="${XDG_CURRENT_DESKTOP:-}" \
  DBUS_SESSION_BUS_ADDRESS="$DBUS_SESSION_BUS_ADDRESS" \
  HYPRLAND_INSTANCE_SIGNATURE="$SIG" \
  NX_PIN_WAIT=1 \
  NX_CAPTURE="${NX_CAPTURE:-auto}" \
  NX_STATE_DIR="${NX_STATE_DIR:-$HOME/.local/state/nx-rust}" \
  NX_BITRATE_KBPS="${NX_BITRATE_KBPS:-16000}" \
  NX_ENCODER="${NX_ENCODER:-$DEF_ENCODER}" \
  NX_FFMPEG_ENCODER="${NX_FFMPEG_ENCODER:-auto}" \
  NX_VAAPI_DEVICE="${NX_VAAPI_DEVICE:-/dev/dri/renderD128}" \
  NX_AUDIO_DELAY_MS="${NX_AUDIO_DELAY_MS:-80}" \
  NX_AUDIO_BITRATE_KBPS="${NX_AUDIO_BITRATE_KBPS:-300}" \
  NX_AUDIO_SOURCE="$NX_AUDIO_SOURCE" \
  "$SERVER_DIR/target/release/nx-server" serve \
    --name "$SERVER_NAME" --pin "$SERVER_PIN" \
  >> "${NX_SERVER_LOG:-/tmp/nx-server.log}" 2>&1 < /dev/null
