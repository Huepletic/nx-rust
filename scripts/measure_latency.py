#!/usr/bin/env python3
"""Объективный замер сквозной задержки стрима (nx-server → Moonlight → планшет).

Идея: в кадре стрима на headless-выходе хоста горит live-время хоста с
миллисекундами (scripts/nx_clock_layer.py — layer-shell только на HEADLESS-1,
физический экран пользователя не трогаем). На планшете снимаем экран (raw:
PNG-энкодинг занимает 2.6 с, raw — 0.2 с) и читаем время из кадра.

Критично для точности:
  * используем ПОСТОЯННУЮ сессию `adb shell` — у свежего соединения
    установка занимает ~200 мс, команда исполняется уже в конце round-trip,
    и оценка смещения часов смещается на ~100 мс (проверено 2026-10-21);
  * на планшете: date → screencap → date; кадр читается в начале вызова
    screencap, поэтому момент съёма ≈ первая метка date;
  * кадр планшета может быть «свежее» отображаемого (SurfaceFlinger отдаёт
    последний отрендеренный клиентом буфер, а не отсканированный), поэтому
    измеренное значение — верхняя оценка минус render-ahead; калибруем
    заморозкой часов (SIGSTOP): тогда кадр статичен и цепочка проверяется.

Запуск:  python3 scripts/measure_latency.py --samples 10
"""
import argparse
import os
import re
import signal
import statistics
import subprocess
import sys
import time

from PIL import Image, ImageDraw, ImageFont

FONT_PATH = "/usr/share/fonts/TTF/DejaVuSansMono.ttf"
CHARS = "0123456789:."
REMOTE = "/data/local/tmp/lat"


class Shell:
    """Постоянная сессия adb shell: round-trip 10-30 мс вместо 300-400."""

    def __init__(self):
        self.p = subprocess.Popen(["adb", "shell"], stdin=subprocess.PIPE,
                                  stdout=subprocess.PIPE, text=True, bufsize=1)
        self.n = 0

    def run(self, cmd: str, timeout: float = 60.0) -> str:
        self.n += 1
        marker = f"__NXDONE{self.n}__"
        self.p.stdin.write(f"{cmd}; echo {marker}\n")
        self.p.stdin.flush()
        out = []
        t_end = time.time()
        while time.time() - t_end < timeout:
            line = self.p.stdout.readline()
            if not line:
                break
            if marker in line:
                break
            out.append(line.rstrip("\n"))
        return "\n".join(out)

    def timed(self, cmd: str, timeout: float = 60.0):
        t0 = time.time()
        out = self.run(cmd, timeout)
        t1 = time.time()
        return out, t0, t1

    def close(self):
        try:
            self.p.stdin.close()
            self.p.wait(timeout=5)
        except Exception:
            self.p.kill()


def host_utc_offset_ms() -> float:
    z = subprocess.run(["date", "+%z"], capture_output=True, text=True).stdout.strip()
    m = re.match(r"^([+-])(\d{2})(\d{2})$", z)
    if not m:
        return 0.0
    sign = 1 if m.group(1) == "+" else -1
    return sign * (int(m.group(2)) * 3600 + int(m.group(3)) * 60) * 1000.0


def build_templates() -> dict:
    font = ImageFont.truetype(FONT_PATH, 64)
    out = {}
    for ch in CHARS:
        img = Image.new("L", (120, 120), 0)
        ImageDraw.Draw(img).text((10, 8), ch, fill=255, font=font)
        out[ch] = norm(img)
    return out


def norm(img: Image.Image) -> list:
    g = img.point(lambda p: 255 if p > 127 else 0)
    bbox = g.getbbox()
    if not bbox:
        return [0] * (16 * 24)
    small = g.crop(bbox).resize((16, 24), Image.BILINEAR)
    return [1 if p > 127 else 0 for p in small.getdata()]


def read_raw(path: str) -> Image.Image:
    with open(path, "rb") as f:
        w, h, fmt = __import__("struct").unpack("<III", f.read(12))
        data = f.read()
    mode = {1: "RGBA", 5: "RGBX", 3: "RGB;16B"}.get(fmt, "RGBA")
    nch = 2 if fmt == 3 else 4
    return Image.frombytes(mode, (w, h), data[:w * h * nch]).convert("L")


def read_clock(img: Image.Image, templates: dict):
    w, h = img.size
    top = int(h * 0.75)
    reg = img.crop((0, top, int(w * 0.62), h)).point(lambda p: 255 if p > 235 else 0)
    W, H = reg.size
    rows = [y for y in range(H) if sum(1 for x in range(W) if reg.getpixel((x, y))) > 5]
    if not rows:
        return None
    y0, y1 = rows[0], rows[-1] + 1
    runs, cur = [], 0
    for x in range(W + 1):
        on = x < W and sum(1 for y in range(y0, y1) if reg.getpixel((x, y))) > 1
        if on:
            if cur == 0:
                start = x
            cur += 1
        elif cur:
            runs.append((start, cur))
            cur = 0
    if len(runs) < 8:
        return None
    chars = []
    for x0, wdt in runs:
        t = norm(reg.crop((x0, y0, x0 + wdt, y1)))
        best_ch, best = None, -1.0
        for ch, tmpl in templates.items():
            score = sum(1 for a, b in zip(t, tmpl) if a == b) / len(tmpl)
            if score > best:
                best_ch, best = ch, score
        chars.append(best_ch)
    return "".join(chars)


