#!/usr/bin/env python3
"""Живые часы хоста (мс) строго на виртуальном мониторе HEADLESS-1.

Зачем: в кадре стрима должно быть live-время хоста, чтобы объективно
измерить сквозную задержку (кадр на планшете vs время съёма экрана).
Физический экран пользователя (eDP-1) трогать нельзя — поэтому окно
создаётся как layer-shell поверхность и насильно привязывается к
монитору HEADLESS-1 (gtk-layer-shell умеет set_monitor).

Запуск:
    XDG_RUNTIME_DIR="$XDG_RUNTIME_DIR" WAYLAND_DISPLAY=wayland-1 \
        python3 nx_clock_layer.py [имя-монитора]
"""
import sys
import time

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
gi.require_version("GtkLayerShell", "0.1")
from gi.repository import Gdk, GLib, Gtk, GtkLayerShell  # noqa: E402


def pick_monitor(want: str):
    """Выбираем монитор по имени модели или по индексу.

    У headless-выхода Hyprland модель пустая (в Gdk он идёт как «1:»), поэтому
    по имени его не найти — принимаем индекс.
    """
    display = Gdk.Display.get_default()
    if display is None:
        return None
    if want.isdigit():
        idx = int(want)
        if idx < display.get_n_monitors():
            return display.get_monitor(idx)
        return None
    names = []
    for i in range(display.get_n_monitors()):
        mon = display.get_monitor(i)
        model = mon.get_model() or ""
        names.append(f"{i}:{model!r}")
        if want.lower() in model.lower():
            return mon
    print("мониторы:", ", ".join(names), file=sys.stderr)
    return None


def main() -> int:
    want = sys.argv[1] if len(sys.argv) > 1 else "HEADLESS"
    win = Gtk.Window()
    win.set_title("nx-clock")
    try:
        win.set_decorated(False)
    except Exception:
        pass
    GtkLayerShell.init_for_window(win)
    GtkLayerShell.set_layer(win, GtkLayerShell.Layer.TOP)
    GtkLayerShell.set_namespace(win, "nxclock")
    GtkLayerShell.set_exclusive_zone(win, -1)
    mon = pick_monitor(want)
    if mon is None:
        # Никогда не показываем окно без явного монитора: иначе часы
        # окажутся на физическом экране пользователя (eDP-1).
        print("ВНИМАНИЕ: монитор не найден — окно НЕ показываю (чтобы не "
              "попасть на eDP-1)", file=sys.stderr)
        return 1
    GtkLayerShell.set_monitor(win, mon)
    # Растягиваем поверхность на весь headless-выход, но часы прижимаем в
    # левый НИЖНИЙ угол: перф-оверлей клиента (Moonlight) рисуется в левом
    # верхнем углу кадра и не должен перекрывать время.
    for edge, anchor in ((GtkLayerShell.Edge.TOP, False),
                         (GtkLayerShell.Edge.BOTTOM, True),
                         (GtkLayerShell.Edge.LEFT, True),
                         (GtkLayerShell.Edge.RIGHT, False)):
        GtkLayerShell.set_anchor(win, edge, anchor)
    for marg in (GtkLayerShell.Edge.TOP, GtkLayerShell.Edge.BOTTOM,
                 GtkLayerShell.Edge.LEFT, GtkLayerShell.Edge.RIGHT):
        GtkLayerShell.set_margin(win, marg, 0)
    print("часы на мониторе:", repr(mon.get_model()), "index",
          mon.get_index() if hasattr(mon, "get_index") else "?", file=sys.stderr)

    label = Gtk.Label()
    label.set_halign(Gtk.Align.START)
    label.set_valign(Gtk.Align.START)
    box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL)
    box.add(label)
    win.add(box)
    # Шрифт задаём один раз через CSS, дальше только set_text: set_markup на
    # каждом тике заново парсит Pango-разметку и переверстывает метку — это
    # 13-16% CPU на 4-ядерном хосте (а хост уже в 86% загрузки).
    css = b"""
    .nxclock { font-family: monospace; font-size: 60pt; color: #ffffff;
               background-color: #000000; padding: 4px 12px; }
    """
    prov = Gtk.CssProvider()
    prov.load_from_data(css)
    Gtk.StyleContext.add_provider_for_screen(
        Gdk.Screen.get_default(), prov,
        Gtk.STYLE_PROVIDER_PRIORITY_APPLICATION)
    label.get_style_context().add_class("nxclock")
    # Во весь headless-выход: в кадре стрима (1088x800) часы всегда в одном
    # месте — левый верхний угол, крупно. Чёрный фон = надёжное чтение.
    gm = mon.get_geometry()
    win.set_default_size(gm.width, gm.height)
    win.move(0, 0)

    def tick():
        t = time.time()
        ms = int((t % 1.0) * 1000)
        label.set_text(time.strftime("%H:%M:%S", time.localtime(t)) + f".{ms:03d}")
        return True

    GLib.timeout_add(8, tick)
    win.connect("destroy", Gtk.main_quit)
    win.show_all()
    Gtk.main()
    return 0


if __name__ == "__main__":
    sys.exit(main())
