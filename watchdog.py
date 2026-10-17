#!/usr/bin/env python3
# nx-watchdog: самовосстановление RNDIS-канала планшета после обрыва.
#
# Симптом обрыва (воспроизведён дважды): на rndis0 планшета ВСЕ RX-пакеты с
# ошибкой (errs == packets), пинг 102.168.178.89 -> 100% loss, при этом adb
# (другой транспорт, bulk usbfs) жив. Единственное лекарство — пере-enumerация
# гаджета. Физический переподключитель кабеля работает, но дёргает пользователя.
# Программный эквивалент — USBDEVFS_RESET на usbfs-узле устройства.
#
# Восстановление зеркалит ручную последовательность (проверена на живом стриме):
#   1) USBDEVFS_RESET -> пере-enumerация гаджета (новый MAC, rndis_host заново)
#   2) ждём adb + rndis0 с адресом 192.168.178.89
#   3) восстанавливаем адрес хоста 192.168.178.1/24 (слетает после сброса)
#   4) чиним fwmark-таблицу rndis0 на планшете (Android шлёт трафик через неё)
#   5) если слетело членство mDNS-группы 224.0.0.251 — перезапускаем nx-server
#   6) переподключаем стрим (adb-тапы по Limelight)
#
# Табу соблюдаются: НЕ setprop sys.usb.config none, НЕ wm density reset,
# НЕ svc power stayon, НЕ pkill -f, ничего системного не меняем вне перечисленного.

import fcntl
import glob
import os
import re
import subprocess
import sys
import time

HOST_IP = "192.168.178.1"
TABLET_IP = "192.168.178.89"
SUBNET = "192.168.178.0/24"
IFACE = "enp0s16u3"
USB_VENDOR = "0e8d"          # MediaTek RNDIS-гаджет
SERVER_DIR = "/home/et/nx-rust"
SERVER_BIN = "./target/release/nx-server"
SERVER_NAME = "archpizdetc"
SERVER_PIN = "1234"
LOG = "/tmp/nx-watchdog.log"

USBDEVFS_RESET = 0x5514      # _IO('U', 20)

CHECK_PERIOD = 5             # сек между проверками здоровья
REPAIR_COOLDOWN = 90         # минимум между полными восстановлениями
MDNS_GROUP = "224.0.0.251"


def log(msg):
    line = "[%s] %s" % (time.strftime("%H:%M:%S"), msg)
    try:
        with open(LOG, "a") as f:
            f.write(line + "\n")
    except Exception:
        pass
    print(line, flush=True)


def sh(cmd, timeout=20):
    """Синхронный shell. Возвращает (rc, output)."""
    try:
        r = subprocess.run(cmd, shell=True, capture_output=True, text=True,
                           timeout=timeout)
        return r.returncode, (r.stdout + r.stderr).strip()
    except subprocess.TimeoutExpired:
        return -1, "timeout"
    except Exception as e:
        return -1, str(e)


def adb(cmd, timeout=20):
    """adb shell. Возвращает (rc, output)."""
    return sh('adb shell "%s"' % cmd, timeout=timeout)


# ---------- диагностика ----------

def ping_tablet():
    rc, _ = sh("ping -c 2 -W 1 %s" % TABLET_IP, timeout=8)
    return rc == 0


def rndis_err_counters():
    """(packets, errs) на rndis0 планшета, либо None если adb недоступен."""
    rc, out = adb("cat /proc/net/dev | grep rndis0")
    if rc != 0 or "rndis0" not in out:
        return None
    parts = out.split()
    # вид: "rndis0: <rx_bytes> <rx_packets> <rx_errs> ..."
    if len(parts) < 4:
        return None
    try:
        packets = int(parts[2])
        errs = int(parts[3])
        return packets, errs
    except ValueError:
        return None


def input_device_count():
    rc, out = sh(
        "for e in $(ls /dev/input/ 2>/dev/null | grep -E '^event[0-9]+$'); do "
        "n=$(cat /sys/class/input/$e/device/name 2>/dev/null); "
        "case \"$n\" in *nx-server*) echo x;; esac; done | wc -l")
    try:
        return int(out.strip())
    except ValueError:
        return 0


