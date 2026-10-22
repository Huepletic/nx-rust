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
#   2) ждём adb + rndis0 с адресом 192.168.8.89
#   3) восстанавливаем адрес хоста 192.168.8.1/24 (слетает после сброса)
#   4) чиним fwmark-таблицу rndis0 на планшете (Android шлёт трафик через неё)
#   5) если слетело членство mDNS-группы 224.0.0.251 — перезапускаем nx-server
#   6) переподключаем стрим (adb-тапы по Limelight)
#
# Табу соблюдаются: НЕ setprop sys.usb.config none, НЕ wm density reset,
# НЕ svc power stayon, НЕ pkill -f, ничего системного не меняем вне перечисленного.

import fcntl
import glob
import os
import pwd
import re
import socket
import subprocess
import sys
import time


# ── Конфигурация ────────────────────────────────────────────────────────────
# Всё переопределяется окружением или файлом /etc/nx-rust/nx.env (пример —
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

_REPO = os.path.dirname(os.path.abspath(__file__))
SERVER_DIR = os.environ.get("NX_SERVER_DIR", _REPO)
# По умолчанию — имя хоста; так репозиторий не содержит личного hostname.
SERVER_NAME = os.environ.get("NX_SERVER_NAME", socket.gethostname().split(".")[0])
SERVER_PIN = os.environ.get("NX_SERVER_PIN", "1234")
LOG = os.environ.get("NX_WATCHDOG_LOG", "/tmp/nx-watchdog.log")

# Пользователь, от которого живут Wayland/PipeWire и приложения. Watchdog
# работает root'ом, поэтому getuid() не подходит: берём владельца каталога
# репозитория. NX_USER/NX_UID переопределяют.
try:
    _pw = pwd.getpwuid(os.stat(SERVER_DIR).st_uid)
    _DEF_USER, _DEF_UID = _pw.pw_name, _pw.pw_uid
except OSError:
    _DEF_USER, _DEF_UID = "nx", 1000
USER_NAME = os.environ.get("NX_USER", _DEF_USER)
USER_UID = int(os.environ.get("NX_UID", str(_DEF_UID)))
USER_RUNTIME = "/run/user/%d" % USER_UID

HOST_IP = os.environ.get("NX_HOST_IP", "192.168.8.1")      # адрес хоста в RNDIS-подсети;
                            # его же Moonlight-клиент хранит в своей БД — менять нельзя
TABLET_IP = os.environ.get("NX_TABLET_IP", "192.168.8.89") # rndis0 на планшете
SUBNET = os.environ.get("NX_TABLET_SUBNET", "192.168.8.0/24")
USB_VENDOR = os.environ.get("NX_USB_VENDOR", "0e8d")       # MediaTek RNDIS-гаджет
# quickshell — личный шелл автора; в чужом окружении его нет. Пусто = не трогать.
QUICKSHELL_UNIT = os.environ.get("NX_QUICKSHELL_UNIT", "")
# Host-овский netdev НЕ захардкожен. rndis.gs4 отдаёт ДВА интерфейса:
#   0xE0 (Wireless, control-only) и 0x02 (CDC Data) — реальный канал данных.
# Прежний фиксированный IFACE=control-интерфейс давал 100% RX-ошибок на
# rndis0 планшета (errs == packets) -> NETDEV WATCHDOG -> зависшая TX-очередь
# -> watchdog делал USBDEVFS_RESET по кругу (~500 раз за ночь) -> xHCI встал
# намертво ("xHCI HW did not halt within 32000 usec"). Теперь интерфейс с
# данными определяется динамически, а IP вешается только на него.
IFACE = None                # вычисляется в data_netdev()

USBDEVFS_RESET = 0x5514      # _IO('U', 20)

CHECK_PERIOD = 5             # сек между проверками здоровья
REPAIR_COOLDOWN = 90         # минимум между полными восстановлениями
STREAM_STALL_SECS = 15       # живой ffmpeg, но кадры не идут столько → зависание
SOFT_REPAIR_COOLDOWN = 60    # не чаще раза в минуту: мягкое восстановление
                            # дёргает интерфейсы, при частом повторе забивает
                            # журнал и нагружает хост (был шторм sudo ip addr
                            # раз в секунду — 2026-10-20 22:09)
_last_soft_repair = 0.0
RECONNECT_COOLDOWN = 120     # не чаще раза в 2 мин: переподключение стрима
                            # поднимает Limelight на планшете, а при погашенном
                            # экране вообще упирается в бесконечный цикл
