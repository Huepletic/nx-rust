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
info "XDG_SESSION_TYPE=${XDG_SESSION_TYPE:-?}  XDG_CURRENT_DESKTOP=${XDG_CURRENT_DESKTOP:-?}  WAYLAND_DISPLAY=${WAYLAND_DISPLAY:-?}"
if [ "${XDG_SESSION_TYPE:-}" = "wayland" ] || [ -n "${WAYLAND_DISPLAY:-}" ]; then
  ok "Wayland-сессия"
else
  fail "нет Wayland-сессии (сервер работает только через wlr-screencopy)"
fi

if command -v wayland-info >/dev/null 2>&1; then
  if wayland-info 2>/dev/null | grep -q zwlr_screencopy_manager_v1; then
    ok "композитор отдаёт zwlr_screencopy_manager_v1"
  else
    fail "нет zwlr_screencopy_manager_v1 — захват не поддерживается (KDE/GNOME?)"
  fi
elif command -v hyprctl >/dev/null 2>&1; then
  ok "hyprctl найден (протокол не проверить: установите wayland-utils)"
else
  fail "нет wayland-info и hyprctl — compositor не проверить"
fi

case "${XDG_CURRENT_DESKTOP:-}" in
  *KDE*|*Plasma*)
    fail "KDE/KWin не реализует wlr-screencopy: нужен wlroots-композитор (Hyprland/Sway)"
    ;;
  *GNOME*)
    fail "GNOME/Mutter не реализует wlr-screencopy: нужен wlroots-композитор"
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
  fail "нет /dev/dri/renderD128 (нужен VA-API render node)"
fi
if command -v vainfo >/dev/null 2>&1; then
  if vainfo 2>/dev/null | grep -q 'VAProfileH264High.*VAEntrypointEncSlice'; then
    ok "VA-API H.264 High encode"
  else
    fail "VA-API не умеет H.264 High encode"
  fi
else
  warn "vainfo не установлен (пакет libva-utils)"
fi
if command -v ffmpeg >/dev/null 2>&1; then
  if ffmpeg -hide_banner -encoders 2>/dev/null | grep -q h264_vaapi; then
    ok "ffmpeg умеет h264_vaapi"
  else
    fail "ffmpeg без h264_vaapi"
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
