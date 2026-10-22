#!/bin/bash
# nx-rust doctor — read-only проверка окружения.
#
# Запуск: ./scripts/doctor.sh
# Ничего не меняет: не создаёт файлов, не перезапускает сервисы, не трогает
# планшет. Только читает и печатает вердикт. Exit 1, если есть FAIL.
set -u

cd "$(dirname "$0")/.." || exit 1

OK=0
WARN=0
FAIL=0

ok()   { printf '  \033[32mOK\033[0m    %s\n' "$*"; OK=$((OK + 1)); }
warn() { printf '  \033[33mWARN\033[0m  %s\n' "$*"; WARN=$((WARN + 1)); }
fail() { printf '  \033[31mFAIL\033[0m  %s\n' "$*"; FAIL=$((FAIL + 1)); }
info() { printf '  info  %s\n' "$*"; }
sec()  { printf '\n\033[1m%s\033[0m\n' "$*"; }

sec "1. Репозиторий и бинарник"
if [ -f target/release/nx-server ]; then
  ok "target/release/nx-server собран"
else
  fail "нет target/release/nx-server — выполните: cargo build --release"
fi
if [ -f Cargo.toml ] && [ -d src ]; then
  ok "исходники на месте ($(pwd))"
else
  fail "запустите из корня репозитория nx-rust"
fi

sec "2. Сессия и композитор"
info "XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-?}  XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-?}  WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-?}  DISPLAY=${DISPLAY:-?}"
BACKEND=""
if [ -x target/release/nx-server ]; then
  BACKEND=$(target/release/nx-server backend 2>/dev/null || true)
fi
if [ -n "$BACKEND" ]; then
  ok "backend: $BACKEND"
else
  warn "backend не определён (бинарник старый/не собран) — проверяю окружение"