def stream_active():
    """Живой (не defunct) ffmpeg = подключён клиент и идёт пайплайн.
    input_device_count() НЕ годится: uinput-устройства живут, пока работает
    nx-server, и не говорят о реальном стриме. ffmpeg спавнится при коннекте
    и становится defunct/умирает при отключении."""
    rc, out = sh("ps -eo stat,comm | grep ffmpeg | grep -v defunct")
    return rc == 0 and "ffmpeg" in out


def mdns_membership_ok():
    rc, out = sh("ip maddr show dev %s" % IFACE)
    return rc == 0 and MDNS_GROUP in out


def server_alive():
    rc, _ = sh("pgrep -x nx-server")
    return rc == 0


def adb_alive():
    """Есть ли РЕАЛЬНО подключённое устройство. Нельзя проверять подстроку
    'device' — заголовок пустого вывода 'List of devices attached' её содержит
    (ловушка, из-за которой wait_adb_back не запускал kill-server)."""
    rc, out = sh("adb devices")
    if rc != 0:
        return False
    for line in out.splitlines():
        line = line.strip()
        if (not line or line.startswith("List of devices")
                or line.startswith("* daemon")):
            continue
        # строка устройства: "<serial>\tdevice" (либо offline/unauthorized)
        if line.split()[-1] == "device":
            return True
    return False


# ---------- восстановление ----------

def resolve_usb_path():
    """Найти /dev/bus/usb/BBB/DDD по vendor ID (devnum может меняться)."""
    for dv in glob.glob("/sys/bus/usb/devices/*/idVendor"):
        try:
            with open(dv) as f:
                if f.read().strip() != USB_VENDOR:
                    continue
        except Exception:
            continue
        dev = os.path.dirname(dv)
        try:
            bus = int(open(os.path.join(dev, "busnum")).read().strip())
            num = int(open(os.path.join(dev, "devnum")).read().strip())
        except Exception:
            continue
        return "/dev/bus/usb/%03d/%03d" % (bus, num)
    return None


def usb_reset():
    path = resolve_usb_path()
    if not path:
        log("  usb-узел планшета не найден (vendor %s)" % USB_VENDOR)
        return False
    try:
        fd = os.open(path, os.O_WRONLY)
        fcntl.ioctl(fd, USBDEVFS_RESET)
        os.close(fd)
        log("  USBDEVFS_RESET %s — ок" % path)
        return True
    except Exception as e:
        log("  USBDEVFS_RESET %s — ошибка: %s" % (path, e))
        return False


def wait_adb_back(timeout=45):
    """Ждать, пока adb вернётся после сброса (сеть поднимаем батнсом отдельно)."""
    t0 = time.time()
    adb_tried = False
    while time.time() - t0 < timeout:
        if adb_alive():
            rc, out = adb("ip -br link show rndis0")
            if rc == 0 and "rndis0" in out:
                return True
        else:
            # после сброса adb-сервер может залипнуть — перезапускаем один раз
            if not adb_tried:
                sh("adb kill-server; adb start-server", timeout=15)
                adb_tried = True
        time.sleep(3)
    return False


def bounce_interfaces():
    """Сбросить интерфейсы с ОБЕИХ сторон. Ключевой шаг: после USB-сброса
    TX-очередь драйвера rndis_host зависает ('NETDEV WATCHDOG: transmit queue
    timed out'), и обычный down/up её чистит. Плюс чистит самоназначенный
    адрес 192.168.215.x и возвращает чистую конфигурацию."""
    adb("ip link set rndis0 down; sleep 2; ip link set rndis0 up")
    adb("ip addr add %s/24 dev rndis0 2>/dev/null" % TABLET_IP)
    sh("ip link set %s down" % IFACE)
    time.sleep(3)
    sh("ip link set %s up" % IFACE)
    time.sleep(3)
    log("  батнс интерфейсов (хост+планшет) — ок")


