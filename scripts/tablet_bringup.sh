#!/bin/bash
# Bring-up канала «планшет ↔ nx-server» со стороны хоста.
#
# Зачем: после ребута хоста (xHCI был убит ошибкой 100% RX на control-интерфейсе
# RNDIS) нужно за 1-2 минуты вернуть: adb по USB, RNDIS-канал с правильным
# интерфейсом (CDC Data, а не 0xE0 control), Magisk-модуль с патчем gralloc
# и проверку CPU-состояния планшета.
#
# Идемпотентно: можно запускать повторно, ничего не ломая.
#
#   ./tablet_bringup.sh wait     # ждать перечисления планшета + adb
#   ./tablet_bringup.sh net      # поднять RNDIS (адрес на интерфейсе с данными)
#   ./tablet_bringup.sh module   # поставить libui_gralloc_fix (нужен Magisk)
#   ./tablet_bringup.sh verify   # проверить CPU планшета
#   ./tablet_bringup.sh all      # всё по порядку

set -u
HERE="$(cd "$(dirname "$0")" && pwd)"
ROOT="$(dirname "$HERE")"
[ -r "${NX_CONFIG:-/etc/nx-rust/nx.env}" ] && . "${NX_CONFIG:-/etc/nx-rust/nx.env}"

SHIM=/tmp/adbshim/shim4.so
HOST_IP="${NX_HOST_IP:-192.168.8.1}"
TABLET_IP="${NX_TABLET_IP:-192.168.8.89}"
SUBNET="${NX_TABLET_SUBNET:-192.168.8.0/24}"
PREFIX="${SUBNET#*/}"
USB_VENDOR="${NX_USB_VENDOR:-0e8d}"
LIBUI_PATCHED=${LIBUI_PATCHED:-/tmp/libui_patched.so}
MK_MODULE=${MK_MODULE:-/tmp/mk_libui_module.sh}
LOG=/tmp/tablet_bringup.log

log() { echo "[$(date +%H:%M:%S)] $*" | tee -a "$LOG"; }

# ---------- adb ----------

adb_plain() { ADB_LIBUSB=1 adb "$@"; }

# Планшет отдаёт ДВА интерфейса ff/42/01 (rndis.gs4 и ffs.adb) — adb по
# умолчанию цепляется к первому (туннелю) и уходит в offline. Шим скрывает
# gs4-интерфейс от выбора adb. Работает только когда интерфейсов >= 2.
adb_shim() {
  ADB_LIBUSB=1 LD_PRELOAD="$SHIM" adb "$@"
}

adb_state() {
  adb devices 2>/dev/null | awk 'NR>1 && NF>1 {print $2; exit}'
}

adb_ready() { [ "$(adb_state)" = "device" ]; }

start_adb_with_shim() {
  log "перезапускаю adb с шимом (LD_PRELOAD=$SHIM)"
  adb kill-server >/dev/null 2>&1
  sleep 1
  ADB_LIBUSB=1 LD_PRELOAD="$SHIM" adb nodaemon server >/tmp/adb-shim.log 2>&1 &
  sleep 3
}

# ---------- USB-интерфейсы планшета ----------

