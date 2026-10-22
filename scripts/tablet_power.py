#!/usr/bin/env python3
# nx-tablet-power: модуль питания планшета (Moonlight-клиент com.limelight).
#
# Задача (2026-10-22): «планшет не заряжается, пока им не пользуешься, что-то
# жрёт батарею; нужен какой-нибудь модуль».
#
# Что выяснено замерами на живом планшете (Treble GSI, Android 11 userdebug,
# ядро 4.14.186 MTK, зарядка mt6370 PMIC + внешняя микросхема SGM41513):
#
#  A. «Жрёт батарею» = висящий в фоне Limelight. Замер по дампу регистров
#     зарядчика (IBUS — ток, взятый с порта хоста):
#        фон: экран выкл, WiFi выкл, doze ......... 300 мА
#        + dumpsys deviceidle force-idle ......... 250 мА
#        + am force-stop com.limelight .......... 100 мА   <- минус ~150 мА
#     Приложение в фоне ест столько же, сколько вся остальная система.
#
#  B. «Не заряжается» — зарядку отключает САМ драйвер планшета, не мы:
#        mtk_dual_switch_charging_run
#          -> mtk_pe_check_charger: stop, PE+ is disabled
#          -> sgm41513_enable_charging: disable charge     (CHG_EN=0, IBAT=0)
#     Железо настроено верно: ICHG=500мA, AICR=500мA, CV=4.35В, IEOC=250мА,
#     VBUS=4.95В (питание есть). Но порт хоста — обычный USB-A: 500 мА
#     (bMaxPower=500mA у обоих сторон), а драйвер программирует ток заряда
#     РАВНЫМ всему бюджету порта, тогда как сам планшет ест 100-400 мА —
#     места не остаётся, и заряд выключается. Проверено: charge_counter стоит
#     (6258000 мА·ч), status=4 (NOT_CHARGING), VBAT=4.13В (батарея не полная).
#     В DT зашито: usb_charger_current=500мА, charging_host_charger_current=1.5А,
#     ac_charger_current=2А, ta_ac_charger_current=3А, enable_type_c=1.
#     Т.е. от сетевой зарядки (или Type-C с PE+) планшет берёт ток нормально,
#     от порта ПК — нет. Никакими нашими правками это не лечится: ограничение
#     на стороне железа+драйвера планшета и на стороне порта хоста.
#
#  C. Планшет не засыпает: зарядчик будит систему каждые 10 с
#     (mtk_charger_alarm_timer_func: not suspend, wake up charger), deviceidle
#     остаётся в ACTIVE. Поэтому «idle» на планшете — это 100-400 мА.
#
# Что делает модуль:
#   1) гасит Limelight, когда стрим не идёт (экономит ~150 мА — главный пункт);
#   2) следит за зарядом (charge_counter, ток, статус, CHG_EN) и пишет
#      /tmp/tablet_power.json;
#   3) если планшет idle, не заряжается и батарея ниже порога — поднимает
#      уведомление с ТОЧНОЙ причиной;
#   4) ловит момент, когда подключили нормальную зарядку: usb/current_max
#      перестаёт быть 500 мА -> проверяем, что заряд пошёл, и пишем скорость.
#
# Табу: ничего не пишем в БД, не перезагружаем планшет, не трогаем RNDIS
# (только чтение), не меняем настройки хоста. Пользовательские приложения
# планшета не убиваем, кроме com.limelight и только когда стрим не идёт.

import json
import os
import pwd
import re
import subprocess
import sys
import time


# ── Конфигурация ────────────────────────────────────────────────────────────
# Переопределяется окружением или /etc/nx-rust/nx.env (пример —
# config/nx.env.example). Окружение имеет приоритет над файлом.
def _load_config(path):
    try:
        with open(path, encoding="utf-8") as f:
            for line in f:
                line = line.strip()
                if not line or line.startswith("#") or "=" not in line:
                    continue
                key, val = line.split("=", 1)
                os.environ.setdefault(key.strip(), val.strip().strip("'\""))
    except OSError:
        pass


