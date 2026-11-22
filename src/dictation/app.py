"""Launcher-first GTK application. GApplication also owns niri's private controls."""

import os
from pathlib import Path
import signal
import sys

from . import config
from .audio import Capture
from .backend import Engine
from .controller import Controller
from .output import copy_text
from .setup import assets_ready
from .ui import Gio, GLib, Gtk, Home, Overlay


class Application(Gtk.Application):
    def __init__(self, values):
        super().__init__(application_id="dev.hype.Flow", flags=Gio.ApplicationFlags.HANDLES_COMMAND_LINE)
        self.values = values
        self.notch_available = False
        self.notch_actions = None
        self.last_state = ("idle", "Ready", False, 0.0, 0.0)
        self.downloading = False
        self.unload_token = 0
        for name in ("background", "toggle", "cancel"):
            self.add_main_option(name, 0, GLib.OptionFlags.NONE, GLib.OptionArg.NONE, "Desktop integration", None)
        self.connect("startup", self.startup)
        self.connect("command-line", self.command_line)
        self.connect("shutdown", self.shutdown)

    def startup(self, _):
        self.hold()
        self.runtime = config.runtime_dir()
        self.status_action = Gio.SimpleAction.new_stateful("status", None, GLib.Variant("(ssddb)", ("idle", "Ready", 0.0, 0.0, False)))
        self.add_action(self.status_action)
        for name in ("toggle", "cancel", "show", "copy-last", "copy-raw", "quit"):
            action = Gio.SimpleAction.new(name, None)
            action.connect("activate", lambda action, _: self.action(action.get_name()))
            self.add_action(action)
        self.overlay = Overlay(self.action)
        self.home = Home(self, self.values, self.download, self.save, self.action)
        self.engine = Engine(self.values, self.runtime)
        self.controller = Controller(self.values, self.runtime, self.engine, self.update, GLib.idle_add)
        self.notch_watch = Gio.bus_watch_name_on_connection(self.get_dbus_connection(), "dev.hype_niri.Notch",
                                                           Gio.BusNameWatcherFlags.NONE, self.notch_appeared, self.notch_vanished)
        for number in (signal.SIGTERM, signal.SIGINT):
            GLib.unix_signal_add(GLib.PRIORITY_DEFAULT, number, self.quit_signal)
        if self.assets_ready():
            self.controller.warm()
        else:
            self.controller.change("idle", "Download the speech model to get started", visible=False)

    def assets_ready(self):
        return assets_ready(self.values)

    def command_line(self, _, command):
        options = command.get_options_dict()
        if options.contains("toggle"):
            self.action("toggle")
        elif options.contains("cancel"):
            self.action("cancel")
        elif not options.contains("background"):
            self.home.present()
        return 0

    def action(self, name):
        if name == "show":
            self.home.present()
        elif name == "quit":
            self.quit()
        elif name in ("copy-last", "copy-raw"):
            try:
                path = self.runtime / ("last-raw-transcript.txt" if name == "copy-raw" else "last-transcript.txt")
                if not path.is_file():
                    raise RuntimeError("No transcript in this session yet")
                text = path.read_text()
                if not text:
                    raise RuntimeError("All words were filtered. Use Copy original to recover the unedited transcript.")
                copy_text(text)
                self.home.feedback("Copied original transcript" if name == "copy-raw" else "Copied last transcript")
            except (OSError, RuntimeError) as error:
                self.home.feedback(str(error))
        elif name == "toggle" and (not self.assets_ready() or self.downloading):
            self.home.present()
        else:
            self.controller.command(name)

    def update(self, state, message, visible=True, volume=0.0, duration=0.0):
        self.last_state = (state, message, visible, volume, duration)
        self.status_action.set_state(GLib.Variant("(ssddb)", (state, message, volume, duration, visible)))
        self.overlay.update(state, message, visible and not self.notch_available, volume, duration)
        self.home.update(state, message, self.assets_ready())
        self.schedule_unload(state)
        if state == "done":
            generation = self.controller.generation

            def settle():
                if self.controller.generation == generation and self.controller.state == "done":
                    self.controller.change("idle", "Ready", visible=False)
                return False

            GLib.timeout_add(1800, settle)

    def schedule_unload(self, state):
        """Release the model after the chosen idle time; any activity restarts the countdown."""
        self.unload_token += 1
        minutes = self.values.get("unload_minutes", 0)
        if state not in ("idle", "done") or not minutes or not self.controller.ready or self.controller.unloaded:
            return
        token = self.unload_token

        def unload():
            if token == self.unload_token:
                self.controller.unload()
            return False

        GLib.timeout_add_seconds(minutes * 60, unload)

    def notch_appeared(self, connection, name, _owner):
        self.notch_actions = Gio.DBusActionGroup.get(connection, name, "/dev/hype_niri/Notch")
        self.notch_actions.connect("action-added", self.notch_changed)
        self.notch_actions.connect("action-removed", self.notch_changed)
        self.notch_changed(self.notch_actions, "dictation")

    def notch_changed(self, actions, _name):
        self.notch_available = actions.has_action("dictation")
        state, message, visible, volume, duration = self.last_state
        self.overlay.update(state, message, visible and not self.notch_available, volume, duration)

    def notch_vanished(self, _connection, _name):
        self.notch_available = False
        self.notch_actions = None
        state, message, visible, volume, duration = self.last_state
        self.overlay.update(state, message, visible, volume, duration)

    def download(self):
        if self.downloading or self.controller.busy:
            return
        from .setup import setup
        self.downloading = True
        self.home.set_downloading(True)

        def progress(message):
            GLib.idle_add(self.home.feedback, message)

        def finished(paths, error):
            self.downloading = False
            self.home.set_downloading(False)
            if error:
                self.controller.change("error", str(error), visible=False)
            else:
                self.values.update(paths, language="en", gpu=False)
                try:
                    config.save(self.values)
                except (OSError, ValueError) as failure:
                    self.controller.change("error", str(failure), visible=False)
                    return
                self.controller.warm()

        self.controller.work(lambda: setup(progress), finished)

    def save(self, changes):
        if self.controller.busy or self.controller.state in ("recording", "starting"):
            self.home.feedback("Finish or cancel dictation before changing settings")
            return
        updated = {**self.values, **changes}
        reload_model = any(updated[key] != self.values[key] for key in
                           ("model", "server", "language", "threads", "prompt", "vocabulary_boosting", "boost_score",
                            "cut_silence", "noise_suppression"))
        try:
            config.save(updated)
        except (OSError, ValueError) as error:
            self.home.feedback(str(error))
            return
        self.values.update(updated)
        if not self.values["keep_mic_ready"]:
            Capture.release()  # Close a microphone held open from the last dictation right away.
        if reload_model or not self.controller.ready:
            self.engine.stop()
            self.engine = Engine(self.values, self.runtime)
            self.controller.engine = self.engine
            if self.assets_ready():
                self.controller.warm()
        self.home.feedback("Settings saved")
        return True

    def quit_signal(self):
        self.quit()
        return False

    def shutdown(self, _):
        self.controller.close()
        Gio.bus_unwatch_name(self.notch_watch)
        (self.runtime / "last-transcript.txt").unlink(missing_ok=True)
        (self.runtime / "last-raw-transcript.txt").unlink(missing_ok=True)


def main():
    os.umask(0o077)
    try:
        return Application(config.load()).run(sys.argv)
    except (OSError, RuntimeError, ValueError) as error:
        dialog = Gtk.MessageDialog(message_type=Gtk.MessageType.ERROR, buttons=Gtk.ButtonsType.CLOSE,
                                   text="Hype Flow could not start")
        dialog.format_secondary_text(str(error))
        dialog.run()
        dialog.destroy()
        return 1
