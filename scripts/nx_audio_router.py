#!/usr/bin/env python3
# Роутинг звука по воркспейсам.
#
# Проблема: nx-server слушал monitor дефолтного sink'а → ВЕСЬ звук десктопа
# шёл и на хост-динамики, и на планшет (дубль), включая приложения,
# открытые на eDP-1.
#
# Решение: приложения, чьи окна висят на стрим-мониторе (HEADLESS-4),
# роутятся в виртуальный sink `nx_stream` (его слушает nx-server → планшет,
# на хост-динамиках тишина). Всё остальное играет в дефолтный sink
# (динамики хоста, в стрим не попадает).
#
# Роутинг по PID: hyprctl clients даёт pid окна, pactl sink-input —
# application.process.id. Для звука, запущенного из терминала (pw-play из
# foot), pid аудио-процесса ≠ pid окна — поднимаемся по дереву предков
# (/proc/<pid>/stat), пока не найдём pid с окном.
#
# Когда сессии Moonlight нет (RTP-пиры на :49002 отсутствуют), всё
# переключается обратно в дефолтный sink — иначе приложения на
# стрим-воркспейсе останются без звука.
#
# Запуск: nohup python3 scripts/nx_audio_router.py > /tmp/nx-audio-router.log 2>&1 &

import json
import os
import re
import socket
import subprocess
import sys
import time

STREAM_MONITOR = os.environ.get("NX_STREAM_MONITOR", "HEADLESS-4")
NX_SINK = os.environ.get("NX_SINK", "nx_stream")
RTP_PORT = "49002"
EVENTS = (
    "openwindow", "movewindow", "closedwindow", "workspace",
    "moveworkspace", "focusedmon", "moveonwindow",
)
DEBOUNCE = 0.3
# Опрос 1 с: запуск pw-play не порождает hyprland-событий, ловим только
# опросом; 5 с не хватало для коротких звуков (тон 3 с доигрывал до опроса).
POLL = 1.0


def sh(*a: str) -> str:
    try:
        return subprocess.run(a, capture_output=True, text=True, timeout=5).stdout
    except Exception:
        return ""


def ensure_sink() -> None:
    sinks = sh("pactl", "list", "short", "sinks")
    if not any(line.split("\t")[1] == NX_SINK for line in sinks.splitlines() if "\t" in line):
        sh("pactl", "load-module", "module-null-sink", f"sink_name={NX_SINK}",
           "sink_properties=device.description=NXStream")
        log(f"sink {NX_SINK} создан")


def streamed_pids() -> set[int]:
    """PID окон, чьи воркспейсы висят на стрим-мониторе."""
    try:
        ws_on_mon = {w["name"] for w in json.loads(sh("hyprctl", "workspaces", "-j"))
                     if w["monitor"] == STREAM_MONITOR
                     and not w["name"].startswith("special")}
        clients = json.loads(sh("hyprctl", "clients", "-j"))
    except Exception:
        return set()
    return {c["pid"] for c in clients if c["workspace"]["name"] in ws_on_mon}


def ancestors(pid: int) -> set[int]:
    """pid и все его предки до init — дерево процессов через /proc."""
    chain: set[int] = set()
    while pid and pid > 1 and pid not in chain and len(chain) < 32:
        chain.add(pid)
        try:
            with open(f"/proc/{pid}/stat") as f:
                rest = f.read().rsplit(")", 1)[1].split()
                pid = int(rest[1])  # PPid
        except Exception:
            break
    return chain


def sink_inputs() -> list[tuple[int, int | None, int | None]]:
    """[(sink-input index, OS-pid процесса, текущий sink id)].

    pactl list sink-inputs не отдаёт application.process.id (даже для
    pw-play) — есть только `Client: N`. Связку даёт pactl list clients:
    `Client #N` → application.process.id.
    """
    clients: dict[int, int] = {}
    cur: int | None = None
    for line in sh("pactl", "list", "clients").splitlines():
        m = re.match(r"Client #(\d+)", line)
        if m:
            cur = int(m.group(1))
        m = re.search(r'application\.process\.id = "?(\d+)"?', line)
        if m and cur is not None:
            clients[cur] = int(m.group(1))
            cur = None
    res: list[tuple[int, int | None, int | None]] = []
    idx: int | None = None
    cid: int | None = None
    sink: int | None = None

    def flush() -> None:
        if idx is not None:
            res.append((idx,
                        clients.get(cid) if cid is not None else None,
                        sink))

    for line in sh("pactl", "list", "sink-inputs").splitlines():
        m = re.match(r"Sink Input #(\d+)", line)
        if m:
            flush()
            idx, cid, sink = int(m.group(1)), None, None
        m = re.match(r"\s*Client: (\d+)", line)
        if m:
            cid = int(m.group(1))
        m = re.match(r"\s*Sink: (\d+)", line)
        if m:
            sink = int(m.group(1))
    flush()
    return res


def sink_names() -> dict[int, str]:
    out = sh("pactl", "list", "short", "sinks")
    res = {}
    for line in out.splitlines():
        f = line.split("\t")
        if len(f) >= 2:
            res[int(f[0])] = f[1]
    return res


def streaming_active() -> bool:
    """Жива ли сессия Moonlight: есть RTP-пиры на :49002."""
    return f":{RTP_PORT}" in sh("ss", "-uan")


def apply() -> None:
    active = streaming_active()
    sp = streamed_pids() if active else set()
    default = sh("pactl", "get-default-sink").strip()
    names = sink_names()
    for idx, pid, sink in sink_inputs():
        if pid is None:
            continue
        target = NX_SINK if (ancestors(pid) & sp) else default
        if names.get(sink) != target:
            sh("pactl", "move-sink-input", str(idx), target)
            log(f"sink-input #{idx} (pid {pid}) -> {target}")


_ts = 0.0


def log(msg: str) -> None:
    global _ts
    now = time.time()
    if now - _ts < 0.01:
        return
    _ts = now
    print(f"{time.strftime('%H:%M:%S')} {msg}", flush=True)


def event_socket() -> socket.socket | None:
    sig = os.environ.get("HYPRLAND_INSTANCE_SIGNATURE")
    if not sig:
        return None
    path = f"/run/user/1000/hypr/{sig}/.socket2.sock"
    s = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
    try:
        s.connect(path)
        return s
    except Exception:
        return None


def main() -> None:
    ensure_sink()
    sock = event_socket()
    log(f"старт: stream_monitor={STREAM_MONITOR} sink={NX_SINK} "
        f"events={'ok' if sock else 'нет (только опрос)'}")
    pending = False
    last = 0.0
    while True:
        try:
            if sock:
                sock.settimeout(POLL)
                try:
                    data = sock.recv(65536).decode(errors="replace")
                    if not data:
                        sock.close()
                        sock = event_socket()
                        continue
                    if any(data.startswith(e) or f"\n{e}" in data for e in EVENTS):
                        pending = True
                except socket.timeout:
                    pass
            now = time.time()
            if pending and now - last > DEBOUNCE:
                apply()
                pending = False
                last = now
            # Всегда периодически: появление sink-input не порождает
            # hyprland-событий, без опроса новый звук не будет замаршрутизирован.
            elif now - last > POLL:
                apply()
                last = now
        except KeyboardInterrupt:
            return
        except Exception as e:
            log(f"ошибка: {e}")
            time.sleep(1)


if __name__ == "__main__":
    sys.exit(main())
