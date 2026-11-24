#!/usr/bin/env python3
"""Check lazy startup and five-minute idle unload with the installed model and worker."""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import time
from pathlib import Path

from benchmark_desktop import create_capture, process_tree_memory
from ui_driver import Gio, GLib, wait

REPO = Path(__file__).resolve().parents[1]


def inside(root):
    assets = Path(
        os.environ.get("HYPE_FLOW_IDLE_ASSETS", Path.home() / ".local/share/hype-flow")
    )
    configuration = root / "config/hype-flow"
    configuration.mkdir(parents=True)
    settings = {
        "model": str(assets / "models/parakeet-unified-en-int8"),
        "server": str(assets / "backend/parakeet/recognizer"),
        "unload_minutes": 5,
        "paste": False,
        "sounds": False,
        "pause_media": False,
    }
    (configuration / "config.toml").write_text(
        "".join(f"{key} = {json.dumps(value)}\n" for key, value in settings.items())
    )
    shim = create_capture(root, Path("/tmp/hype-flow-eval/jfk.wav"))
    environment = {
        **os.environ,
        "XDG_CONFIG_HOME": str(root / "config"),
        "PATH": str(shim) + ":" + os.environ["PATH"],
        "HYPE_FLOW_BENCHMARK_ROOT": str(root),
        "HYPE_FLOW_BENCHMARK_PACE": "1",
    }
    app = subprocess.Popen(
        [str(REPO / "target/release/hype-flow"), "--background"], env=environment
    )
    group = Gio.DBusActionGroup.get(
        Gio.bus_get_sync(Gio.BusType.SESSION, None), "dev.hype.Flow", "/dev/hype/Flow"
    )

    def phase(name):
        assert app.poll() is None
        state = group.get_action_state("status")
        return state and state.unpack()[0] == name

    try:
        wait(lambda: phase("idle"), "background ready")
        assert not any(
            row["executable"] == "recognizer"
            for row in process_tree_memory(app.pid)["processes"]
        )
        group.activate_action("toggle", None)
        wait(lambda: phase("recording"), "first recording starts")
        wait(lambda: phase("done"), "first recording finishes", timeout=90)
        loaded = process_tree_memory(app.pid)
        assert any(row["executable"] == "recognizer" for row in loaded["processes"])
        began = time.monotonic()
        while time.monotonic() - began < 305:
            while GLib.MainContext.default().iteration(False):
                pass
            time.sleep(0.05)
        unloaded = process_tree_memory(app.pid)
        assert not any(
            row["executable"] == "recognizer" for row in unloaded["processes"]
        )
        assert loaded["pss_kib"] - unloaded["pss_kib"] > 500_000
        group.activate_action("toggle", None)
        wait(lambda: phase("recording"), "record immediately after idle unload")
        wait(
            lambda: any(
                row["executable"] == "recognizer"
                for row in process_tree_memory(app.pid)["processes"]
            ),
            "worker reload",
        )
        group.activate_action("cancel", None)
        wait(lambda: phase("idle"), "cancel after reload")
        report = {
            "timer_seconds": 300,
            "observed_after_seconds": time.monotonic() - began,
            "loaded": loaded,
            "unloaded": unloaded,
            "record_and_reload_after_unload": True,
            "limit": "One real timer expiration; repeated latency/memory benchmarks are reported separately.",
        }
        (REPO / ".migration/idle-unload.json").write_text(json.dumps(report, indent=2))
        print("Real idle unload and next-recording reload passed", flush=True)
    finally:
        group.activate_action("quit", None)
        app.wait(timeout=10)


if __name__ == "__main__":
    if "--inside" in sys.argv:
        try:
            root = Path(os.environ["HYPE_FLOW_IDLE_ROOT"])
            inside(root)
            (root / "complete").touch()
        finally:
            subprocess.run(
                ["niri", "msg", "action", "quit", "--skip-confirmation"],
                check=False,
                timeout=5,
            )
    else:
        (REPO / ".migration").mkdir(exist_ok=True)
        parser = argparse.ArgumentParser(description=__doc__)
        parser.add_argument(
            "--assets", type=Path, default=Path.home() / ".local/share/hype-flow"
        )
        arguments = parser.parse_args()
        with tempfile.TemporaryDirectory(prefix="hype-flow-idle-") as temporary:
            root = Path(temporary)
            runtime = root / "runtime"
            runtime.mkdir(mode=0o700)
            display = Path(os.environ["WAYLAND_DISPLAY"])
            if not display.is_absolute():
                display = Path(os.environ["XDG_RUNTIME_DIR"]) / display
            environment = {
                **os.environ,
                "HYPE_FLOW_IDLE_ROOT": str(root),
                "XDG_RUNTIME_DIR": str(runtime),
                "WAYLAND_DISPLAY": str(display),
                "GTK_A11Y": "none",
                "HYPE_FLOW_IDLE_PYTHON": sys.executable,
                "HYPE_FLOW_IDLE_SCRIPT": str(Path(__file__).resolve()),
                "HYPE_FLOW_IDLE_ASSETS": str(arguments.assets.resolve()),
                "HYPE_FLOW_IDLE_LOG": str(REPO / ".migration/idle-unload-inside.log"),
            }
            subprocess.run(
                [
                    "dbus-run-session",
                    "--",
                    "niri",
                    "-c",
                    str(REPO / "tests/fixtures/niri.kdl"),
                    "--",
                    "sh",
                    "-c",
                    'exec "$HYPE_FLOW_IDLE_PYTHON" "$HYPE_FLOW_IDLE_SCRIPT" --inside > "$HYPE_FLOW_IDLE_LOG" 2>&1',
                ],
                env=environment,
                check=True,
                timeout=400,
            )
            assert (root / "complete").exists(), (
                REPO / ".migration/idle-unload-inside.log"
            ).read_text()
