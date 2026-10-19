#!/bin/bash
# Поднять nx-server с контекстом сессии et (Wayland/Hyprland).
# Используется watchdog'ом (restart_server) и вручную.
# setsid обязателен: без него процесс привязан к вызвавшему шеллу
# и умирает вместе с ним (наблюдалось 2026-10-18 02:57:23).
cd /home/et/nx-rust || exit 1
pkill -x nx-server 2>/dev/null
sleep 1
SIG=$(ls -t /run/user/1000/hypr/ 2>/dev/null | grep _ | head -1)
exec setsid env \
  XDG_RUNTIME_DIR=/run/user/1000 \
  WAYLAND_DISPLAY=wayland-1 \
  HYPRLAND_INSTANCE_SIGNATURE="$SIG" \
  NX_PIN_WAIT=1 \
  NX_BITRATE_KBPS=20000 \
  NX_ENCODER=vaapi \
  NX_AUDIO_DELAY_MS=80 \
  ./target/release/nx-server serve --name archpizdetc --pin 1234 \
  >> /tmp/nx-server.log 2>&1 < /dev/null
