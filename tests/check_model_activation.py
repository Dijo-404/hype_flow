#!/usr/bin/env python3
"""Verify first-use model activation through the production application's action."""
import argparse
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import tomllib

from ui_driver import Gio, wait

REPO = Path(__file__).resolve().parents[1]


def inside(root, low_memory_archive):
    cached = Path.home() / ".local/share/hype-flow/downloads"
    data = root / "data/hype-flow"
    downloads = data / "downloads"
    downloads.mkdir(parents=True)
    for source in cached.iterdir():
        if source.is_file() and not source.name.endswith(".part"):
            (downloads / source.name).symlink_to(source)
    if low_memory_archive:
        (downloads / "parakeet-110m-en-int8.tar.bz2").symlink_to(low_memory_archive)
    configuration = root / "config/hype-flow"
    configuration.mkdir(parents=True)
    config = configuration / "config.toml"
    initial = 'model = "/test/previous-model"\nserver = "/test/previous-worker"\nlanguage = "auto"\ngpu = true\npaste = false\nprompt = "Saved names"\nsounds = false\npause_media = false\n'
    if low_memory_archive:
        initial += 'model_variant = "low-memory"\n'
    config.write_text(initial)
    tools = root / "no-compiler"
    tools.mkdir()
    compiler = tools / "cc"
    compiler.write_text('#!/bin/sh\nexit 97\n')
    compiler.chmod(0o700)
    environment = {**os.environ, "XDG_DATA_HOME": str(root / "data"), "XDG_CONFIG_HOME": str(root / "config"), "PATH": str(tools) + ":" + os.environ["PATH"]}
    app = subprocess.Popen([str(REPO / "target/release/hype-flow"), "--background"], env=environment)
    group = Gio.DBusActionGroup.get(Gio.bus_get_sync(Gio.BusType.SESSION, None), "dev.hype.Flow", "/dev/hype/Flow")
    def state():
        assert app.poll() is None
        value = group.get_action_state("status")
        value = value.unpack() if value else None
        assert not value or value[0] != "error", value
        return value
    try:
        wait(lambda: group.has_action("download") and state(), "setup action")
        group.activate_action("download", None)
        wait(lambda: (value := state()) and value[:2] == ("idle", "Ready"), "installed model ready", timeout=180)
        values = tomllib.loads(config.read_text())
        name = "parakeet-110m-en-int8" if low_memory_archive else "parakeet-unified-en-int8"
        model = data / "models" / name
        assert values["model"] == str(model)
        assert values["feature_dim"] == (80 if low_memory_archive else 128)
        assert values["server"] == str(data / "backend/parakeet/recognizer")
        assert values["language"] == "en" and not values["gpu"]
        assert not values["paste"] and values["prompt"] == "Saved names"
        assert (configuration / "config.toml.previous").read_text() == initial
        assert (model / "bpe.vocab").is_file() == (not low_memory_archive)
        print("Production setup action activated the verified model and preserved preferences/backup", flush=True)
        (root / "complete").touch()
    finally:
        group.activate_action("quit", None)
        app.wait(timeout=10)


if __name__ == "__main__":
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--low-memory-archive", type=Path)
    args = parser.parse_args()
    if args.low_memory_archive:
        args.low_memory_archive = args.low_memory_archive.resolve(strict=True)
    if args.inside:
        try:
            inside(Path(os.environ["HYPE_FLOW_ACTIVATION_ROOT"]), args.low_memory_archive)
        finally:
            subprocess.run(["niri", "msg", "action", "quit", "--skip-confirmation"], timeout=5)
    else:
        with tempfile.TemporaryDirectory(prefix="hype-flow-activation-") as temporary:
            root = Path(temporary)
            runtime = root / "runtime"
            runtime.mkdir(mode=0o700)
            display = Path(os.environ["WAYLAND_DISPLAY"])
            if not display.is_absolute():
                display = Path(os.environ["XDG_RUNTIME_DIR"]) / display
            log = REPO / ".migration/model-activation-inside.log"
            log.parent.mkdir(exist_ok=True)
            environment = {**os.environ, "HYPE_FLOW_ACTIVATION_ROOT": str(root), "XDG_RUNTIME_DIR": str(runtime),
                           "WAYLAND_DISPLAY": str(display), "GTK_A11Y": "none", "ACTIVATION_PYTHON": sys.executable,
                           "ACTIVATION_SCRIPT": str(Path(__file__).resolve()), "ACTIVATION_LOG": str(log)}
            subprocess.run(["dbus-run-session", "--", "niri", "-c", str(REPO / "tests/fixtures/niri.kdl"), "--", "sh", "-c",
                            'exec "$ACTIVATION_PYTHON" "$ACTIVATION_SCRIPT" --inside "$@" > "$ACTIVATION_LOG" 2>&1', "sh", *sys.argv[1:]], env=environment, check=True, timeout=240)
            assert (root / "complete").exists(), log.read_text()