def parse_hms(txt: str):
    m = re.match(r"^(\d{2}):(\d{2}):(\d{2})\.(\d{3})", txt or "")
    if not m:
        return None
    h, mi, s, ms = (int(x) for x in m.groups())
    return ((h * 3600 + mi * 60 + s) * 1000) + ms


def clock_pid() -> int | None:
    out = subprocess.run(
        ["bash", "-lc", "ps -eo pid,args | grep nx_clock_layer | grep -v grep | awk '{print $1}' | head -1"],
        capture_output=True, text=True).stdout.strip()
    return int(out) if out else None


def main() -> int:
    ap = argparse.ArgumentParser()
    ap.add_argument("--samples", type=int, default=10)
    ap.add_argument("--outdir", default="/tmp/lat")
    args = ap.parse_args()
    os.makedirs(args.outdir, exist_ok=True)
    templates = build_templates()
    tz = host_utc_offset_ms()
    sh = Shell()
    sh.run(f"rm -rf {REMOTE}; mkdir -p {REMOTE}")

    # 1) смещение часов через постоянную сессию (round-trip 10-30 мс)
    samples = []
    for _ in range(15):
        out, t0, t1 = sh.timed("date +%s%3N")
        if out.strip():
            samples.append(int(out.strip()) - (t0 + t1) * 500.0)
    off = statistics.median(samples)
    spread = max(samples) - min(samples)
    print(f"смещение часов планшет−хост: {off:+.0f} мс (разброс {spread:.0f} мс), "
          f"зона хоста UTC{tz/3600000:+.0f}", flush=True)

    # 3) основные выборки
    calib = None
    lat = []
    for i in range(args.samples):
        out, t0, t1 = sh.timed(f"date +%s%3N > {REMOTE}/t.txt; screencap {REMOTE}/s{i}.raw")
        tc = int(sh.run(f"cat {REMOTE}/t.txt").strip())
        subprocess.run(["adb", "pull", f"{REMOTE}/s{i}.raw", f"{args.outdir}/s{i}.raw"],
                       capture_output=True)
        try:
            img = read_raw(f"{args.outdir}/s{i}.raw")
            txt = read_clock(img, templates)
            H = parse_hms(txt)
        except Exception:
            H, txt = None, None
        if H is None:
            print(f"  #{i}: часы не распознаны ({txt!r})")
            continue
        d = ((tc - off + tz) % 86400000) - H
        lat.append(d)
        print(f"  #{i}: кадр {txt} | задержка {d:5.0f} мс", flush=True)
    # 4) калибровка (после выборок: первый снимок после заморозки загрязнён): замораживаем часы (кадр становится статичным) и проверяем,
    #    что цепочка «время в кадре ↔ время съёма» сходится.
    pid = clock_pid()
    calib = None
    if pid:
        os.kill(pid, signal.SIGSTOP)
        frozen_at = time.time()
        time.sleep(0.4)
        out, t0, t1 = sh.timed(f"date +%s%3N > {REMOTE}/c.txt; screencap {REMOTE}/c.raw")
        tc = int(sh.run(f"cat {REMOTE}/c.txt").strip())
        subprocess.run(["adb", "pull", f"{REMOTE}/c.raw", f"{args.outdir}/c.raw"],
                       capture_output=True)
        try:
            img = read_raw(f"{args.outdir}/c.raw")
            F = parse_hms(read_clock(img, templates))
        except Exception:
            F = None
        os.kill(pid, signal.SIGCONT)
        if F is not None:
            # F — локальное время хоста, запечатанное в замороженном кадре
            # (на ~47 мс раньше самого SIGSTOP — часы обновляются раз в 8 мс).
            # Снимок сделан примерно через 0.42 с после заморозки, значит
            # «истинная» задержка на момент съёма = (frozen_at + 0.42) − F.
            # measured_raw = elapsed_true + (off_true − off_used) → отсюда
            # поправка к измеренному смещению часов.
            host_local_at_capture = (tc - off + tz) % 86400000
            elapsed_true = ((frozen_at + 0.42) * 1000 + tz) % 86400000 - F
            calib = host_local_at_capture - F - elapsed_true
            print(f"калибровка (замороженные часы): в кадре {F} мс, "
                  f"ожидаемая задержка {elapsed_true:.0f} мс, поправка {calib:+.0f} мс",
                  flush=True)

    if calib is not None:
        lat = [d + calib for d in lat]
        print(f"применена поправка калибровки {calib:+.0f} мс", flush=True)
    sh.close()
    if not lat:
        print("замеров нет")
        return 1
    lat_s = sorted(lat)
    print(f"\nзадержка: медиана {statistics.median(lat):.0f} мс, "
          f"мин {min(lat):.0f}, макс {max(lat):.0f} (n={len(lat)})")
    return 0


if __name__ == "__main__":
    sys.exit(main())
