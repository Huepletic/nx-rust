# Заряд от USB-порта ПК: usb_unlimited (2026-10-23)

## Симптом
Планшет (Treble GSI, MTK, mt6370 PMIC + SGM41513) не заряжается от USB-A
порта ПК: `status: Not charging`, `CHG_EN=0`, charge_counter стоит.
Заряд от сетевой зарядки при этом работает.

## Причина (дизассемблировано по исходникам MTK 4.9/4.14)
1. Хост = BC1.2 STANDARD_HOST → в `mtk_dual_switch_charging.c`
   `dual_swchg_select_charging_current_limit()` берёт лимиты из DT-ветки
   `usb_charger_current*` (узел `/charger`):
   - `usb_charger_current_suspend = 0` (!)
   - `usb_charger_current_unconfigured = 70 мА`
   - `usb_charger_current_configured = 500 мА`
2. USB-статус гаджета приходит из `BATTERY_SetUSBState` (framework).
   При переходах гаджета (adb root, пере-enumeration) статус может
   зависнуть в `USB_SUSPEND` → input limit = **0 мА** →
   `chg1's aicr is set to 0mA, turn off` → CHG_EN=0.
3. Дополнительно: через 12 ч (HW safety timer mt6370, DT
   `safety_timer=12`) заряд блокируется флагом `safety_timeout=true`;
   флаг сбрасывается ТОЛЬКО при plug_in (переподключение кабеля).
   До сброса алгоритм не включит заряд вообще (`tmp:… s:1` в dmesg).

## Фикс
`/proc/mtk_battery_cmd/current_cmd` принимает `«<usb_unlimited> <cmd_discharging>»`.
`usb_unlimited=1` уводит алгоритм на ветку сетевой зарядки
(`ac_charger_input_current=2 А`) ДО проверки usb_state → заряд включается,
даже когда framework держит USB_SUSPEND. Регистры:
`ICHG=2 А, AICR=2 А (реально ограничено портом), CHG_EN=1, progress`.

Замеры при работающем стриме Moonlight (1088x800@60, 16 Мбит/с):
- `CURRENT_NOW` 514–1515 мА (заряд), уровень 77→80% за ~15 мин;
- при зарядке «не идёт» — 0 мА и стоящий charge_counter.

Ядро сбрасывает `usb_unlimited` при plug_out, поэтому флаг поднимается
каждый цикл модулем `nx-tablet-power` (`force_unlimited_charging()` в
`scripts/tablet_power.py`, сервис `nx-tablet-power.service`).

## Что НЕ решено
- Ток ограничен бюджетом порта USB-A хоста (~500 мА без RNDIS,
  до ~1.5 А по VBUS при согласовании); реально измерено 0.5–1.5 А.
- HW safety timer: через 12 ч непрерывной зарядки флаг `safety_timeout`
  снова заблокирует заряд до переподключения кабеля. Перезагрузка
  планшета или передёргивание кабеля сбрасывает.
