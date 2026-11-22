"""Run with HYPE_FLOW_UI_TESTS=1 under dbus-run-session in a Wayland session."""

import os
import json
from pathlib import Path
import subprocess
import tempfile
import time
import unittest
from unittest.mock import Mock, patch


@unittest.skipUnless(os.environ.get("HYPE_FLOW_UI_TESTS") == "1", "requires isolated desktop session")
class DesktopTests(unittest.TestCase):
    def test_download_activates_new_model_and_keeps_preferences(self):
        from dictation.app import Application
        from dictation.config import load
        from dictation.ui import GLib

        engine = Mock()
        values = load()
        values.update(model="/old/model.bin", server="/old/whisper", language="auto", gpu=True,
                      prompt="Saved names", paste=False)
        paths = {"model": "/new/parakeet", "server": "/new/recognizer"}
        app = Application(values)
        errors = []
        deadline = time.monotonic() + 10
        started = False

        def drive():
            nonlocal started
            try:
                if time.monotonic() > deadline:
                    raise AssertionError("Model activation timed out")
                if not hasattr(app, "controller"):
                    return True
                if not started:
                    app.download()
                    started = True
                elif app.controller.ready:
                    self.assertEqual(values["model"], paths["model"])
                    self.assertEqual(values["server"], paths["server"])
                    self.assertEqual(values["prompt"], "Saved names")
                    self.assertFalse(values["paste"])
                    self.assertFalse(values["gpu"])
                    self.assertEqual(values["language"], "en")
                    app.quit()
                    return False
            except Exception as error:
                errors.append(error)
                app.quit()
                return False
            return True

        with tempfile.TemporaryDirectory() as directory, \
                patch("dictation.app.Engine", return_value=engine), \
                patch("dictation.app.config.runtime_dir", return_value=Path(directory)), \
                patch("dictation.app.config.save") as save, \
                patch("dictation.app.assets_ready", side_effect=lambda current: current["model"] == paths["model"]), \
                patch("dictation.setup.setup", return_value=paths):
            GLib.timeout_add(50, drive)
            app.run(["hype-flow", "--background"])
            self.assertFalse(errors, str(errors))
            save.assert_called_once_with(values)

    def test_settings_navigation_controls_and_compact_layout(self):
        from dictation.config import load
        from dictation.ui import Gio, GLib, Gtk, Home

        app = Gtk.Application(application_id="dev.hype.Flow", flags=Gio.ApplicationFlags.NON_UNIQUE)
        app.register(None)
        save, action = Mock(), Mock()
        home = Home(app, load(), Mock(), save, action)

        def settle():
            deadline = time.monotonic() + 0.25
            while time.monotonic() < deadline:
                while GLib.MainContext.default().iteration(False):
                    pass
                time.sleep(0.005)

        def capture(name):
            directory = os.environ.get("HYPE_FLOW_UI_CAPTURE_DIR")
            if directory:
                import cairo
                path = Path(directory)
                path.mkdir(parents=True, exist_ok=True)
                width, height = home.get_size()
                surface = cairo.ImageSurface(cairo.FORMAT_ARGB32, width, height)
                home.draw(cairo.Context(surface))
                surface.write_to_png(str(path / (name + ".png")))

        try:
            home.present()
            settle()
            windows = json.loads(subprocess.check_output(["niri", "msg", "--json", "windows"]))
            window = next(window for window in windows if window["pid"] == os.getpid() and window["title"] == "Hype Flow")
            self.assertEqual(window["app_id"], "dev.hype.Flow")
            subprocess.run(["niri", "msg", "action", "move-window-to-floating", "--id", str(window["id"])], check=True)
            settle()
            subprocess.run(["niri", "msg", "action", "set-window-width", "--id", str(window["id"]), "960"], check=True)
            subprocess.run(["niri", "msg", "action", "set-window-height", "--id", str(window["id"]), "680"], check=True)
            home.update("idle", "Ready", True)
            settle()
            for index, name in enumerate(("general", "model", "vocabulary", "cleanup", "shortcuts")):
                home.navigation.select_row(home.navigation.get_row_at_index(index))
                settle()
                self.assertEqual(home.stack.get_visible_child_name(), name)
                capture(name)
            self.assertTrue(home.prompt.get_sensitive())
            home.prompt.set_text("नमस्ते, niri")
            home.paste.set_active(False)
            home.corrections_buffer.set_text("open ai => OpenAI")
            home.blocked_buffer.set_text("um, uh, unwanted phrase")
            home.boost_score.set_value(2.0)
            home.save_button.clicked()
            save.assert_called_once()
            changes = save.call_args.args[0]
            self.assertEqual(changes["prompt"], "नमस्ते, niri")
            self.assertEqual(changes["corrections"], "open ai => OpenAI")
            self.assertEqual(changes["blocked_words"], "um, uh, unwanted phrase")
            self.assertEqual(changes["boost_score"], 2.0)
            self.assertTrue(changes["filter_words"])
            self.assertFalse(changes["remove_repetitions"])
            home.update("error", "Test error details", True)
            self.assertEqual(home.status.get_text(), "Test error details")
            self.assertEqual(home.badge.get_text(), "Needs attention")
            self.assertTrue(home.status.get_style_context().has_class("error"))
            home.update("idle", "Ready", False)
            self.assertTrue(home.download_button.get_visible())
            home.set_downloading(True)
            self.assertFalse(home.download_button.get_sensitive())
            home.set_downloading(False)
            home.navigation.select_row(home.navigation.get_row_at_index(0))
            subprocess.run(["niri", "msg", "action", "set-window-width", "--id", str(window["id"]), "660"], check=True)
            subprocess.run(["niri", "msg", "action", "set-window-height", "--id", str(window["id"]), "620"], check=True)
            settle()
            self.assertEqual(home.compact, (True, True))
            self.assertFalse(home.navigation_labels[0].get_visible())
            self.assertTrue(all(row.get_orientation() == Gtk.Orientation.VERTICAL for row, _ in home.rows))
            capture("compact")
        finally:
            home.destroy()

    def test_launcher_dictation_and_notch_handshake(self):
        from dictation.app import Application
        from dictation.config import load
        from dictation.ui import GLib

        engine = Mock()
        engine.transcribe.return_value = "Um, open ai works."
        failures, observed = [], []
        phase = 0
        deadline = time.monotonic() + 15
        notch = None
        script = os.environ.get("HYPE_FLOW_NOTCH_SCRIPT")
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "curl").write_text('#!/bin/sh\nprintf "%s" "Clear|+20°C|+20°C|40%|Test"\n')
            (directory / "curl").chmod(0o700)
            if script:
                notch = subprocess.Popen(["bash", script], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
                                          env={**os.environ, "PATH": str(directory) + ":" + os.environ["PATH"],
                                               "GTK_USE_PORTAL": "0", "NO_AT_BRIDGE": "1"})
            values = load()
            values.update(model=__file__, server=__file__, corrections="open ai => OpenAI", prompt="")
            runtime = directory / "runtime"
            runtime.mkdir(mode=0o700)
            app = Application(values)

            class FakeRecorder:
                def __init__(self, target, on_frame, on_end, hold=False):
                    self.on_frame = on_frame

                def start(self):
                    self.on_frame(0.5, 0.1)

                def stop(self):
                    return b"\x01\x00" * 1600

            def drive():
                nonlocal phase
                try:
                    if time.monotonic() > deadline:
                        raise AssertionError(f"Desktop pipeline timed out in phase {phase}")
                    if not hasattr(app, "controller"):
                        return True
                    if phase == 0 and app.controller.ready and (not script or app.notch_available):
                        app.action("toggle")
                        phase = 1
                    elif phase == 1 and app.controller.state == "recording":
                        observed.append("recording")
                        self.assertEqual(app.overlay.window.get_visible(), not bool(script))
                        app.action("toggle")
                        phase = 2
                    elif phase == 2 and app.controller.state == "done":
                        observed.append("done")
                        self.assertEqual((app.runtime / "last-transcript.txt").read_text(), "OpenAI works.")
                        self.assertEqual((app.runtime / "last-raw-transcript.txt").read_text(), "Um, open ai works.")
                        self.assertEqual(app.controller.message, "Pasted")
                        app.action("copy-raw")
                        app.save({"blocked_words": "um, uh, erm"})
                        self.assertTrue(app.controller.ready)
                        engine.ensure.assert_called()
                        app.quit()
                        return False
                except Exception as error:
                    failures.append(error)
                    app.quit()
                    return False
                return True

            GLib.timeout_add(50, drive)
            try:
                with patch("dictation.app.Engine", return_value=engine), \
                        patch("dictation.app.assets_ready", return_value=True), \
                        patch("dictation.app.config.runtime_dir", return_value=runtime), \
                        patch("dictation.app.config.save"), \
                        patch("dictation.app.copy_text") as copy_original, \
                        patch("dictation.controller.Recorder", FakeRecorder), \
                        patch("dictation.controller.focused_window", return_value={"id": 999}), \
                        patch("dictation.controller.deliver", return_value="Pasted") as deliver:
                    app.run(["hype-flow", "--background"])
                self.assertFalse(failures, str(failures))
                self.assertEqual(observed, ["recording", "done"])
                deliver.assert_called_once_with("OpenAI works.", {"id": 999}, True, trailing_space=False, press_enter=False)
                copy_original.assert_called_once_with("Um, open ai works.")
            finally:
                if notch:
                    notch.terminate()
                    try:
                        notch.wait(timeout=5)
                    except subprocess.TimeoutExpired:
                        notch.kill()
                        notch.wait()
