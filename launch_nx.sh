#!/bin/bash
# Поднять nx-server с контекстом сессии et (Wayland/Hyprland).
# Используется watchdog'ом (restart_server) и вручную.
# setsid обязателен: без него процесс привязан к вызвавшему шеллу
# и умирает вместе с ним (наблюдалось 2026-10-18 02:57:23).
cd /home/et/nx-rust || exit 1
pkill -x nx-server 2>/dev/null
sleep 1
# Виртуальный sink для стрим-звука: nx-server слушает только его monitor.
# Приложения на HEADLESS-4 роутит scripts/nx_audio_router.py, остальные
# играют в дефолтный sink (динамики хоста) и в стрим не попадают.
pactl list short sinks | awk -F'\t' '$2=="nx_stream"{f=1} END{exit !f}' || \
  pactl load-module module-null-sink sink_name=nx_stream \
    sink_properties=device.description=NXStream
# Роутер: приложения с окнами на стрим-мониторе → nx_stream, остальные →
# дефолтный sink. Без него весь звук десктопа дублируется на планшете.
if ! pgrep -f 'nx_audio_rout[e]r' >/dev/null; then
  setsid python3 scripts/nx_audio_router.py >> /tmp/nx-audio-router.log 2>&1 < /dev/null &
fi
SIG=$(ls -t /run/user/1000/hypr/ 2>/dev/null | grep _ | head -1)
exec setsid env \
  XDG_RUNTIME_DIR=/run/user/1000 \
  WAYLAND_DISPLAY=wayland-1 \
  HYPRLAND_INSTANCE_SIGNATURE="$SIG" \
  NX_PIN_WAIT=1 \
  NX_BITRATE_KBPS=20000 \
  NX_ENCODER=vaapi \
  NX_AUDIO_DELAY_MS=80 \
  NX_AUDIO_BITRATE_KBPS=256 \
  NX_AUDIO_SOURCE=nx_stream.monitor \
  ./target/release/nx-server serve --name archpizdetc --pin 1234 \
  >> /tmp/nx-server.log 2>&1 < /dev/null
