#!/usr/bin/env python3
"""Minimal GTK3 text-input-v3 client for the input-method harness.

One window, one Gtk.Entry that grabs focus when the window is mapped.
Everything interesting is printed to stdout, line buffered:

    READY                 window mapped and the entry has focus
    ENTRY <text>          the entry's buffer changed (committed text)
    PREEDIT <text>        the entry's preedit string changed
    KEY <keyname>         a key press reached the widget (debugging aid)
    BYE                   window closed

Run it only through run-entry-client.sh, which points it at the NESTED
compositor and forces the GTK "wayland" IM module so that GTK speaks
zwp_text_input_v3 to the compositor instead of talking to ibus directly.
"""

import sys
import gi

gi.require_version("Gtk", "3.0")
gi.require_version("Gdk", "3.0")
from gi.repository import Gtk, Gdk, GLib  # noqa: E402


def emit(line):
    sys.stdout.write(line + "\n")
    sys.stdout.flush()


class App:
    def __init__(self, title):
        self.win = Gtk.Window(title=title)
        self.win.set_default_size(640, 160)
        box = Gtk.Box(orientation=Gtk.Orientation.VERTICAL, spacing=8)
        box.set_border_width(12)
        self.entry = Gtk.Entry()
        self.entry.set_placeholder_text("type here")
        self.label = Gtk.Label(label="preedit: ")
        self.label.set_xalign(0.0)
        box.pack_start(self.entry, False, False, 0)
        box.pack_start(self.label, False, False, 0)
        self.win.add(box)

        self.entry.connect("changed", self.on_changed)
        self.entry.connect("preedit-changed", self.on_preedit)
        self.entry.connect("key-press-event", self.on_key)
        self.win.connect("destroy", self.on_destroy)
        self.win.connect("map-event", self.on_map)

    def on_changed(self, entry):
        emit("ENTRY %s" % entry.get_text())

    def on_preedit(self, entry, preedit):
        self.label.set_text("preedit: %s" % preedit)
        emit("PREEDIT %s" % preedit)

    def on_key(self, _widget, event):
        emit("KEY %s" % (Gdk.keyval_name(event.keyval) or "?"))
        return False

    def on_map(self, *_args):
        self.entry.grab_focus()
        # Give the compositor a beat to deliver keyboard focus before we claim
        # to be ready, otherwise a test can inject keys into nothing.
        GLib.timeout_add(200, self.announce_ready)
        return False

    def announce_ready(self):
        display = Gdk.Display.get_default()
        emit("BACKEND %s" % type(display).__name__)
        emit("READY")
        return False

    def on_destroy(self, *_args):
        emit("BYE")
        Gtk.main_quit()

    def run(self):
        self.win.show_all()
        Gtk.main()


if __name__ == "__main__":
    title = sys.argv[1] if len(sys.argv) > 1 else "im-harness entry"
    App(title).run()