fi
case "$BACKEND" in
  "xdg-desktop-portal/PipeWire")
    if command -v busctl >/dev/null 2>&1 && \
       busctl --user introspect org.freedesktop.portal.Desktop /org/freedesktop/portal/desktop org.freedesktop.portal.ScreenCast >/dev/null 2>&1; then
      ok "портал ScreenCast доступен (org.freedesktop.portal.Desktop)"
    else
      fail "нет ScreenCast в xdg-desktop-portal (нужен xdg-desktop-portal-kde/gnome)"
    fi
    if pgrep -x pipewire >/dev/null 2>&1; then
      ok "PipeWire работает"
    else
      fail "PipeWire не запущен (портал ScreenCast не отдаст кадры)"
    fi
    if command -v pkg-config >/dev/null 2>&1 && pkg-config --exists libpipewire-0.3; then
      ok "libpipewire-0.3 (dev) есть"
    else
      warn "нет libpipewire-0.3 dev — сборка с порталом не пройдёт"
    fi
    ;;
  "x11grab")
    if [ -n "${DISPLAY:-}" ]; then
      ok "X11 DISPLAY=$DISPLAY"
    else
      fail "нет DISPLAY"
    fi
    if command -v xrandr >/dev/null 2>&1 && xrandr --current >/dev/null 2>&1; then
      ok "xrandr отвечает"
    else
      fail "xrandr не отвечает"
    fi
    if command -v ffmpeg >/dev/null 2>&1 && ffmpeg -hide_banner -devices 2>/dev/null | grep -q x11grab; then
      ok "ffmpeg умеет x11grab"
    else
      fail "ffmpeg без x11grab (нужен ffmpeg с libxcb)"
    fi
    ;;
  "wlr-screencopy")
    if command -v wayland-info >/dev/null 2>&1; then
      if wayland-info 2>/dev/null | grep -q zwlr_screencopy_manager_v1; then
        ok "композитор отдаёт zwlr_screencopy_manager_v1"
      else
        fail "нет zwlr_screencopy_manager_v1"
      fi
    elif command -v hyprctl >/dev/null 2>&1; then
      ok "hyprctl найден (протокол не проверить: установите wayland-utils)"
    else
      warn "нет wayland-info/hyprctl — протокол не проверить"
    fi
    ;;
  *)
    if [ "${XDG_SESSION_TYPE:-}" = "wayland" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
      ok "Wayland-сессия"
    elif [ -n "${DISPLAY:-}" ]; then
      ok "X11-сессия"
    else
      fail "нет ни Wayland, ни X11"
    fi
    ;;
esac
if command -v hyprctl >/dev/null 2>&1; then
  if hyprctl monitors -j 2>/dev/null | grep -q 'HEADLESS-'; then
    ok "виртуальный вывод HEADLESS-* уже есть"
  else
    info "HEADLESS-* нет — сервер создаст его сам через hyprctl"
  fi
fi

sec "3. VA-API / энкодер"
if [ -e /dev/dri/renderD128 ]; then
  ok "/dev/dri/renderD128 доступен"
else
  warn "нет /dev/dri/renderD128 — будет libx264 (медленнее; NX_FFMPEG_ENCODER=x264)"
fi
if command -v vainfo >/dev/null 2>&1; then
  if vainfo 2>/dev/null | grep -q 'VAProfileH264High.*VAEntrypointEncSlice'; then
    ok "VA-API H.264 High encode"
  else
    warn "VA-API не умеет H.264 High encode — нужен x264 fallback"
  fi
else
  warn "vainfo не установлен (пакет libva-utils)"
fi
if command -v ffmpeg >/dev/null 2>&1; then
  encs=$(ffmpeg -hide_banner -encoders 2>/dev/null || true)
  if echo "$encs" | grep -q h264_vaapi; then
    ok "ffmpeg умеет h264_vaapi"
  elif echo "$encs" | grep -q libx264; then
    warn "ffmpeg без h264_vaapi — будет libx264"
  else
    fail "ffmpeg без h264_vaapi/libx264"
  fi
else
  fail "ffmpeg не установлен"
fi

sec "4. Звук"
if command -v pactl >/dev/null 2>&1; then
  if pactl info >/dev/null 2>&1; then
    ok "PulseAudio/PipeWire отвечает"
    if pactl list short sinks 2>/dev/null | awk -F'\t' '$2=="nx_stream"{f=1} END{exit !f}'; then
      ok "sink nx_stream есть"
      if pactl list short sources 2>/dev/null | grep -q 'nx_stream.monitor'; then
        ok "source nx_stream.monitor есть"
      else
        warn "нет nx_stream.monitor — launch_nx.sh создаст sink/keepalive"
      fi
    else
      info "sink nx_stream нет — launch_nx.sh создаст; или используйте monitor дефолтного sink'а"
    fi
  else
    fail "pactl не подключается к звуковому серверу"
  fi
else
  fail "pactl не установлен (pipewire-pulse/pulseaudio)"
fi

sec "5. Порты"
check_port() {
  local p="$1" name="$2"
  if ss -H -ltnu 2>/dev/null | awk '{print $5}' | grep -qE "[:.]$p$"; then
    if ss -H -ltnup 2>/dev/null | grep -E "[:.]$p " | grep -q nx-server; then
      ok "$name :$p — nx-server"
    elif [ "$p" = "5353" ]; then
      info "$name :$p занят (mDNS; SO_REUSEPORT — это норма)"
    else
      warn "$name :$p занят другим процессом"
    fi
  else
    info "$name :$p свободен"
  fi
}
check_port 48989 "HTTP"
check_port 48984 "HTTPS"
check_port 49010 "RTSP"
check_port 49000 "video RTP"
check_port 49001 "control ENet"
check_port 49002 "audio RTP"
check_port 5353  "mDNS"

sec "6. Состояние и опциональное"
STATE_DIR="${XDG_STATE_HOME:-$HOME/.local/state}/nx-rust"
if [ -d "$STATE_DIR" ]; then
  ok "state dir: $STATE_DIR"
elif [ -w "$(dirname "$STATE_DIR")" ]; then
  info "state dir будет создан: $STATE_DIR"
else
  fail "нет доступа к $(dirname "$STATE_DIR")"
fi

if command -v adb >/dev/null 2>&1; then
  info "adb есть (планшет: adb devices — запускать вручную)"
else
  info "adb нет — USB-режим/watchdog недоступны, WiFi-режим работает"
fi

printf '\n\033[1mИтог:\033[0m OK=%d, WARN=%d, FAIL=%d\n' "$OK" "$WARN" "$FAIL"
if [ "$FAIL" -gt 0 ]; then
  printf 'Есть блокеры. Смотрите README → «Совместимость» и «Troubleshooting».\n'
  exit 1
fi
printf 'Блокеров нет. Если что-то не работает — README → Troubleshooting.\n'
