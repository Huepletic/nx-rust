# Планшет: разряд на полке (P1) — 2026-10-27

## Симптом

Планшет, оставленный НЕподключённым на полке неделю, разрядился в ноль.
Средний фоновый расход: **~34 мА** (7450 мАч / 65.4 ч из batterystats),
экран ни разу не включался.

## Диагноз (данные, не догадки)

Снято с планшета в день инцидента:

- `dumpsys batterystats --charged`: Time on battery 2д 17ч 14м, screen on 0%.
  Все userspace wakelock'и суммарно **11 мин 5 с** за 65 ч — apps ни при чём.
- `Total partial wakelock time: 11m 4s`; главный держатель — uid 1000 (system),
  мелочь. Приложения (включая com.limelight) — доли процента.
- WiFi выключен (`wifi_on=0`), Bluetooth выключен, SIM нет, сотовый в OOS 100%
  (`Cellular kernel active time: 0ms`).
- **Ядро ни разу не заснуло**: `/sys/power/suspend_stats/success = 0` за всё время.
- SPM (MTK System Power Manager) каждые 5 с пишет:
  `Power/swap DP: No enter --- SODI3: No enter --- SODI: No enter` —
  ни deep idle, ни SODI не достигаются.
- `mtk_charger_alarm_timer_func: not suspend, wake up charger` — зарядный
  alarm будит систему каждые ~10 с.
- **CPU governors = performance** на обоих кластерах (2.1/2.0 ГГц) при
  выключенном экране: vendor.mtkpower@1.0-service не отдаёт перформанс-моду
  после завершения стрима.
- kworker'ы в сумме >100% CPU; найден и убит спин `dnsmasq` tethering'а:
  **accept4-спин 100% CPU** (34613 failed accept4 за 4 с; 31 мин CPU).
- Doze-сенсоры Treble GSI (me.phh.treble.app Doze) слушали accelerometer
  125 Гц даже с выключенным экраном → отключено (key_doze_handwave/pocket=false
  в /data/data/me.phh.treble.app/shared_prefs/).

Итог: планшет никогда не спит + CPU на максимальной частоте = 34 мА.

## Fix

### nx-shelf-daemon.sh (на планшете, `05767d9`)

Root-демон в `/data/local/tmp/nx-shelf-daemon.sh` (репо: `tablet/`):

- Каждые 15 с проверяет `/sys/class/power_supply/usb/online`.
- USB пропал >90 с → `schedutil` на policy0/policy4 + kill dnsmasq.
- USB вернулся → `performance` обратно (стриму нужны частоты).
- Лог: `/data/local/tmp/nx-shelf-daemon.log`.

Запуск (adbd уже root на userdebug):

```bash
adb push tablet/nx-shelf-daemon.sh /data/local/tmp/
adb shell chmod +x /data/local/tmp/nx-shelf-daemon.sh
adb shell 'toybox setsid -d /data/local/tmp/nx-shelf-daemon.sh </dev/null >/dev/null 2>&1'
```

Внимание: процесс виден в `ps` как `sh` с ppid 1, не как «nx-shelf-daemon».
Живость проверять по свежей метке в лог-файле.

### Что демон НЕ решает

Главный расход — **отсутствие suspend** (success=0, SPM «No enter»).
schedutil снизит частоту, но полностью проблему «полки» не закроет: даже
на низкой частоте SoC без suspend ест заметно больше, чем должен. Оценка
после демона: ~34 мА → ~12–20 мА (месяц вместо недели). Полный fix требует
разбора, почему PM не уходит в suspend (подозреваемые: USB-гаджет в
configured state, mtk_charger alarm, thermal polling `mt-i2c` IRQ,
`stk3x3x` ALS/PS poll). Это следующий шаг, если неделя на schedutil не устроят.

## Попутно найдено и починено

- Watchdog ребутал планшет на заряде 1% в момент, когда юзер включал
  USB tethering (`222d0c1`): gates в `reboot_tablet()` — пауза 8 с,
  boot_completed, экран горит → не ребутаем, заряд <10% → не ребутаем.
- «Failed to start Desktop (error 400)» после серии ребутов — nx-server
  залип в старой RTSP-сессии и отвечал на `/resume` честным
  «resume not yet implemented» (`src/nvhttp/mod.rs:490`). Лечение —
  рестарт nx-server через `launch_nx.sh` (без pkill). Кандидат на future
  fix: реализовать `/resume` (переподнять pipeline) или `/quit` сброс.
