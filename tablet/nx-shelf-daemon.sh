#!/system/bin/sh
# nx-shelf-daemon: экономия батареи, когда планшет лежит на полке (USB отключён).
#
# Диагноз 2026-10-27: планшет не входит в suspend (suspend_stats/success=0),
# CPU governors = performance (mtkpower HAL не отдаёт перформанс после стрима),
# dnsmasq tethering'а крутится в accept4-спине 100% CPU. Итог — ~34 мА фона,
# 7450 мАч сгорает за неделю. Root-демон на планшете это чинит БЕЗ хоста.
#
# Правило: пока USB подключён — не мешаем (стриму нужен performance).
# USB пропал → ждём QUIET сек (пользователь мог выдернуть кабель, чтобы
# переподключить) → глушим фон → USB вернулся → восстанавливаем.

LOG=/data/local/tmp/nx-shelf-daemon.log

log() { echo "[$(date '+%m-%d %H:%M:%S')] $*" >> "$LOG"; }

GOV0=/sys/devices/system/cpu/cpufreq/policy0/scaling_governor
GOV4=/sys/devices/system/cpu/cpufreq/policy4/scaling_governor
SHELF_GOV=schedutil

usb_connected() {
  # /sys/class/power_supply/usb/online: 1 = кабель в порту
  [ "$(cat /sys/class/power_supply/usb/online 2>/dev/null)" = "1" ]
}

set_gov() {
  echo "$1" > "$GOV0" 2>/dev/null
  echo "$1" > "$GOV4" 2>/dev/null
}

get_gov() { cat "$GOV0" 2>/dev/null; }

# dnsmasq tethering'а: при выключенном экране и живом USB-гаджете крутится
# accept4-спином на 100% CPU (2026-10-27: 31 мин CPU за 50 мин). Если USB
# отключён — он не нужен вовсе; если подключён — netd сам его перезапустит.
kill_dnsmasq() {
  local p
  p=$(ps -A -o PID,NAME | awk '$2=="dnsmasq"{print $1; exit}')
  if [ -n "$p" ] && [ "$p" != "Z" ]; then
    kill "$p" 2>/dev/null
    log "dnsmasq ($p) убит (tethering без хоста не нужен)"
  fi
}

enter_shelf() {
  log "ушёл в полку: governors $SHELF_GOV (был $(get_gov)), dnsmasq под нож"
  set_gov "$SHELF_GOV"
  kill_dnsmasq
}

exit_shelf() {
  log "USB вернулся: governors performance, стрим важнее"
  set_gov performance
}

QUIET=90          # сек без USB, прежде чем глушить
PREV_USB=1
ABSENT_SINCE=0

log "запущен (pid $$)"
while true; do
  if usb_connected; then
    ABSENT_SINCE=0
    if [ "$PREV_USB" = "0" ]; then
      exit_shelf
      PREV_USB=1
    fi
  else
    if [ "$PREV_USB" = "1" ]; then
      ABSENT_SINCE=$(cat /proc/uptime | cut -d' ' -f1 | cut -d. -f1)
      PREV_USB=0
      log "USB пропал — таймер $QUIET c"
    fi
    NOW=$(cat /proc/uptime | cut -d' ' -f1 | cut -d. -f1)
    AGE=$((NOW - ABSENT_SINCE))
    if [ "$AGE" -ge "$QUIET" ] && [ "$(get_gov)" != "$SHELF_GOV" ]; then
      enter_shelf
    fi
  fi
  sleep 15
done