def ensure_host_ip():
    rc, out = sh("ip -br addr show %s" % IFACE)
    if rc == 0 and HOST_IP in out:
        return False
    sh("ip addr add %s/24 dev %s" % (HOST_IP, IFACE))
    log("  восстановил адрес хоста %s/24" % HOST_IP)
    return True


def ensure_tablet_fwmark_route():
    """Android шлёт весь трафик с fwmark через таблицу 'rndis0'. Если там нет
    маршрута к 178.x — 'Network is unreachable' при живом main."""
    rc, out = adb("ip route show table rndis0")
    if rc == 0 and SUBNET in out:
        return False
    adb("ip route add %s dev rndis0 proto static scope link table rndis0" % SUBNET)
    log("  добавил fwmark-маршрут %s в таблицу rndis0 планшета" % SUBNET)
    return True


def ensure_mdns_membership():
    if mdns_membership_ok():
        return False
    # членство слетает при пере-регистрации интерфейса — сокет жив, группа нет
    if stream_active():
        # стрим активен — перезапуск сервера оборвёт его; не трогаем
        log("  mDNS-группа потеряна, но стрим активен — пропускаю перезапуск")
        return False
    restart_server()
    return True


def restart_server():
    sh("pkill -x nx-server")
    time.sleep(3)
    # nx-serverу нужен Wayland-контекст сессии et (иначе capture не подключится).
    # Разрешаем подпись Hyprland динамически и запускаем от et, не от root.
    env = ("XDG_RUNTIME_DIR=/run/user/1000 WAYLAND_DISPLAY=wayland-1 "
           "HYPRLAND_INSTANCE_SIGNATURE=$(ls -t /run/user/1000/hypr/ "
           "| grep _ | head -1)")
    sh("cd %s && sudo -u et env NX_PIN_WAIT=1 %s %s serve --name %s --pin %s "
       "> /tmp/nx-server.log 2>&1 < /dev/null &"
       % (SERVER_DIR, env, SERVER_BIN, SERVER_NAME, SERVER_PIN), timeout=15)
    time.sleep(5)
    log("  перезапустил nx-server")


# ---------- переподключение стрима (best-effort, adb-тапы) ----------

def ui_dump():
    adb("uiautomator dump /sdcard/nxw.xml >/dev/null 2>&1")
    rc, out = adb("cat /sdcard/nxw.xml")
    return out if rc == 0 else ""


def find_center(xml, texts):
    """Центр первого <node>, чей text (без учёта регистра) совпал. Или None."""
    for node in re.split(r"<node", xml):
        m = re.search(r'text="([^"]*)"', node)
        b = re.search(r'bounds="\[(\d+),(\d+)\]\[(\d+),(\d+)\]"', node)
        if not m or not b:
            continue
        val = m.group(1).strip().lower()
        if val and any(val == t.lower() for t in texts):
            x1, y1, x2, y2 = map(int, b.groups())
            return (x1 + x2) // 2, (y1 + y2) // 2
    return None


def tap(pt):
    if pt:
        adb("input tap %d %d" % pt)


def launch_limelight():
    # MainActivity не существует (Error type 3) — стартовый activity это PcView.
    # monkey надёжно поднимает launcher-activity и возвращает фокус.
    adb("monkey -p com.limelight -c android.intent.category.LAUNCHER 1 "
        ">/dev/null 2>&1")


def reconnect_stream(max_tries=12):
    """Ведём UI Limelight к живому стриму. Best-effort: тапаем только по
    уверенно найденным узлам. После пере-enumeration USB-Preferences могут
    перехватить фокус — закрываем BACK и возвращаем Limelight."""
    launch_limelight()
    time.sleep(3)
    for _ in range(max_tries):
        if stream_active():
            return True
        xml = ui_dump()
        if not xml:
            time.sleep(3)
            continue
        low = xml.lower()
        if "usb preferences" in low or "use usb for" in low:
            # USB-настройки перехватили фокус после пере-enumerации
            adb("input keyevent KEYCODE_BACK")
            time.sleep(2)
            launch_limelight()
            time.sleep(3)
            continue
        if "terminat" in low or "disconnect" in low:
            pt = (find_center(xml, ["OK", "Ok", "CLOSE", "Close",
                                    "Закрыть", "Dismiss"])
                  or (1457, 896))
            log("  стрим: тапаю OK диалога обрыва")
            tap(pt)
        elif find_center(xml, ["Desktop"]):
            log("  стрим: тапаю Desktop")
            tap(find_center(xml, ["Desktop"]))
        elif find_center(xml, [SERVER_NAME]):
            log("  стрим: тапаю хост %s" % SERVER_NAME)
            tap(find_center(xml, [SERVER_NAME]))
        else:
            # незнакуемое состояние — не тапаем вслепую, пробуем вернуть Limelight
            log("  стрим: неизвестное состояние UI, возвращаю Limelight")
            launch_limelight()
        time.sleep(4)
    return stream_active()


