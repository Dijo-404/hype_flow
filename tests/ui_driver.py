#!/usr/bin/env python3
"""Exercise the production Rust app in an isolated D-Bus/Wayland session."""

import json
import os
import signal
import subprocess
import sys
import tempfile
import time
from pathlib import Path

import gi

gi.require_version("Gtk", "4.0")
from gi.repository import Gio, GLib, Gtk  # noqa: E402, RUF100


def wait(predicate, description, timeout=15):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        while GLib.MainContext.default().iteration(False):
            pass
        result = predicate()
        if result:
            return result
        time.sleep(0.01)
    raise AssertionError("Timed out: " + description)


def focused():
    return json.loads(
        subprocess.check_output(["niri", "msg", "--json", "focused-window"], timeout=3)
    )


def overlay_visible():
    layers = json.loads(
        subprocess.check_output(["niri", "msg", "--json", "layers"], timeout=3)
    )

    def contains(value):
        if isinstance(value, dict):
            return value.get("namespace") == "hype-flow" or any(
                contains(item) for item in value.values()
            )
        return isinstance(value, list) and any(contains(item) for item in value)

    return contains(layers)


def write_executable(path, text):
    path.write_text(text)
    path.chmod(0o700)


def main():
    executable = str(Path(sys.argv[1]).resolve())
    Gtk.init()
    target_app = Gtk.Application(
        application_id="dev.hype.Flow.TestTarget", flags=Gio.ApplicationFlags.NON_UNIQUE
    )
    target_app.register(None)
    window = Gtk.ApplicationWindow(
        application=target_app, title="Hype Flow controlled paste target"
    )
    editor = Gtk.TextView()
    window.set_child(editor)
    window.set_default_size(600, 350)
    window.present()
    editor.grab_focus()
    target = wait(
        lambda: (
            current
            if (current := focused()) and current.get("pid") == os.getpid()
            else None
        ),
        "controlled target focus",
    )

    with tempfile.TemporaryDirectory(prefix="hype-flow-ui-") as temporary:
        root = Path(temporary)
        data = root / "data/hype-flow"
        model = data / "models/parakeet-110m-ctc-en-int8"
        backend = data / "backend/parakeet"
        config = root / "config/hype-flow"
        fake = root / "bin"
        for directory in (model, backend, config, fake):
            directory.mkdir(parents=True)
        for name in ("model.int8.onnx", "tokens.txt"):
            (model / name).write_bytes(b"0")
        for name in ("silero_vad.onnx", "gtcrn_simple.onnx"):
            (model.parent / name).write_bytes(b"0")
        (backend / "recognizer.version").write_text("9\n")
        # The sandboxed worker cannot write host files, so it reports through its log.
        worker_log = Path(os.environ["XDG_RUNTIME_DIR"]) / "hype-flow/backend.log"
        worker_marker = f"fake worker started in {root}"

        def worker_started():
            return worker_log.exists() and worker_marker in worker_log.read_text()

        write_executable(
            backend / "recognizer",
            "#!"
            + sys.executable
            + "\n"
            + r"""
import json, os, struct, sys, time
print(os.environ["HYPE_TEST_WORKER_STARTED"], file=sys.stderr, flush=True)
def send(value):
    data = json.dumps(value).encode()
    sys.stdout.buffer.write(struct.pack("<I", len(data)) + data)
    sys.stdout.buffer.flush()
send({"ready": True})
while True:
    header = sys.stdin.buffer.read(4)
    if not header:
        break
    count = struct.unpack("<I", header)[0]
    remaining = count * 2
    while remaining:
        data = sys.stdin.buffer.read(remaining)
        if not data:
            sys.exit(1)
        remaining -= len(data)
    time.sleep(0.4)
    send({"text": "um open ai hello."})
""",
        )
        write_executable(
            fake / "pw-record",
            "#!"
            + sys.executable
            + "\n"
            + r"""
import os, sys, time
with open(os.environ["HYPE_TEST_PID_FILE"], "a") as log:
    log.write(str(os.getpid()) + "\n")
while True:
    sys.stdout.buffer.write(bytes([1, 0]) * 800)
    sys.stdout.buffer.flush()
    time.sleep(1 if os.path.exists(os.environ["HYPE_TEST_SHORT_CAPTURE"]) else 0.05)
""",
        )
        write_executable(fake / "pw-dump", "#!/bin/sh\nprintf '[]'\n")
        write_executable(
            fake / "niri",
            "#!"
            + sys.executable
            + "\n"
            + r"""
import os, sys, time
if sys.argv[1:] == ["msg", "--json", "focused-window"] and os.path.exists(os.environ["HYPE_TEST_DELAY"]):
    time.sleep(0.4)
os.execv("/usr/bin/niri", ["/usr/bin/niri", *sys.argv[1:]])
""",
        )
        (config / "config.toml").write_text(
            'recover_recordings = true\nsounds = false\npause_media = false\nkeep_mic_ready = true\ncorrections = "open ai => OpenAI"\ndictation_shortcut = "Alt+Space"\n'
        )
        niri_config = root / "config/niri/config.kdl"
        niri_config.parent.mkdir(parents=True)
        niri_config.write_text('input { keyboard { xkb { layout "us"; }; }; }\n')
        env = {
            **os.environ,
            "XDG_DATA_HOME": str(root / "data"),
            "XDG_CONFIG_HOME": str(root / "config"),
            "XDG_STATE_HOME": str(root / "state"),
            "NIRI_CONFIG": str(niri_config),
            "PATH": str(fake) + ":" + os.environ["PATH"],
            "HYPE_TEST_PID_FILE": str(root / "capture-pids"),
            "HYPE_TEST_DELAY": str(root / "focus-delay"),
        }
        short_capture = root / "short-capture"
        env["HYPE_TEST_SHORT_CAPTURE"] = str(short_capture)
        env["HYPE_TEST_WORKER_STARTED"] = worker_marker
        log = (root / "app.log").open("wb")
        app = subprocess.Popen(
            [executable, "--background"],
            env=env,
            stdout=log,
            stderr=log,
            start_new_session=True,
        )
        group = Gio.DBusActionGroup.get(
            Gio.bus_get_sync(Gio.BusType.SESSION, None),
            "dev.hype.Flow",
            "/dev/hype/Flow",
        )

        def state():
            assert app.poll() is None, "Rust app exited unexpectedly"
            variant = group.get_action_state("status")
            if variant:
                assert variant.get_type_string() == "(ssddb)"
                return variant.unpack()

        def phase(name):
            current = state()
            return current and current[0] == name

        def text():
            buffer = editor.get_buffer()
            return buffer.get_text(buffer.get_start_iter(), buffer.get_end_iter(), True)

        try:
            wait(lambda: group.has_action("toggle") and phase("idle"), "ready actions")
            assert not worker_started(), "Speech worker started before dictation"
            for name in ("toggle", "cancel", "show", "copy-last", "copy-raw", "quit"):
                assert group.has_action(name), name
            short_capture.touch()
            group.activate_action("toggle", None)
            wait(lambda: phase("recording"), "short recording starts")
            wait(worker_started, "first dictation starts the speech worker")
            group.activate_action("toggle", None)
            wait(lambda: phase("done"), "short recording rejected")
            assert "Recording too short" in state()[1], state()
            short_capture.unlink()
            group.activate_action("toggle", None)
            wait(
                lambda: phase("recording"),
                "single shortcut starts again after a short recording",
                timeout=4,
            )
            group.activate_action("cancel", None)
            wait(lambda: phase("idle"), "cancel retried recording")
            group.activate_action("shortcuts", None)
            wait(
                lambda: niri_config.with_name("hype-flow.kdl").exists(),
                "custom shortcut installation",
            )
            bindings = niri_config.with_name("hype-flow.kdl").read_text()
            assert "Alt+Space" in bindings and "Mod+Ctrl+Space" not in bindings
            assert "Mod+Ctrl+Escape" in bindings
            subprocess.run(
                ["wl-copy", "--sensitive"], input=b"previous clipboard", check=True
            )
            group.activate_action("toggle", None)
            wait(lambda: phase("recording"), "microphone ready")
            assert focused()["id"] == target["id"], "Overlay stole focus"
            wait(overlay_visible, "fallback layer surface")
            connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            notch_actions = Gio.SimpleActionGroup()
            notch_action = Gio.SimpleAction.new("dictation", None)
            notch_actions.add_action(notch_action)
            exported = connection.export_action_group(
                "/dev/hype_niri/Notch", notch_actions
            )
            owner = Gio.bus_own_name_on_connection(
                connection,
                "dev.hype_niri.Notch",
                Gio.BusNameOwnerFlags.NONE,
                None,
                None,
            )
            try:
                wait(lambda: not overlay_visible(), "notch suppresses fallback overlay")
                assert focused()["id"] == target["id"], "Notch handover stole focus"
                notch_actions.remove_action("dictation")
                wait(overlay_visible, "notch action removal restores fallback")
                notch_actions.add_action(notch_action)
                wait(
                    lambda: not overlay_visible(),
                    "notch action return suppresses fallback",
                )
            finally:
                Gio.bus_unown_name(owner)
                connection.unexport_action_group(exported)
            wait(overlay_visible, "notch disappearance restores fallback")
            wait(overlay_visible, "fallback overlay")
            connection = Gio.bus_get_sync(Gio.BusType.SESSION, None)
            notch = Gio.SimpleActionGroup()
            notch.add_action(Gio.SimpleAction.new("dictation", None))
            exported = connection.export_action_group("/dev/hype_niri/Notch", notch)
            owner = Gio.bus_own_name_on_connection(
                connection,
                "dev.hype_niri.Notch",
                Gio.BusNameOwnerFlags.NONE,
                None,
                None,
            )
            wait(lambda: not overlay_visible(), "notch suppresses fallback overlay")
            notch.remove_action("dictation")
            wait(overlay_visible, "missing notch action restores fallback")
            notch.add_action(Gio.SimpleAction.new("dictation", None))
            wait(lambda: not overlay_visible(), "notch action restores handover")
            Gio.bus_unown_name(owner)
            connection.unexport_action_group(exported)
            wait(overlay_visible, "notch disappearance restores fallback")
            wait(lambda: state()[3] >= 0.35, "minimum recording duration before paste")
            group.activate_action("toggle", None)
            try:
                wait(lambda: phase("done"), "transcript delivery")
                wait(lambda: text() == "OpenAI hello.", "paste into original target")
            except AssertionError as error:
                raise AssertionError(
                    f"{error}. Status: {state()!r}, target text: {text()!r}"
                ) from error
            wait(
                lambda: (
                    subprocess.check_output(["wl-paste", "--no-newline"])
                    == b"previous clipboard"
                ),
                "clipboard restoration",
            )
            runtime = Path(os.environ["XDG_RUNTIME_DIR"]) / "hype-flow"
            raw = runtime / "last-raw-transcript.txt"
            cleaned = runtime / "last-transcript.txt"
            assert raw.read_text() == "um open ai hello."
            assert cleaned.read_text() == "OpenAI hello."
            assert raw.stat().st_mode & 0o777 == 0o600
            group.activate_action("copy-raw", None)
            wait(
                lambda: (
                    subprocess.check_output(["wl-paste", "--no-newline"])
                    == b"um open ai hello."
                ),
                "original recovery",
            )
            group.activate_action("toggle", None)
            wait(lambda: phase("recording"), "second recording")
            wait(lambda: state()[3] >= 0.35, "minimum recording duration before decode")
            group.activate_action("toggle", None)
            wait(lambda: phase("transcribing"), "second decode")
            group.activate_action("cancel", None)
            wait(lambda: phase("idle"), "cancelled decode")
            until = time.monotonic() + 0.8
            while time.monotonic() < until:
                while GLib.MainContext.default().iteration(False):
                    pass
                time.sleep(0.01)
            assert text() == "OpenAI hello.", "Cancelled decode pasted late text"
            subprocess.run([executable, "--cancel"], env=env, check=True, timeout=5)
            subprocess.run([executable], env=env, check=True, timeout=5)

            def settings_focused():
                current = focused()
                return (
                    current
                    and current["app_id"] == "dev.hype.Flow"
                    and current["pid"] == app.pid
                )

            def keys(*args):
                subprocess.run(["wtype", *args], check=True, timeout=5)

            wait(settings_focused, "launcher settings window")
            for key in ("w", "q"):
                keys(
                    "-M", "ctrl", "-M", "shift", "-k", key, "-m", "shift", "-m", "ctrl"
                )
                time.sleep(0.15)
                assert settings_focused() and app.poll() is None, (
                    f"Ctrl+Shift+{key.upper()} closed settings"
                )
            keys("-M", "ctrl", "-k", "w", "-m", "ctrl")
            wait(
                lambda: (
                    not any(
                        item.get("pid") == app.pid
                        for item in json.loads(
                            subprocess.check_output(
                                ["niri", "msg", "--json", "windows"]
                            )
                        )
                    )
                ),
                "Ctrl+W hides settings",
            )
            assert app.poll() is None
            subprocess.run([executable], env=env, check=True, timeout=5)
            wait(settings_focused, "launcher reopens settings in the same process")
            keys("-M", "ctrl", "-k", "q", "-m", "ctrl")
            app.wait(timeout=8)
            assert app.returncode == 0
            assert not raw.exists() and not cleaned.exists(), (
                "Transcript not removed at exit"
            )
            log.close()
            diagnostics = (root / "app.log").read_text()
            assert "panicked" not in diagnostics and "CRITICAL" not in diagnostics, (
                diagnostics
            )
            print(
                "Rust desktop integration: actions/status, capture, no-focus overlay, cleanup, paste, clipboard restoration, raw recovery, cancelled decode, launcher and shutdown passed"
            )
            for scenario in ("pending openers", "delivery shutdown"):
                subprocess.run(
                    [
                        "niri",
                        "msg",
                        "action",
                        "focus-window",
                        "--id",
                        str(target["id"]),
                    ],
                    check=True,
                )
                editor.grab_focus()
                wait(lambda: focused()["id"] == target["id"], "test target focus")
                log = (root / "app.log").open("ab")
                app = subprocess.Popen(
                    [executable, "--background"],
                    env=env,
                    stdout=log,
                    stderr=log,
                    start_new_session=True,
                )
                group = Gio.DBusActionGroup.get(
                    Gio.bus_get_sync(Gio.BusType.SESSION, None),
                    "dev.hype.Flow",
                    "/dev/hype/Flow",
                )
                wait(
                    lambda group=group: group.has_action("toggle") and phase("idle"),
                    "restart ready",
                )
                before = text()
                if scenario == "pending openers":
                    (root / "focus-delay").touch()
                    for _ in range(4):
                        group.activate_action("toggle", None)
                        wait(lambda: phase("recording"), "rapid recording")
                        group.activate_action("cancel", None)
                        wait(lambda: phase("idle"), "rapid cancellation")
                else:
                    group.activate_action("toggle", None)
                    wait(lambda: phase("recording"), "shutdown recording")
                    wait(
                        lambda: state()[3] >= 0.35,
                        "minimum recording duration before delivery",
                    )
                    group.activate_action("toggle", None)
                    wait(lambda: phase("delivering"), "delivery before shutdown")
                group.activate_action("quit", None)
                app.wait(timeout=8)
                assert app.returncode == 0
                (root / "focus-delay").unlink(missing_ok=True)
                deadline = time.monotonic() + 0.4
                while time.monotonic() < deadline:
                    while GLib.MainContext.default().iteration(False):
                        pass
                    time.sleep(0.01)
                assert text() == before, (
                    scenario + " pasted text after cancellation/quit"
                )
                for pid in (root / "capture-pids").read_text().split():
                    process = Path("/proc") / pid
                    assert not process.exists() or process.joinpath(
                        "stat"
                    ).read_text().split(") ", 1)[1].startswith("Z"), (
                        "Microphone child survived " + scenario
                    )
                assert not raw.exists() and not cleaned.exists()
                assert not list((root / "state/hype-flow/spool").glob("*.pcm")), (
                    "Cancelled recording survived shutdown"
                )
                log.close()
                print("Rust desktop regression: " + scenario + " passed")
        finally:
            if app.poll() is None:
                os.killpg(app.pid, signal.SIGTERM)
                try:
                    app.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(app.pid, signal.SIGKILL)
                    app.wait()
            log.close()
            window.destroy()


if __name__ == "__main__":
    main()