_load_config(os.environ.get("NX_CONFIG", "/etc/nx-rust/nx.env"))

_REPO = os.path.dirname(os.path.dirname(os.path.abspath(__file__)))
try:
    _pw = pwd.getpwuid(os.stat(_REPO).st_uid)
    _DEF_USER, _DEF_UID = _pw.pw_name, _pw.pw_uid
except OSError:
    _DEF_USER, _DEF_UID = "nx", 1000
USER_NAME = os.environ.get("NX_USER", _DEF_USER)
USER_UID = int(os.environ.get("NX_UID", str(_DEF_UID)))
USER_RUNTIME = "/run/user/%d" % USER_UID

LOG = os.environ.get("NX_POWER_LOG", "/var/log/nx-tablet-power.log")
# /run, а не /tmp: сервис работает от root, а на этом хосте root не может
# перезаписать файл, принадлежащий пользователю (fs.protected_regular).
STATE = os.environ.get("NX_POWER_STATE", "/run/tablet_power.json")

PERIOD = 60              # сек между опросами
APP = "com.limelight"
PORT_USB2_MA = 500       # бюджет обычного USB-A порта (bMaxPower хоста)
LOW_BATTERY = 60         # ниже этого уровня — поднимаем тревогу
NOTIFY_COOLDOWN = 1800   # не спамить уведомлениями чаще раза в 30 мин
IDLE_BEFORE_STOP = 120   # столько секунд приложение должно висеть в фоне,
                         # прежде чем мы его погасим (не мешаем watchdog'у,
                         # который в этот момент может переподключать стрим)
# Правило пользователя (2026-10-22): в пассивном режиме планшет должен
# заряжаться со скоростью не меньше 50% от того, с какой он разряжается
# в активном режиме. Скорость берём из тока батареи (current_now, мкА:
# отрицательный = разряд, положительный = заряд), а «активный» расход
# запоминаем как максимум разряда за последнее окно.
PASSIVE_MIN_RATIO = 0.5
DISCHARGE_MIN_MA = 50      # ниже этого шум АЦП, активным расходом не считаем
DISCHARGE_WINDOW = 4 * 3600   # окно памяти об активном расходе, с


_state = {}              # последнее состояние
_last_cc = None          # предыдущий charge_counter
_last_cc_ts = 0.0
_last_notify = 0.0
_bg_since = 0.0          # с какого момента приложение в фоне
_stopped_at = 0.0        # когда последний раз гасили приложение
_peak_discharge_ma = 0.0  # максимум разряда батареи за окно (мА)
_peak_discharge_ts = 0.0
_passive_ma = 0.0        # последняя измеренная скорость заряда в простое (мА)


def log(msg):
    line = "[%s] %s" % (time.strftime("%Y-%m-%d %H:%M:%S"), msg)
    for path in (LOG, "/tmp/nx-tablet-power.log"):
        try:
            with open(path, "a") as f:
                f.write(line + "\n")
            break
        except Exception:
            continue
    print(line, flush=True)


def sh(cmd, timeout=25):
    """Синхронный shell. Возвращает (rc, output)."""
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True,
                           timeout=timeout)
        return r.returncode, (r.stdout + r.stderr).strip()
    except subprocess.TimeoutExpired:
        return -1, "timeout"
    except Exception as e:  # noqa: BLE001
        return -1, str(e)


def adb(cmd, timeout=25):
    return sh('adb shell "%s"' % cmd, timeout=timeout)


