# Разряд планшета на полке: 11%/день при выключенном экране

Дата: 2026-10-27. Планшет Ri0T1100PK0008473, Treble GSI Android 11 (userdebug),
MT8788 (mt6771), ядро 4.14.186, Magisk 29.

## Симптом

Планшет, оставленный НЕподключённым на полке неделю, разрядился в ноль.
Batterystats (2d17h, снято в `bs.txt`): 7450 мАч (100%) сгорело на «idle» —
**34 мА среднего фонового расхода**. Экран off 100% времени, WiFi off,
BT off, сотовый вне сети, wakelock'и приложений — 11 мин за 65 ч.
Радио и приложения НЕ виноваты.

## Корень (доказательства)

1. **Uptime == realtime на батарее 65.4 ч** → ядро ни разу не ушло в suspend
   за всё время на полке. `/sys/power/suspend_stats/success = 0`.
   dmesg каждые ~5 с: `Power/swap DP: No enter --- SODI3: No enter --- SODI: No enter`,
   `mtk_charger_alarm_timer_func: not suspend, wake up charger` каждые 10 с.
2. **CPU governor = performance на всех 8 ядрах** — ставил наш же Magisk-модуль
   `cpu_perf` (v2) на каждой загрузке. Ядра не даунклочатся, idle-потребление
   SoC максимальное.
3. **getgameserver** (vendor MTK, `/vendor/bin/getgameserver`, capabilities
   NET_ADMIN/NET_RAW) держит AF_PACKET-сокет (`tpacket_rcv` на ALL, `/proc/net/packet`)
   и гоняет `eth_work` по каждому пакету RNDIS-линка. ftrace (workqueue_execute_start,
   3 с окно): **208 001 вызовов eth_work**, kworkers (`kworker/1:0`, `/0:0`, `/3:1`,
   `/2:2`) по 100% CPU, loadavg 31. `strace` dnsmasq (tethering): accept4-спин
   10420 EINVAL/с, 3917 pselect6/с — второй фоновой житель CPU.
4. **phh Treble Doze** (handwave/pocket) держал PROXIMITY-сенсор активным на
   экране-off (`me.phh.treble.app.Doze,proximity @1000ms`).

## Почему suspend не наступал — проверка вручную

`echo mem > /sys/power/state` (root): попытки ЕСТЬ, но `fail` растёт,
`last_failed_step = suspend_noirq`, активные wakeup-источники в момент отказа:
`musb-hdrc` (USB-гаджет), `NETLINK` (rtnl-сокеты dnsmasq/getgameserver),
`pri_disp_wakelock` (экран). Т.е. **framework suspend-путь живой**; пока кабель
в порту — musb-hdrc легально держит систему. На полке (кабель вынут) остаются
NETLINK-держатели — их убирает демон (ниже).

## Исправление (host-независимое, живёт на планшете)

Magisk-модуль `cpu_perf` v3 (`/data/adb/modules/cpu_perf/service.sh`,
бэкап v2 рядом: `service.sh.v2.bak`). После каждой загрузки:

- `stop getgameserver` — init не рестартит остановленный; eth_work-шторм гаснет.
- Демон-полка (цикл 20 с):
  - USB подключён ИЛИ жив `com.limelight` → governor `performance` (стриму нужен);
  - иначе (полка) → governor `schedutil` (ядра даунклочатся) + `dnsmasq` под нож
    (tethering без хоста не нужен, netd сам перезапустит при USB).
- Прочие твики стрима без изменений: 8 ядер online, thermal-триппоинты подняты,
  deadline I/O, VM/net sysctl, density 320.

Заменён временный `/data/local/tmp/nx-shelf-daemon.sh` (переименован в
`.disabled`) — два демона дрались бы за governor.

phh Doze-сенсоры (handwave/pocket) выключены в
`/data/data/me.phh.treble.app/shared_prefs/me.phh.treble.app_preferences.xml`
(force-stop TrebleApp; proximity-соединение из sensorservice пропало).

## Проверка после ремонта (USB подключён, Limelight жив)

- CPU по `/proc/stat` за 4 с: ~26% одного ядра (было: kworkers 4×100%, sys 155%).
- eth_work в чистом 3-с ftrace-окне: шторм ушёл вместе с getgameserver.
- governor: performance (корректно: стрим/экран активны).
- `getgameserver`: `init.svc = stopped`, процесса нет.

## Что осталось проверить (нужен пользователь / отсоединение кабеля)

1. Отсоединить USB при ≥30% заряда → подождать 90 c (демон: schedutil +
   kill dnsmasq) → оставить на 2–3 ч → проверить процент за час и
   `suspend_stats/success > 0` при следующем подключении.
2. Если успех не растёт — смотреть `dmesg | grep suspend_noirq` и активные
   wakeup-источники (`/proc/wakeup_sources` недоступен на этом ядре;
   брать из dmesg-строк `active wakeup source:`).