tablet_usb_dir() {
  for idv in /sys/bus/usb/devices/*/idVendor; do
    [ -r "$idv" ] || continue
    [ "$(cat "$idv" 2>/dev/null | tr 'A-F' 'a-f')" = "$USB_VENDOR" ] || continue
    dirname "$idv"
    return 0
  done
  return 1
}

# Интерфейс, который реально несёт данные: CDC Data (bInterfaceClass=02).
# 0xE0 — control-only: адрес на нём даёт 100% RX-ошибок на rndis0 планшета.
data_netdev() {
  local devdir ifd cls
  devdir=$(tablet_usb_dir) || return 1
  for ifd in "$devdir":*; do
    [ -d "$ifd/net" ] || continue
    cls=$(cat "$ifd/bInterfaceClass" 2>/dev/null | tr 'A-F' 'a-f')
    if [ "$cls" = "02" ]; then ls "$ifd/net"; return 0; fi
  done
  for ifd in "$devdir":*; do
    [ -d "$ifd/net" ] || continue
    cls=$(cat "$ifd/bInterfaceClass" 2>/dev/null | tr 'A-F' 'a-f')
    [ "$cls" = "e0" ] && continue
    ls "$ifd/net"; return 0
  done
  for ifd in "$devdir":*; do
    [ -d "$ifd/net" ] || continue
    ls "$ifd/net"; return 0
  done
  return 1
}

all_netdevs() {
  local devdir ifd
  devdir=$(tablet_usb_dir) || return 1
  for ifd in "$devdir":*; do
    [ -d "$ifd/net" ] || continue
    ls "$ifd/net"
  done
}

# ---------- шаги ----------

cmd_wait() {
  local i
  for i in $(seq 1 60); do
    if lsusb 2>/dev/null | grep -qi "0e8d"; then
      log "планшет перечислен (lsusb)"
      break
    fi
    [ $((i % 5)) -eq 0 ] && log "жду планшет... ($i c)"
    sleep 5
  done
  lsusb 2>/dev/null | grep -i "0e8d" || { log "планшет не перечислился"; return 1; }

  # adb: сначала обычный, при offline/unauthorized — с шимом
  adb kill-server >/dev/null 2>&1; sleep 1; adb start-server >/dev/null 2>&1
  for i in $(seq 1 20); do
    adb_ready && { log "adb готов (обычный режим)"; return 0; }
    sleep 2
  done
  if [ -f "$SHIM" ]; then
    start_adb_with_shim
    for i in $(seq 1 20); do
      adb_ready && { log "adb готов (шим)"; return 0; }
      sleep 2
    done
  fi
  log "adb не поднялся: state=$(adb_state)"
  return 1
}

cmd_net() {
  local dev other
  dev=$(data_netdev) || { log "нет host-интерфейса данных"; return 1; }
  log "интерфейс с данными: $dev"
  sudo -n ip link set "$dev" up >/dev/null 2>&1
  for other in $(all_netdevs); do
    [ "$other" = "$dev" ] && continue
    if ip -br addr show "$other" 2>/dev/null | grep -q "$HOST_IP"; then
      sudo -n ip addr del "$HOST_IP/$PREFIX" dev "$other" && log "снял $HOST_IP с $other"
    fi
  done
  ip -br addr show "$dev" 2>/dev/null | grep -q "$HOST_IP" || \
    sudo -n ip addr add "$HOST_IP/$PREFIX" dev "$dev"
  ip -br addr show "$dev" | tee -a "$LOG"

  # сторона планшета: rndis0 + маршрут в таблице rndis0 (fwmark-трафик)
  adb_plain shell "ip link set rndis0 up; ip addr add $TABLET_IP/24 dev rndis0 2>/dev/null; ip route add $SUBNET dev rndis0 proto static scope link table rndis0 2>/dev/null; ip -br addr show rndis0" 2>&1 | tee -a "$LOG"
  sleep 1
  if ping -c 2 -W 1 "$TABLET_IP" >/dev/null 2>&1; then
    log "пинг $TABLET_IP — ok"
    return 0
  fi
  log "пинг $TABLET_IP — fails"
  return 1
}

cmd_module() {
  if adb_plain shell "su -c 'ls /data/adb/modules/libui_gralloc_fix/system/lib64/libui.so'" 2>/dev/null | grep -q libui.so; then
    log "модуль libui_gralloc_fix уже установлен"
    return 0
  fi
  [ -f "$LIBUI_PATCHED" ] || { log "нет $LIBUI_PATCHED"; return 1; }
  [ -f "$MK_MODULE" ] || { log "нет $MK_MODULE"; return 1; }
  adb_plain push "$LIBUI_PATCHED" /data/local/tmp/libui_patched.so 2>&1 | tee -a "$LOG"
  adb_plain push "$MK_MODULE" /data/local/tmp/mk_libui_module.sh 2>&1 | tee -a "$LOG"
  adb_plain shell "su -c 'sh /data/local/tmp/mk_libui_module.sh'" 2>&1 | tail -20 | tee -a "$LOG"
  log "модуль создан; нужен ребут планшета для вступления в силу"
}

cmd_gadget() {
  # Планшет после ребута поднимает только adb-only (0x201c) — RNDIS-функции
  # нет. Лечим configfs на планшете: classic rndis.gs4 + ffs.adb → 0x2005.
  if lsusb 2>/dev/null | grep -qi "0e8d:2005"; then
    log "гаджет уже 0x2005 (rndis+adb) — правка не нужна"
    return 0
  fi
  local src="$ROOT/scripts/fix_gadget3.sh"
  local dst="/data/local/tmp/fix_gadget3.sh"
  [ -f "$src" ] || { log "нет $src — гаджет не починю"; return 1; }
  adb_plain push "$src" "$dst" 2>&1 | tail -1 | tee -a "$LOG"
  log "правлю configfs гаджета на планшете"
  adb_plain shell "su -c 'sh $dst'" 2>&1 | tail -8 | tee -a "$LOG"
  sleep 6
  lsusb 2>/dev/null | grep -qi "0e8d:2005" && { log "гаджет 0x2005 — ок"; return 0; }
  log "гаджет остался $(lsusb 2>/dev/null | grep -i 0e8d | head -1)"
  return 1
}

cmd_verify() {
  log "CPU планшета:"
  adb_plain shell 'cat /sys/devices/system/cpu/online; for c in 0 1 2 3 4 5 6 7; do printf "cpu%s: " $c; cat /sys/devices/system/cpu/cpu$c/cpufreq/scaling_governor 2>/dev/null || echo n/a; done' 2>&1 | tee -a "$LOG"
  log "модули Magisk:"
  adb_plain shell "su -c 'ls /data/adb/modules'" 2>&1 | tee -a "$LOG"
  log "HW-декодер:"
  adb_plain shell "dumpsys media.player 2>/dev/null | grep -i -m5 'OMX.MTK.VIDEO.DECODER'" 2>&1 | tee -a "$LOG"
}

case "${1:-all}" in
  wait)   cmd_wait ;;
  gadget) cmd_gadget ;;
  net)    cmd_net ;;
  module) cmd_module ;;
  verify) cmd_verify ;;
  all)
    cmd_wait && cmd_gadget && cmd_net && cmd_verify
    ;;
  *)
    echo "usage: $0 {wait|gadget|net|module|verify|all}"; exit 2 ;;
esac