# --------------------------------------------------------------------------
# Состояние планшета — одним заходом на планшет (adb shell), чтобы не
# плодить десятки adb-вызовов каждые 60 с.
# --------------------------------------------------------------------------
STATE_SCRIPT = r"""
b=/sys/class/power_supply/battery
u=/sys/class/power_supply/usb
echo "L=$(cat $b/capacity 2>/dev/null)"
echo "S=$(cat $b/status 2>/dev/null)"
echo "I=$(cat $b/current_now 2>/dev/null)"
echo "CC=$(cat $b/charge_counter 2>/dev/null)"
echo "V=$(cat $b/voltage_now 2>/dev/null)"
echo "UMAX=$(cat $u/current_max 2>/dev/null)"
echo "UON=$(cat $u/online 2>/dev/null)"
echo "W=$(dumpsys power 2>/dev/null | grep -m1 mWakefulness= | cut -d= -f2 | tr -d ' \r')"
echo "F=$(dumpsys window 2>/dev/null | grep -m1 mCurrentFocus= | cut -d= -f2 | tr -d ' \r')"
echo "P=$(pidof com.limelight 2>/dev/null | tr -d ' \r')"
echo "D=$(dmesg 2>/dev/null | grep mt6370_dump_register | tail -4 | tr '\n' ' ')"
"""


def _num(txt):
    try:
        return int(str(txt).strip())
    except (TypeError, ValueError):
        return None


def tablet_state():
    """Собирает состояние планшета. None — планшет недоступен."""
    rc, out = sh("adb shell '%s'" % STATE_SCRIPT.replace("'", '"'), timeout=30)
    if rc != 0 or "L=" not in out:
        return None
    kv = {}
    for line in out.splitlines():
        if "=" in line:
            k, v = line.split("=", 1)
            kv[k.strip()] = v.strip()
    if not kv.get("L"):
        return None
    st = {
        "level": _num(kv.get("L")),
        "status": kv.get("S"),               # Charging/Discharging/Not charging/Full
        "current_now": _num(kv.get("I")),
        "charge_counter": _num(kv.get("CC")),
        "voltage": _num(kv.get("V")),
        "port_budget_ma": (_num(kv.get("UMAX")) or 0) // 1000,
        "usb_online": kv.get("UON") == "1",
        "wakefulness": kv.get("W"),
        "focus": kv.get("F") or "",
        "app_pid": kv.get("P"),
        "ts": time.time(),
    }
    d = kv.get("D") or ""
    m = re.search(r"IBUS = (-?\d+)mA", d)
    st["ibus_ma"] = int(m.group(1)) if m else None
    m = re.search(r"IBAT = (-?\d+)mA", d)
    st["ibat_ma"] = int(m.group(1)) if m else None
    m = re.search(r"CHG_EN = (\d+)", d)
    st["chg_en"] = int(m.group(1)) if m else None
    m = re.search(r"ICHG = (\d+)mA", d)
    st["ichg_ma"] = int(m.group(1)) if m else None
    m = re.search(r"AICR = (\d+)mA", d)
    st["aicr_ma"] = int(m.group(1)) if m else None
    return st


def host_streaming():
    """Идёт ли видео-стрим с хоста.

    Два признака: heartbeat энкодера nx-server и живое TCP-соединение
    клиента к control-порту 49001. Аудио-ffmpeg как признак НЕ используем:
    он живёт постоянно и не говорит о подключённом клиенте (на этом споткнулся
    первый вариант модуля)."""
    try:
        if (time.time() - os.path.getmtime("/tmp/nx-vaapi-alive")) < 20:
            return True
    except OSError:
        pass
    rc, out = sh("ss -tan state established '( sport = :49001 )'", timeout=8)
    return rc == 0 and ":49001" in out


def in_foreground(st):
    return APP in (st.get("focus") or "")


def screen_off(st):
    """Экран погашен (планшет в doze/ asleep) — пользователь не смотрит на него."""
    return (st.get("wakefulness") or "").lower() != "awake"


