# xHCI-контроллер хоста мёртв: диагностика и корневая причина

Дата: 2026-10-20. Хост: nx-host (Lenovo, AMD Carrizo APU, FCH xHCI 0000:00:10.0).

## Симптом

Планшет (стриминг-клиент, GSI Treble + Magisk + com.limelight) подключён к хосту по USB.
После серии сбоев RNDIS устройство перестало перечисляться: `lsusb` показывает только
шины 001 и 003 (EHCI), шины 002/004 (xHCI) отсутствуют.

## Факты (проверяемые команды)

```
$ sudo dmesg | grep -i xhci | tail -6
[450888.920398] pci 0000:00:10.0: xHCI HW did not halt within 32000 usec status = 0x0
[450888.920865] pci 0000:00:10.0: quirk_usb_early_handoff+0x0/0x810 took 32961 usecs
[450888.921206] xhci_hcd 0000:00:10.0: xHCI Host Controller
[450888.921219] xhci_hcd 0000:00:10.0: new USB bus registered, assigned bus number 2
[450889.002682] xhci_hcd 0000:00:10.0: Host halt failed, -110
[450889.002703] xhci_hcd 0000:00:10.0: probe with driver xhci_hcd failed with error -110
```

`status = 0x0` — контроллер не отвечает на обращения вообще (а не «медленно»).
Драйвер не забинден: `lspci -nnk -s 00:10.0` показывает `Kernel modules: xhci_pci`
без строки `Kernel driver in use`; `/sys/bus/pci/drivers/xhci_pci/` отсутствует.

## Что НЕ помогло (проверено)

| Средство | Результат |
|---|---|
| `modprobe xhci_pci` (probe) | -110, bus deregistered |
| unbind/bind драйвера | -110 |
| `echo 1 > .../0000:00:10.0/remove` + `echo 1 > /sys/bus/pci/rescan` | -110 (контроллер не отвечает) |
| `setpci` FLR | нет поддержки: `/sys/bus/pci/devices/0000:00:10.0/reset` отсутствует |
| reset PCIe-порта (`pcieport/reset`) | без эффекта (FCH — внутренняя шина) |
| **S3 suspend + resume** (`echo mem > /sys/power/state`, RTC-alarm 90 c как страховка) | S3 прошёл (`PM: suspend exit`), но контроллер остался мёртвым: probe снова -110 |

Итог: нужен настоящий power-cycle FCH → **ребут хоста**. Других путей нет.

## Корневая причина (не «xHCI сломался», а «кто его сломал»)

1. `rndis.gs4` на планшете отдаёт **два** USB-интерфейса:
   `0xE0` (Wireless, control-only) и `0x02` (CDC Data — реальный канал).
   На хосте это два netdev'а: `enp0s16u3` (0xE0) и `enp0s16u3i1` (0x02).
2. Старый `watchdog.py` вешал адрес хоста (`192.0.2.1/24`) на **захардкоженный**
   `enp0s16u3` — то есть на control-интерфейс, который данных не несёт.
   Следствие на планшете: `rndis0` RX packets == RX errs (100% ошибок),
   на хосте TX-очередь `rndis_host` забивается → `NETDEV WATCHDOG: transmit queue 0 timed out`
   каждые ~5 с.
3. На каждый фейл пинга watchdog делал `USBDEVFS_RESET` гаджета (кулдаун 90 с).
   За ночь набежало ~500 переподключений USB. Серия ре-Reset'ов + зависшая
   TX-очередь и положили xHCI: после этого контроллер перестал отвечать даже
   на probe.

**Проверка причинности:** до правки в `watchdog.py` адрес висел на 0xE0, после
правки IP переносится на интерфейс с `bInterfaceClass == 0x02`, control-интерфейс
очищается, а `USBDEVFS_RESET` ограничен бюджетом (4 сброса / 30 мин) и идёт
**после** мягкого восстановления (интерфейс/адрес/маршрут), а не вместо него.

## Что сделано в коде (2026-10-20)

* `nx-rust/watchdog.py`
  * `data_netdev()` / `tablet_netdevs()` — динамический выбор интерфейса с данными
    по `bInterfaceClass` (02 = CDC Data) вместо константы `IFACE`;
  * `ensure_host_ip()` — HOST_IP только на интерфейсе с данными, с control-интерфейса
    адрес снимается;
  * `soft_repair()` — починка адреса/маршрута/линка **без** USB-сброса;
  * `reset_budget_left()` — жёсткий бюджет `USBDEVFS_RESET` (4 / 30 мин);
  * сервер поднимается **всегда**, а не только при живом пинге к планшету.
* `nx-rust/scripts/tablet_bringup.sh` — bring-up после ребута: ждём перечисление,
  adb (с шимом `/tmp/adbshim/shim4.so`, если интерфейсов ff/42/01 больше одного),
  RNDIS на интерфейсе с данными, маршрут в таблице `rndis0` на планшете, установка
  Magisk-модуля `libui_gralloc_fix`, проверка CPU.
* `/etc/systemd/system/tablet-bringup.service` — автозапуск bring-up при загрузке.

## Важное: тестовый телефон — не стриминг-клиент

В ходе диагностики использовался отдельный тестовый телефон (adb по WiFi).
Moonlight на нём не стоял; все изменения на нём откатаны, adbd возвращён в
non-root. Планшет-клиент — другое устройство (GSI/Magisk/com.limelight),
именно он сидит на USB-порту с мёртвым xHCI.

## Следующий шаг

Ребут хоста → xHCI живой → планшет перечисляется → `tablet_bringup.sh all` →
проверка `rndis0` (errs == 0), установка `libui_gralloc_fix` (HW-декод) →
объективный замер end-to-end лага (ms_clock на HEADLESS-4 + скриншот клиента).