_last_reconnect = 0.0
MDNS_GROUP = "224.0.0.251"
RESET_BUDGET = 4             # максимум USBDEVFS_RESET за окно RESET_WINDOW
RESET_WINDOW = 1800          # 30 минут
_resets = []                 # времена последних USB-сбросов


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
    и становится defunct/умирает при отключении.
    NX_ENCODER=vaapi: ffmpeg-процесса нет — живость по heartbeat-файлу
    /tmp/nx-vaapi-alive (обновляется nx-server раз в секунду при encode)."""
    rc, out = sh("ps -eo stat,comm | grep ffmpeg | grep -v defunct")
    if rc == 0 and "ffmpeg" in out:
        return True
    try:
        import os
        st_ = os.stat("/tmp/nx-vaapi-alive")
        age = time.time() - st_.st_mtime
        return age < 5.0
    except OSError:
        return False


def live_ffmpeg_pid():
    """PID живого (Sl/S) ffmpeg, либо None. defunct (зомби) не считается живым.
    NX_ENCODER=vaapi: процесса нет — возвращаем -1 (стрим жив по heartbeat,
    stall-детектор ниже различает sentinel)."""
    rc, out = sh("ps -eo pid,stat,comm | grep ffmpeg | grep -v defunct")
    if rc == 0:
        for line in out.splitlines():
            parts = line.split()
            if len(parts) >= 3 and parts[1][:1] in ("S", "R"):
                try:
                    return int(parts[0])
                except ValueError:
                    pass
    try:
        import os
        st_ = os.stat("/tmp/nx-vaapi-alive")
        if time.time() - st_.st_mtime < 5.0:
            return -1
    except OSError:
        pass
    return None


def client_connected() -> bool:
    """Клиент на связи.

    Раньше здесь проверялось наличие ':49002' в выводе `ss -uan`. Но сервер
    ВСЕГДА держит UDP-сокет аудио-порта открытым (слушает, а не подключён) —
    проверка была всегда истинной, и watchdog каждые ~2 минуты писал
    «АУДИО-КОНВЕЙЕР МЁРТ (ffmpeg -f pulse нет) — перезапускаю сервер»,
    хотя клиента не было (2026-10-22 06:55-07:04, далее до рестарта сервера
    стрим всё равно не поднимался).

    Теперь: у клиента Moonlight открыто TCP-соединение к control-порту
    49001 (ENet) — это и есть признак «клиент на связи».
    """
    rc, out = sh("ss -tan state established '( sport = :49001 )'", timeout=8)
    return rc == 0 and ":49001" in out


def wireplumber_cpu_pct(window=2.0) -> float:
    """Текущая загрузка CPU процессом wireplumber (дельта utime+stime)."""
    pids = []
    rc, out = sh("pgrep -x wireplumber", timeout=8)
    if rc == 0:
        for p in out.split():
            try:
                pids.append(int(p))
            except ValueError:
                pass
    if not pids:
        return 0.0
    total = 0.0
    for pid in pids:
        try:
            with open("/proc/%d/stat" % pid) as f:
                r = f.read().rsplit(")", 1)[1].split()
            total += int(r[11]) + int(r[12])      # utime + stime (тики)
        except OSError:
            pass
    time.sleep(window)
    total2 = 0.0
    for pid in pids:
        try:
            with open("/proc/%d/stat" % pid) as f:
                r = f.read().rsplit(")", 1)[1].split()
            total2 += int(r[11]) + int(r[12])
        except OSError:
            pass
    try:
        hz = os.sysconf("SC_CLK_TCK")
    except (ValueError, OSError):
        hz = 100
    return max(0.0, (total2 - total) * 100.0 / (hz * window))


def restart_wireplumber():
    """Перезапуск session manager'а звука.

    wireplumber на этом хосте входит в бесконечный retry по D-Bus
    «Unexpected reply 3 when releasing name org.freedesktop.ReserveDevice1.*»
    и жжёт 0.5-4 ядра (2026-10-21: спираль съела весь 4-ядерный CPU, поток
    видео падал до 1-20 fps). Рестарт сбрасывает состояние: после рестарта
    процесс держит ~5% CPU, ошибка не повторяется. PipeWire при этом не
    трогаем — наш null-sink nx_stream и аудио-ffmpeg переживают рестарт."""
    # systemctl --user требует пользовательской шины D-Bus; watchdog запускается
    # как системный юнит без этих переменных (2026-10-21: rc=1, рестарт не прошёл).
    rc, out = sh("XDG_RUNTIME_DIR=%s "
                 "DBUS_SESSION_BUS_ADDRESS=unix:path=%s/bus "
                 "systemctl --user restart wireplumber.service"
                 % (USER_RUNTIME, USER_RUNTIME), timeout=30)
    if rc == 0:
        log("  перезапустил wireplumber (был в спирали ReserveDevice)")
        time.sleep(5)
        return True
    log("  не смог перезапустить wireplumber: rc=%s" % rc)
    return False


def last_writer_total():
    """Последний 'writer: ... (всего N)' из лога nx-server = реально ушедшие
    в ffmpeg кадры. N двигается только при живом потоке."""
    try:
        with open("/tmp/nx-server.log", "rb") as f:
            data = f.read()[-200000:]
    except Exception:
        return None
    txt = data.decode("utf-8", "replace").replace("\x00", "")
    last = None
    for line in txt.splitlines():
        if line.startswith("writer:") and "всего" in line:
            m = re.search(r"всего\s+(\d+)", line)
            if m:
                last = int(m.group(1))
    return last


def server_alive():
    """Процесс nx-server жив. pgrep -x (точное имя), чтобы не поймать обёртку."""
    rc, out = sh("pgrep -x nx-server")
    return rc == 0 and out.strip() != ""


def mdns_membership_ok():
    """mDNS-группа должна быть на интерфейсе, где реально идёт трафик."""
    for dev in host_netdevs():
        rc, out = sh("ip maddr show dev %s" % dev)
        if rc == 0 and MDNS_GROUP in out:
            return True
    return False


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


def tablet_usb_dir():
    """Каталог USB-устройства планшета в sysfs (vendor 0e8d) или None."""
    for dv in glob.glob("/sys/bus/usb/devices/*/idVendor"):
        try:
            with open(dv) as f:
                if f.read().strip().lower() != USB_VENDOR:
                    continue
        except Exception:
            continue
        return os.path.dirname(dv)
    return None


def tablet_netdevs():
    """[(netdev, bInterfaceClass, usb-ifname)] по интерфейсам планшета.

    У rndis.gs4 их два: 0xe0 — control-only (данные не несёт), 0x02 — CDC Data
    (единственный рабочий канал). Раньше IP вешали на 0xe0."""
    dev = tablet_usb_dir()
    if not dev:
        return []
    out = []
    for ifd in sorted(glob.glob(dev + ":*")):
        nets = glob.glob(ifd + "/net/*")
        if not nets:
            continue
        try:
            cls = open(ifd + "/bInterfaceClass").read().strip().lower()
        except Exception:
            cls = "?"
        for n in nets:
            out.append((os.path.basename(n), cls, os.path.basename(ifd)))
    return out


def data_netdev():
    """Netdev планшета, который реально несёт данные.

    Приоритет: CDC Data (class 0x02). Если такого нет — любой, кроме
    RNDIS-control (0xe0). Если интерфейс один и он control — берём его
    (классический RNDIS с одним интерфейсом и двумя altsetting'ами)."""
    devs = tablet_netdevs()
    if not devs:
        return None
    for name, cls, _if in devs:
        if cls == "0x02":
            return name
    for name, cls, _if in devs:
        if cls != "0xe0":
            return name
    return devs[0][0]


def host_netdevs():
    """Все host-овские netdev'ы планшета (для проверки mDNS-группы)."""
    return [name for name, _cls, _if in tablet_netdevs()]


def usb_reset():
    """USBDEVFS_RESET — НЕ вызывается автоматически (см. repair()).

    Оставлен для ручного диагностического применения: сам по себе сброс
    гаджета с хоста убивал xHCI хоста (серия ~500 reset'ов за ночь ->
    "xHCI HW did not halt within 32000 usec")."""
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


def adb_root(cmd, timeout=30):
    """adb shell от root'а на планшете (su)."""
    return sh('adb shell "su -c \'%s\'"' % cmd.replace('"', '\\"'), timeout=timeout)


def tablet_gadget_fix():
    """Вернуть планшету конфиг USB-гаджета с RNDIS+ADB (0x2005).

    После ребута планшет поднимает только adb-only (0x201c) — RNDIS-функции
    нет, значит нет и канала данных. Скрипт правит configfs (classic
    rndis.gs4 + ffs.adb) прямо на планшете."""
    script = "/data/local/tmp/fix_gadget3.sh"
    rc, out = adb_root("test -f %s" % script)
    if rc != 0:
        src = os.path.join(os.path.dirname(os.path.abspath(__file__)),
                           "scripts", "fix_gadget3.sh")
        if not os.path.exists(src):
            log("  нет скрипта починки гаджета (%s) — RNDIS не поднимется" % src)
            return False
        sh('adb push "%s" %s' % (src, script), timeout=60)
    log("  чиню configfs гаджета на планшете (rndis.gs4 + ffs.adb)")
    adb_root("sh %s" % script, timeout=60)
    time.sleep(6)
    return True


def restore_tablet_network():
    """После ребута/переподключения планшета поднять канал данных целиком."""
    # 1) гаджет (RNDIS + ADB) — на планшете
    tablet_gadget_fix()
    # 2) интерфейс хоста: адрес и маршрут
    dev = data_netdev()
    if not dev:
        log("  host-интерфейс данных не появился после ребута планшета")
        return False
    ensure_host_ip()
    ensure_tablet_fwmark_route()
    # 3) адрес rndis0 на планшете + fwmark-маршрут
    adb_root("ip link set rndis0 up; ip addr flush dev rndis0; "
             "ip addr add %s/24 dev rndis0" % TABLET_IP, timeout=30)
    adb_root("ip route replace %s dev rndis0 scope link table %s"
             % (SUBNET, tablet_rndis_table()), timeout=30)
    time.sleep(2)
    return ping_tablet()


def reboot_tablet():
    """Перезагрузка ПЛАНШЕТА — рабочее восстановление зависшего USB-канала.

    Рецепт пользователя (2026-10-21): «в будущее - фиксится ребутом ПЛАНШЕТА».
    Именно ребут пересоздаёт configfs/USB-функции гаджета. USBDEVFS_RESET с
    хоста дёргает гаджет и убивает xHCI хоста, поэтому в автопроцессе его
    больше нет. adb живёт на отдельной USB-функции (ff/42/01) и работает
    даже при мёртвом RNDIS-канале — значит ребут планшета возможен, когда
    данные уже не идут."""
    if not adb_alive():
        log("  adb недоступен — ребут планшета невозможен (нужен физический "
            "кабель или ребут хоста)")
        return False
    log("  перезагружаю ПЛАНШЕТ (пользовательский рецепт: чинит USB-канал)")
    adb("svc power reboot", timeout=20)
    t0 = time.time()
    while time.time() - t0 < 180:
        time.sleep(10)
        if adb_alive():
            log("  планшет вернулся (adb) за %ds" % int(time.time() - t0))
            break
    else:
        log("  планшет не вернулся за 180с")
        return False
    time.sleep(8)
    return restore_tablet_network()


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
    # адрес rndis0 на планшете держим вручную: RNDIS-HAL сам выставляет
    # 192.168.138.229 (случайный), а клиент и watchdog ждут 192.168.8.89
    adb("ip addr flush dev rndis0; ip addr add %s/24 dev rndis0" % TABLET_IP)
    dev = data_netdev() or IFACE
    sh("ip link set %s down" % dev)
    time.sleep(3)
    sh("ip link set %s up" % dev)
    time.sleep(3)
    log("  батнс интерфейсов (хост+планшет) — ок (%s)" % dev)


def ensure_host_ip():
    """Адрес хоста — только на интерфейсе с данными.
    Если HOST_IP висит на control-интерфейсе (0xe0), его надо снять: пакеты
    уходят в control-канал, планшет их не принимает (rndis0: errs == packets)
    и TX-очередь хоста забивается."""
    dev = data_netdev()
    if not dev:
        return False
    changed = False
    for name, _cls, _if in tablet_netdevs():
        if name == dev:
            continue
        rc, out = sh("ip -br addr show %s" % name)
        if rc == 0 and HOST_IP in out:
            sh("ip addr del %s/24 dev %s" % (HOST_IP, name))
            log("  снял %s с control-интерфейса %s" % (HOST_IP, name))
            changed = True
    rc, out = sh("ip -br addr show %s" % dev)
    if rc == 0 and HOST_IP in out:
        return changed
    sh("ip addr add %s/24 dev %s" % (HOST_IP, dev))
    log("  восстановил адрес хоста %s/24 на %s" % (HOST_IP, dev))
    return True


def tablet_rndis_table():
    """Номер таблицы маршрутизации, которую Android вешает на rndis0.

    На A11 это 97 (правило 'iif lo oif rndis0 uidrange 0-0 lookup 97').
    Таблицы с ИМЕНЕМ 'rndis0' не существует — прежний код писал в неё и
    получал 'table id value is invalid', то есть маршрут не появлялся."""
    rc, out = adb("ip rule show | grep 'oif rndis0' | grep -o 'lookup [0-9]*' | head -1")
    m = re.search(r"lookup (\d+)", out)
    if m:
        return m.group(1)
    rc, out = adb("ip route show table all | grep rndis0 | grep -oE 'table [0-9]+' | head -1")
    m = re.search(r"table (\d+)", out)
    return m.group(1) if m else "97"


def ensure_tablet_fwmark_route():
    """Android гонит fwmark-трафик через СВОЮ таблицу (на A11 — 97, по правилу
    'iif lo oif rndis0 -> lookup 97'). Именно в ней нужен маршрут к нашей
    подсети, иначе 'Network is unreachable' при живом main."""
    tbl = tablet_rndis_table()
    rc, out = adb("ip route show table %s" % tbl)
    if rc == 0 and SUBNET in out:
        return False
    adb("ip route replace %s dev rndis0 scope link table %s" % (SUBNET, tbl))
    log("  fwmark-маршрут %s добавлен в таблицу %s планшета" % (SUBNET, tbl))
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
    # Запуск через launch_nx.sh: внутри setsid + env сессии пользователя.
    # Раньше запускали `sudo -u <user> env ... &` без setsid — процесс привязывался
    # к шеллу watchdog'а и умирал вместе с ним (02:57:23: сервер поднялся и
    # исчез за 48 c).
    sh("chmod +x %s/launch_nx.sh; sudo -u %s bash %s/launch_nx.sh &"
       % (SERVER_DIR, USER_NAME, SERVER_DIR), timeout=15)
    time.sleep(5)
    # Приоритет: DDLC на этом 4-ядерном APU съедает ~75% CPU и дерёт fps
    # (замер 04:04: writer 57fps против потолка энкодера 99fps). nx-server
    # поднимаем на nice -10; ffmpeg-потоки унаследуют приоритет от родителя
    # в момент спавна пайплайна.
    sh("PID=$(pgrep -x nx-server | head -1); [ -n \"$PID\" ] && renice -n -10 -p $PID",
       timeout=10)
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


def tablet_awake():
    """Горит ли экран планшета.

    Когда планшет в doze (пользователь его не смотрит), переподключать стрим
    бессмысленно: UI-dump отдаёт шторку уведомлений, watchdog не находит ни
    «Desktop», ни хоста и каждые 6 с заново поднимает Limelight. Этот шторм
    (с 2026-10-22 01:50, 5600+ сообщений) держал приложение на планшете
    постоянно запущенным (~150 мА) и не давал устройству уснуть — то есть
    это и было «жрёт батарею».
    """
    rc, out = adb("dumpsys power | grep -m1 mWakefulness=", timeout=15)
    return "mWakefulness=Awake" in out


def reconnect_stream(max_tries=12):
    """Ведём UI Limelight к живому стриму. Best-effort: тапаем только по
    уверенно найденным узлам. После пере-enumeration USB-Preferences могут
    перехватить фокус — закрываем BACK и возвращаем Limelight."""
    global _last_reconnect
    now = time.time()
    if not tablet_awake():
        if now - _last_reconnect > 300:
            _last_reconnect = now
            log("  стрим: экран планшета погашен — переподключение отложено")
        return False
    if now - _last_reconnect < RECONNECT_COOLDOWN:
        return False
    _last_reconnect = now
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

def reset_budget_left():
    """Сколько USB-сбросов ещё можно сделать в текущем окне.

    Каждый USBDEVFS_RESET дёргает гаджет; при зависшем xHCI серия сбросов
    добивает контроллер окончательно. Поэтому бюджет жёсткий."""
    now = time.time()
    while _resets and now - _resets[0] > RESET_WINDOW:
        _resets.pop(0)
    return max(0, RESET_BUDGET - len(_resets))


def soft_repair():
    """Мягкое восстановление БЕЗ сброса USB: адрес/интерфейс/маршрут.

    Раньше watchdog при первом же фейле пинга делал USBDEVFS_RESET. За ночь
    набежало ~500 переподключений гаджета; вместе с 100% RX-ошибок (IP висел
    на control-интерфейсе) это положило xHCI ("xHCI HW did not halt within
    32000 usec"). Теперь USB — последнее средство."""
    dev = data_netdev()
    if not dev:
        log("  нет host-интерфейса данных — мягкое восстановление невозможно")
        return False
    # Кулдаун: без него при нестабильном RNDIS восстановление повторялось
    # каждый цикл (журнал набивался `sudo ip addr add` раз в секунду —
    # 2026-10-20 22:09, перед падением композитора).
    global _last_soft_repair
    now_ts = time.time()
    if now_ts - _last_soft_repair < SOFT_REPAIR_COOLDOWN:
        return False
    _last_soft_repair = now_ts
    ensure_host_ip()
    ensure_tablet_fwmark_route()
    # Батнс ОБЕИХ сторон: односторонний down/up интерфейса хоста рвёт
    # alt-setting RNDIS — после него пакеты идут только в одну сторону
    # (проверено 2026-10-20 22:25: хост->планшет 100% loss, TX-счётчик 0).
    adb("ip link set rndis0 down; sleep 1; ip link set rndis0 up")
    sh("ip link set %s down" % dev)
    time.sleep(2)
    sh("ip link set %s up" % dev)
    time.sleep(3)
    adb("ip addr flush dev rndis0; ip addr add %s/24 dev rndis0" % TABLET_IP)
    ensure_host_ip()
    ensure_tablet_fwmark_route()
    time.sleep(2)
    return ping_tablet()


def repair(reason):
    log("ВОССТАНОВЛЕНИЕ (причина: %s)" % reason)
    # 1) мягко: чиним конфиг (интерфейс/адрес/маршрут) — USB не трогаем
    if soft_repair():
        log("  мягкое восстановление (интерфейс/адрес/маршрут) — достаточно")
        ensure_mdns_membership()
        time.sleep(2)
        reconnect_stream()
        if ping_tablet():
            log("ВОССТАНОВЛЕНО: пинг ok, стрим %s"
                % ("жив" if stream_active() else "НЕ поднялся (тапните вручную)"))
            return True
        return False
    # 2) планшет мог перезагрузиться и поднять adb-only (0x201c) — тогда RNDIS-
    #    функции нет вообще. Лечим configfs + адреса, ребут не нужен.
    if restore_tablet_network():
        log("  восстановлен канал данных (гаджет/адреса) без ребута")
        ensure_mdns_membership()
        time.sleep(2)
        ok = reconnect_stream()
        if ping_tablet():
            log("ВОССТАНОВЛЕНО: пинг ok, стрим %s"
                % ("жив" if ok else "НЕ поднялся (тапните вручную)"))
            return True
        return False
    # 3) жёстко: ребут ПЛАНШЕТА (пользовательский рецепт). USBDEVFS_RESET с
    #    хоста больше не делаем — именно он убивал xHCI хоста.
    log("  мягко не помогло — перезагружаю планшет")
    if not reboot_tablet():
        log("  ребут планшета не помог (или adb недоступен) — нужен физический "
            "кабель / ребут хоста")
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


def ensure_audio_sink():
    """Null-sink nx_stream + keepalive-поток.

    nx-server слушает monitor'а nx_stream. Модуль null-sink живёт в процессе
    pipewire, поэтому после рестарта pipewire он исчезает — и monitor'а нет
    вовсе (ffmpeg-аудио умирает: «Error during demuxing»). Плюс sink без
    входящих потоков проваливается в IDLE, и monitor отдаёт тишину рывками
    (замерено 2026-10-21: 118 pkt/s вместо 200) → клиент голодает → шипение.
    Keepalive: module-null-source → module-loopback в nx_stream; у потока нет
    application.process.id, роутер его не трогает.

    Watchdog — системный сервис (root), а pipewire живёт в сессии et:
    pactl обязательно запускаем от пользователя с его D-Bus/RUNTIME.
    """
    env = ("XDG_RUNTIME_DIR=%s "
           "DBUS_SESSION_BUS_ADDRESS=unix:path=%s/bus "
           % (USER_RUNTIME, USER_RUNTIME))
    def user_pactl(*args):
        return sh("sudo -u %s " % USER_NAME + env + "pactl " + " ".join(args),
                  timeout=15)

    rc, out = user_pactl("list", "short", "sinks")
    sink_idx = None
    for line in out.splitlines():
        f = line.split("\t")
        if len(f) >= 2 and f[1] == "nx_stream":
            sink_idx = f[0]
    if sink_idx is None:
        user_pactl("load-module", "module-null-sink", "sink_name=nx_stream",
                   "sink_properties=device.description=NXStream")
        log("  аудио: создал sink nx_stream")
        time.sleep(1)
        rc, out = user_pactl("list", "short", "sinks")
        for line in out.splitlines():
            f = line.split("\t")
            if len(f) >= 2 and f[1] == "nx_stream":
                sink_idx = f[0]
    if sink_idx is None:
        return
    # keepalive: нужен хотя бы один поток в nx_stream (иначе sink в IDLE и
    # monitor отдаёт тишину рывками). Сравниваем ИНДЕКС sink'а (в short-выводе
    # pactl второе поле — индекс, а не имя; с именем проверка не срабатывала
    # никогда и keepalive плодился каждые 30 с).
    rc, out = user_pactl("list", "short", "sink-inputs")
    has_stream = any(line.split("\t")[1:2] == [sink_idx]
                      for line in out.splitlines() if "\t" in line)
    if not has_stream:
        user_pactl("load-module", "module-null-source", "source_name=nx_keepalive",
                   "source_properties=device.description=NXKeepAlive")
        user_pactl("load-module", "module-loopback", "source=nx_keepalive",
                   "sink=nx_stream")
        log("  аудио: добавил keepalive в nx_stream (иначе monitor отдаёт тишину рывками)")


def ensure_quickshell():
    """Поднять пользовательский шелл (quickshell), если он умер.

    Это не часть nx-server: страховка автора для его Hyprland-сессии. В чужом
    окружении отключается пустым NX_QUICKSHELL_UNIT (по умолчанию так и есть).
    Если задан — проверяем, что сессия жива, а юнит активен; иначе поднимаем.
    """
    if not QUICKSHELL_UNIT:
        return
    rc, out = sh("ls %s/hypr 2>/dev/null" % USER_RUNTIME)
    if rc != 0 or not out.strip():
        return  # сессии нет — нечего поднимать
    # systemctl --user от root не видит пользовательскую шину — идём от юзера.
    env = ("XDG_RUNTIME_DIR=%s "
           "DBUS_SESSION_BUS_ADDRESS=unix:path=%s/bus "
           % (USER_RUNTIME, USER_RUNTIME))
    rc, out = sh("sudo -u %s " % USER_NAME + env
                 + "systemctl --user is-active " + QUICKSHELL_UNIT, timeout=15)
    if out.strip() == "active":
        return
    rc, out = sh("sudo -u %s " % USER_NAME + env
                 + "systemctl --user restart " + QUICKSHELL_UNIT, timeout=25)
    if rc == 0:
        log("QUICKSHELL не активен — поднял (%s)" % QUICKSHELL_UNIT)
    else:
        log("QUICKSHELL не активен, поднять не удалось: %s" % out[:120])


def main():
    log("nx-watchdog запущен (проверка каждые %ss, кулдаун %ss, стрим-стол %ss)"
        % (CHECK_PERIOD, REPAIR_COOLDOWN, STREAM_STALL_SECS))
    last_repair = 0.0
    last_stream_fix = 0.0
    last_server_fix = 0.0
    last_wp_check = 0.0
    last_wp_restart = 0.0
    last_audio_check = 0.0
    last_audio_fix = 0.0
    last_qs_check = 0.0
    fails = 0
    # отслеживание продвижения кадров (writer: всего N) для ловли зависшего стрима
    w_seen = None          # последнее виденное значение счётчика кадров
    w_since = 0.0          # когда значение последний раз ИЗМЕНЯЛОСЬ (старт стола)
    while True:
        try:
            # --- сервер должен быть живым ВСЕГДА, даже если планшет недоступен:
            # раньше блок был внутри ping_tablet(), и после ребута хоста nx-server
            # не поднимался, пока канал к планшету не починен.
            now = time.time()
            # --- wireplumber в спирали ReserveDevice съедал весь CPU (0.5-4 ядра)
            #     и ронял видеопоток до 1-20 fps. Раз в минуту меряем реальную
            #     загрузку и перезапускаем сессион-менеджер, если он завертелся.
            if (now - last_wp_check) > 60:
                last_wp_check = now
                pct = wireplumber_cpu_pct(2.0)
                if pct > 40 and (now - last_wp_restart) > 600:
                    last_wp_restart = now
                    log("WIREPLUMBER В СПИРАЛИ: %.0f%% CPU — перезапускаю" % pct)
                    restart_wireplumber()
            # --- аудио-канал: sink nx_stream должен существовать и быть RUNNING ---
            # После рестарта pipewire модуль null-sink исчезает, а sink без
            # входящего потока проваливается в IDLE: monitor отдаёт тишину
            # рывками (118 pkt/s вместо 200) → клиентский AudioTrack голодает →
            # шипение. Держим null-sink + keepalive (null-source → loopback).
            if (now - last_audio_check) > 30:
                last_audio_check = now
                ensure_audio_sink()
            # --- quickshell (панель/док/обои) ---
            # qs отвалился вместе с композитором 2026-10-20 22:09 и не вернулся:
            # автостарт в hypr/execs.lua не сработал (systemctl --user не успел).
            if (now - last_qs_check) > 60:
                last_qs_check = now
                ensure_quickshell()
            if not server_alive() and (now - last_server_fix) > REPAIR_COOLDOWN:
                last_server_fix = now
                log("NX-SERVER МЁРТ — перезапуск")
                restart_server()
                time.sleep(3)
            if ping_tablet():
                if fails:
                    log("канал восстановился сам (%d fails)" % fails)
                fails = 0
                now = time.time()

                # --- self-heal: nx-server умер при живой сети ---
                # Раньше watchdog следил ТОЛЬКО за пингом: nx-server падал (например,
                # после дисконнекта клиента) и его никто не поднимал — стрим лежал
                # часами, пока пользователь не дёрнется. Теперь при живом пинге
                # проверяем сам процесс сервера.
                if not server_alive() and (now - last_server_fix) > REPAIR_COOLDOWN:
                    last_server_fix = now
                    log("NX-SERVER МЁРТВ при живом пинге — перезапуск")
                    restart_server()
                    time.sleep(3)
                    now = time.time()

                # --- мониторинг ЗДОРОВЬЯ СТРИМА (не сети) ---
                # ping здоров, но capture мог умереть (EAGAIN и раньше т.п.) —
                # картинка замёрзла, input от клиента ещё идёт. Раньше не ловилось:
                # watchdog следил ТОЛЬКО за сетью. Сигнал: живой ffmpeg (Sl), но
                # счётчик кадров не двигается. При чистом отключении клиента ffmpeg
                # становится defunct (не Sl) → это не ложное срабатывание.
                # --- аудио-конвейер умер, а видео идёт ---
                # После рестарта pipewire (или его падения) ffmpeg-аудио теряет
                # соединение с pulse и умирает («Error during demuxing»), а
                # видеопоток продолжает идти — клиент получает картинку без звука.
                # Признак: клиент на связи, а процесса «ffmpeg -f pulse» нет.
                if (now - last_audio_fix) > 60 and client_connected():
                    rc, out = sh("pgrep -af 'ffmpeg .*-f pulse' | grep -v pgrep")
                    if not out.strip():
                        last_audio_fix = now
                        log("АУДИО-КОНВЕЙЕР МЁРТ (ffmpeg -f pulse нет) — перезапускаю сервер")
                        restart_server()
                        time.sleep(3)
                        now = time.time()

                pid = live_ffmpeg_pid()
                if pid is not None:
                    w = last_writer_total()
                    if w is None:
                        w_since = now
                    elif w != w_seen:
                        w_seen = w          # кадры пошли/продвинулись — стол сброшен
                        w_since = now
                    elif w_since and (now - w_since) > STREAM_STALL_SECS \
                            and (now - last_stream_fix) > REPAIR_COOLDOWN:
                        last_stream_fix = now
                        log("СТРИМ ЗАВИС: энкодер %s жив, кадры стоят >%ss — "
                            "перезапуск сервера" % ("ffmpeg pid %s" % pid if pid > 0 else "vaapi (heartbeat)", STREAM_STALL_SECS))
                        w_seen = None
                        w_since = now
                        restart_server()
                        time.sleep(3)
                        reconnect_stream()
                else:
                    # Живого ffmpeg/ваапи-heartbeat нет. Но если клиент на
                    # связи (аудио-пир есть) — значит видеосессия не поднялась
                    # (обычно после рестарта nx-server). Лечим переподключением.
                    if client_connected():
                        if not w_since:
                            w_since = now
                        elif (now - w_since) > STREAM_STALL_SECS \
                                and (now - last_stream_fix) > REPAIR_COOLDOWN:
                            last_stream_fix = now
                            log("СТРИМ НЕ ПОДНЯЛСЯ: клиент на связи, видеокадров "
                                "нет >%ss — переподключаю" % STREAM_STALL_SECS)
                            w_seen = None
                            w_since = now
                            reconnect_stream()
                    elif tablet_awake():
                        # Клиента нет, но планшет проснулся (экран горит) —
                        # пользователь ждёт стрим: поднимаем Limelight сами.
                        # При погашенном экране НЕ делаем ничего: ui_dump
                        # отдаёт шторку, watchdog не находит ни «Desktop», ни
                        # хоста и только заново поднимает Limelight каждые 6 с
                        # (шторм 2026-10-22 01:50-07:12, жор батареи).
                        if not w_since:
                            w_since = now
                        elif (now - w_since) > STREAM_STALL_SECS \
                                and (now - last_stream_fix) > REPAIR_COOLDOWN:
                            last_stream_fix = now
                            log("ПЛАНШЕТ ПРОСНУЛСЯ, а стрима нет >%ss — "
                                "поднимаю Limelight" % STREAM_STALL_SECS)
                            w_seen = None
                            w_since = now
                            reconnect_stream()
                    else:
                        # живого ffmpeg нет — стрима нет (простой/чистое отключение)
                        w_seen = None
                        w_since = 0.0

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