def charging_state(st, cc_delta):
    """(состояние, человекочитаемая причина)."""
    status = (st.get("status") or "").lower()
    if status.startswith("charging"):
        return "CHARGING", "заряжается"
    if cc_delta is not None and cc_delta > 0:
        return "CHARGING", "заряд идёт (%+d мА·ч)" % cc_delta
    if not st.get("usb_online"):
        return "ON_BATTERY", "USB-питания нет — планшет на батарее"
    budget = st.get("port_budget_ma") or 0
    if budget and budget <= PORT_USB2_MA:
        return "USB_PORT_TOO_WEAK", (
            "порт USB-A ПК: %d мА. Драйвер планшета (mt6370+SGM41513) видит "
            "источник как «USB-порт ПК» (ACCDET), идёт по ветке PE+ "
            "(«PE+ is disabled») и выключает заряд: CHG_EN=0, ICHG=%s мА — "
            "на весь бюджет порта, а сам планшет ест %s мА. От сетевой "
            "зарядки берёт 2 А, от Type-C-источника — 1.5 А "
            "(charging_host_charger_current). Чтобы заряд пошёл: кабель "
            "Type-C↔Type-C в Type-C порт хоста (или сетевой зарядник)."
            % (budget, st.get("ichg_ma") or "500",
               st.get("ibus_ma") or "~100-400"))
    return "NOT_CHARGING", (
        "USB есть, бюджет %s мА, но CHG_EN=%s — заряд не идёт (нештатно)"
        % (budget or "?", st.get("chg_en") or "?"))


def notify(title, text):
    global _last_notify
    now = time.time()
    if now - _last_notify < NOTIFY_COOLDOWN:
        return
    _last_notify = now
    # notify-send от root не достучится до пользовательской сессии — идём
    # от владельца репозитория (NX_USER/NX_UID).
    sh("sudo -u %s env DBUS_SESSION_BUS_ADDRESS=unix:path=%s/bus "
       "XDG_RUNTIME_DIR=%s notify-send -u critical -t 15000 %s %s"
       % (USER_NAME, USER_RUNTIME, USER_RUNTIME,
          sh_quote(title), sh_quote(text)), timeout=10)
    log("УВЕДОМЛЕНИЕ: %s — %s" % (title, text))


def sh_quote(s):
    return "'" + str(s).replace("'", "'\\''") + "'"


def trim_idle(st):
    """Гасит Limelight, когда он висит в фоне, а стрим не идёт.

    Замер: am force-stop com.limelight снимает ~150 мА (IBUS 300 -> 100 мА).
    Приложение в фоне держит систему и не даёт планшету уснуть.
    """
    global _bg_since, _stopped_at
    if in_foreground(st):
        _bg_since = 0.0
        return
    if not st.get("app_pid"):
        _bg_since = 0.0
        return
    now = time.time()
    if _bg_since == 0.0:
        _bg_since = now
        return
    if now - _bg_since < IDLE_BEFORE_STOP:
        return
    if now - _stopped_at < 300:
        return
    if not screen_off(st):
        return          # экран горит — человек рядом, приложение не трогаем
    if host_streaming():
        return          # идёт видео-стрим — не мешаем
    rc, out = adb("am force-stop %s" % APP, timeout=20)
    _stopped_at = now
    if rc == 0:
        log("Limelight в фоне без стрима — погасил (экономия ~150 мА)")
    else:
        log("Не смог погасить Limelight: %s" % out[:120])


def save_state(st, cc_delta, state, reason):
    st.update({"charge_state": state, "reason": reason,
               "cc_delta_uah": cc_delta, "host_streaming": host_streaming()})
    try:
        with open(STATE, "w") as f:
            json.dump(st, f, ensure_ascii=False, indent=1)
    except Exception:
        pass


def load_peak():
    """Восстанавливаем пик разряда из прошлого запуска (правило 50% должно
    учитывать и то, что планшет разряжался раньше сессии)."""
    global _peak_discharge_ma, _peak_discharge_ts
    try:
        with open(STATE) as f:
            d = json.load(f)
        peak = float(d.get("peak_discharge_ma") or 0.0)
        if peak >= DISCHARGE_MIN_MA:
            _peak_discharge_ma = peak
            _peak_discharge_ts = time.time()
    except Exception:
        pass