# ---------- цикл восстановления ----------

def repair(reason):
    log("ВОССТАНОВЛЕНИЕ (причина: %s)" % reason)
    if not usb_reset():
        log("  сброс не удался — выхожу")
        return False
    if not wait_adb_back():
        log("  adb не вернулся за 45с — возможно, нужен физический кабель")
        return False
    # после сброса TX-очередь rndis_host зависает — чистим батнсом, до 3 раз
    for attempt in range(3):
        bounce_interfaces()
        ensure_host_ip()
        ensure_tablet_fwmark_route()
        time.sleep(2)
        if ping_tablet():
            break
        log("  пинг всё ещё fails после батнса #%d" % (attempt + 1))
    else:
        log("  батнс не помог за 3 попытки — возможно, нужен физический кабель")
        return False
    ensure_mdns_membership()
    time.sleep(2)
    ok = reconnect_stream()
    if ping_tablet():
        log("ВОССТАНОВЛЕНО: пинг ok, стрим %s"
            % ("жив" if ok else "НЕ поднялся (тапните вручную)"))
        return True
    log("ВОССТАНОВЛЕНИЕ НЕПОЛНОЕ: пинг всё ещё fails")
    return False


def main():
    log("nx-watchdog запущен (проверка каждые %ss, кулдаун %ss)"
        % (CHECK_PERIOD, REPAIR_COOLDOWN))
    last_repair = 0.0
    fails = 0
    while True:
        try:
            if ping_tablet():
                if fails:
                    log("канал восстановился сам (%d fails)" % fails)
                fails = 0
                time.sleep(CHECK_PERIOD)
                continue

            fails += 1
            ctr = rndis_err_counters()
            detail = ("rndis RX=%s errs=%s" % ctr) if ctr else "adb недоступен"
            # счётчик errs на rndis0 — накопительный мусор гаджета (errs==packets
            # всегда, даже при живом пинге) — годится ТОЛЬКО для лога, не для гейта
            log("пинг fails #%d (%s)" % (fails, detail))

            if fails >= 2 and (time.time() - last_repair) > REPAIR_COOLDOWN:
                if resolve_usb_path() is None:
                    # планшет физически не на шине — чинить нечего, не жжём кулдаун
                    log("  usb-узел планшета отсутствует — жду подключения")
                    fails = 0
                else:
                    last_repair = time.time()
                    fails = 0
                    repair("ping fails, %s" % detail)
            time.sleep(CHECK_PERIOD)
        except KeyboardInterrupt:
            log("остановлен пользователем")
            break
        except Exception as e:
            log("ошибка цикла: %s" % e)
            time.sleep(CHECK_PERIOD)


if __name__ == "__main__":
    if "--check" in sys.argv:
        # одиночная диагностика без восстановления
        print("ping_tablet:", ping_tablet())
        print("adb_alive:", adb_alive())
        print("rndis_err:", rndis_err_counters())
        print("mdns_membership_ok:", mdns_membership_ok())
        print("server_alive:", server_alive())
        print("input_devices:", input_device_count())
        print("usb_path:", resolve_usb_path())
        sys.exit(0)
    if "--repair-now" in sys.argv:
        # одиночный прогон repair-цепочки (для контролируемого теста)
        repair("manual --repair-now")
        sys.exit(0)
    main()
