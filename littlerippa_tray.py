#!/usr/bin/env python3
"""Tray icon helper for the Autosort window.

This runs as its own process because XApp's status icon links GTK 3, and GTK 3
and GTK 4 cannot load into one process. It exits when its stdin closes, so it
cannot outlive the app.

Protocol, one item per line:
    stdin:  JSON objects with any of "icon", "tooltip" and "label"
    stdout: "ready", "unavailable", "show" or "quit"
"""

import json
import sys

import gi

gi.require_version("Gtk", "3.0")
gi.require_version("XApp", "1.0")
from gi.repository import Gio, GLib, Gtk, XApp  # noqa: E402

DEFAULT_ICON = "io.github.cpntodd.YLRAuto-symbolic"
# The panel can start after this helper when both launch at login.
PANEL_WAIT_SECONDS = 30
PANEL_CHECK_SECONDS = 2


def send(message):
    print(message, flush=True)


class Tray:
    def __init__(self):
        self.seconds_waited = 0
        self.icon = None
        self.input = Gio.DataInputStream.new(Gio.UnixInputStream.new(sys.stdin.fileno(), False))
        self.input.read_line_async(GLib.PRIORITY_DEFAULT, None, self._on_line)
        if not self._try_create():
            GLib.timeout_add_seconds(PANEL_CHECK_SECONDS, self._retry_create)

    def _try_create(self):
        if not XApp.StatusIcon.any_monitors():
            return False
        self.icon = XApp.StatusIcon()
        self.icon.set_name("ylr-auto")
        self.icon.set_icon_name(DEFAULT_ICON)
        self.icon.set_tooltip_text("YLR-Auto")
        self.icon.connect("activate", lambda *_: send("show"))
        self.icon.set_secondary_menu(self._build_menu())
        send("ready")
        return True

    def _retry_create(self):
        if self._try_create():
            return False
        self.seconds_waited += PANEL_CHECK_SECONDS
        if self.seconds_waited >= PANEL_WAIT_SECONDS:
            send("unavailable")
            return False
        return True

    def _build_menu(self):
        menu = Gtk.Menu()
        for label, message in (("Show YLR-Auto", "show"), ("Quit", "quit")):
            item = Gtk.MenuItem(label=label)
            item.connect("activate", lambda _item, text=message: send(text))
            menu.append(item)
        menu.show_all()
        return menu

    def _on_line(self, stream, result):
        line, _length = stream.read_line_finish_utf8(result)
        if line is None:
            Gtk.main_quit()
            return
        self._apply(line)
        stream.read_line_async(GLib.PRIORITY_DEFAULT, None, self._on_line)

    def _apply(self, line):
        if self.icon is None:
            return
        state = json.loads(line)
        if "icon" in state:
            self.icon.set_icon_name(state["icon"])
        if "tooltip" in state:
            self.icon.set_tooltip_text(state["tooltip"])
        if "label" in state:
            self.icon.set_label(state["label"])


if __name__ == "__main__":
    tray = Tray()
    Gtk.main()