def cycle():
    global _last_cc, _last_cc_ts, _peak_discharge_ma, _peak_discharge_ts
    global _passive_ma
    st = tablet_state()
    if not st:
        log("планшет недоступен (adb) — пропускаю цикл")
        return
    cc_delta = None
    if st.get("charge_counter") is not None:
        if _last_cc is not None and st["charge_counter"] >= _last_cc:
            cc_delta = st["charge_counter"] - _last_cc
        if _last_cc_ts:
            dt = st["ts"] - _last_cc_ts
            if dt > 0 and cc_delta is not None:
                # мкА·ч/с -> мА
                st["charge_rate_ma"] = cc_delta / dt / 1000.0
        _last_cc = st["charge_counter"]
        _last_cc_ts = st["ts"]

    # --- скорости: разряд в активном режиме и заряд в пассивном ---
    # current_now: отрицательный = разряд, положительный = заряд (мкА)
    cur = st.get("current_now") or 0
    if cur < 0 and not screen_off(st) and (-cur / 1000.0) >= DISCHARGE_MIN_MA:
        # разряд при включённом экране — это «активный расход», его запоминаем
        now = time.time()
        if (-cur / 1000.0) > _peak_discharge_ma or \
                now - _peak_discharge_ts > DISCHARGE_WINDOW:
            _peak_discharge_ma = -cur / 1000.0
            _peak_discharge_ts = now
    if screen_off(st) and st.get("charge_rate_ma") is not None:
        _passive_ma = st["charge_rate_ma"]

    state, reason = charging_state(st, cc_delta)
    trim_idle(st)
    save_state(st, cc_delta, state, reason)

    need_ma = _peak_discharge_ma * PASSIVE_MIN_RATIO
    st["peak_discharge_ma"] = round(_peak_discharge_ma, 1)
    st["passive_charge_ma"] = round(_passive_ma, 1)
    st["required_ma"] = round(need_ma, 1)
    st["ratio_ok"] = (need_ma <= 0) or (_passive_ma >= need_ma)
    try:
        with open(STATE, "w") as f:
            json.dump(st, f, ensure_ascii=False, indent=1)
    except Exception:
        pass

    if state == "CHARGING":
        log("заряд: идёт, %.0f мА (уровень %s%%, нужно ≥%.0f мА)"
            % (st.get("charge_rate_ma") or 0.0, st.get("level"), need_ma))
    elif state == "USB_PORT_TOO_WEAK":
        log("заряд: НЕ идёт — %s" % reason)
    elif state == "ON_BATTERY":
        log("заряд: %s (уровень %s%%, ток %s мА)"
            % (reason, st.get("level"), st.get("current_now")))
    else:
        log("заряд: %s" % reason)

    # Правило пользователя: в пассивном режиме заряд должен быть не меньше
    # половины активного расхода. Не met → тревога с настоящей причиной.
    if state != "CHARGING" and need_ma > 0 and _passive_ma < need_ma:
        notify("Планшет не добирает заряд",
               "В простое заряд %.0f мА, нужно ≥%.0f мА (половина активного "
               "расхода %.0f мА). %s"
               % (_passive_ma, need_ma, _peak_discharge_ma, reason))
    elif state != "CHARGING" and (st.get("level") or 0) <= LOW_BATTERY:
        notify("Планшет не заряжается",
               "Уровень %d%%. %s" % (st.get("level") or 0, reason))


def main():
    once = "--once" in sys.argv
    load_peak()
    log("nx-tablet-power запущен (опрос каждые %d с)" % PERIOD)
    if _peak_discharge_ma:
        log("пик активного разряда из прошлой сессии: %.0f мА "
            "(нужно в простое ≥%.0f мА)"
            % (_peak_discharge_ma, _peak_discharge_ma * PASSIVE_MIN_RATIO))
    while True:
        try:
            cycle()
        except Exception as e:  # noqa: BLE001
            log("ошибка цикла: %r" % e)
        if once:
            return
        time.sleep(PERIOD)


if __name__ == "__main__":
    main()
