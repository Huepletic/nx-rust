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

# Если WAYLAND_DISPLAY не задан (запуск из systemd/watchdog), берём его из
# окружения живого Hyprland — так не нужно угадывать wayland-0/wayland-1.
if [ -z "${NX_WAYLAND_DISPLAY:-}" ] && [ -z "${WAYLAND_DISPLAY:-}" ]; then
  WPID=$(pgrep -x Hyprland 2>/dev/null | head -1)
  if [ -n "$WPID" ]; then
    WAYLAND_DISPLAY=$(tr '\0' '\n' < "/proc/$WPID/environ" 2>/dev/null \
      | sed -n 's/^WAYLAND_DISPLAY=//p' | head -1)
  fi
fi
WAYLAND_DISPLAY="${NX_WAYLAND_DISPLAY:-${WAYLAND_DISPLAY:-wayland-1}}"
export WAYLAND_DISPLAY

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

# Роутер: приложения с окнами на стрим-мониторе → nx_stream, остальные →
# дефолтный sink. Без него весь звук десктопа дублируется на планшете.
# Это Hyprland-специфичная часть; на KDE/другом композиторе её можно не
# запускать (тогда NX_AUDIO_SOURCE лучше не задавать — возьмётся monitor
# дефолтного sink'а).
if [ "${NX_AUDIO_ROUTER:-1}" = "1" ] && command -v hyprctl >/dev/null 2>&1; then
  if ! pgrep -f 'nx_audio_rout[e]r' >/dev/null; then
    setsid python3 scripts/nx_audio_router.py >> /tmp/nx-audio-router.log 2>&1 < /dev/null &
  fi
fi

SIG=$(ls -t "$XDG_RUNTIME_DIR/hypr/" 2>/dev/null | grep _ | head -1)
exec setsid env \
  XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" \
  WAYLAND_DISPLAY="$WAYLAND_DISPLAY" \
  HYPRLAND_INSTANCE_SIGNATURE="$SIG" \
  NX_PIN_WAIT=1 \
  NX_BITRATE_KBPS="${NX_BITRATE_KBPS:-16000}" \
  NX_ENCODER="${NX_ENCODER:-vaapi}" \
  NX_AUDIO_DELAY_MS="${NX_AUDIO_DELAY_MS:-80}" \
  NX_AUDIO_BITRATE_KBPS="${NX_AUDIO_BITRATE_KBPS:-300}" \
  NX_AUDIO_SOURCE="${NX_AUDIO_SOURCE:-nx_stream.monitor}" \
  "$SERVER_DIR/target/release/nx-server" serve \
    --name "$SERVER_NAME" --pin "$SERVER_PIN" \
  >> "${NX_SERVER_LOG:-/tmp/nx-server.log}" 2>&1 < /dev/null
