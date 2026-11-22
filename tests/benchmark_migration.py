#!/usr/bin/env python3
"""Compare the Python and release Rust apps in an isolated nested niri session."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import statistics
import subprocess
import sys
import tempfile
import time
import wave

from ui_driver import Gio, GLib, Gtk, focused, wait, write_executable


REPO = Path(__file__).resolve().parents[1]
BUS_NAME = "dev.hype.Flow"
APP_PATH = "/dev/hype/Flow"
SETUP_MESSAGE = "Download the speech model to get started"

METRICS = {
    "ready_with_model": [
        ("startup_ms", "Launch to loaded and ready", "ms"),
        ("rss_kib", "Loaded idle tree RSS", "KiB"),
        ("pss_kib", "Loaded idle tree PSS", "KiB"),
    ],
    "idle_without_model": [
        ("startup_ms", "Launch to setup prompt", "ms"),
        ("rss_kib", "Model-free idle tree RSS", "KiB"),
        ("pss_kib", "Model-free idle tree PSS", "KiB"),
    ],
    "prerecorded_dictation": [
        ("shortcut_first_frame_ms", "Shortcut to first captured frame", "ms"),
        ("decode_cleanup_ms", "Transcribing to delivering", "ms"),
        ("delivery_ms", "Delivering to pasted completion", "ms"),
        ("capture_end_to_paste_ms", "Capture end to pasted completion", "ms"),
    ],
}


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def process_tree_memory(root_pid):
    """Sum Linux smaps_rollup across unique PIDs, including children of each thread."""
    seen, pending, sample = set(), [root_pid], []
    while pending:
        pid = pending.pop()
        if pid in seen:
            continue
        seen.add(pid)
        proc = Path(f"/proc/{pid}")
        try:
            status = (proc / "smaps_rollup").read_text()
        except (FileNotFoundError, ProcessLookupError):
            # Short-lived helper processes may exit before the sample is available.
            continue
        try:
            executable = Path(os.readlink(proc / "exe")).name
        except FileNotFoundError:
            continue
        metrics = {
            key.lower() + "_kib": int(value)
            for key, value in re.findall(r"^(Pss|Rss):\s+(\d+) kB$", status, re.M)
        }
        assert metrics.keys() == {"rss_kib", "pss_kib"}, (pid, status)
        sample.append({"pid": pid, "executable": executable, **metrics})
        for children in (proc / "task").glob("*/children"):
            try:
                pending.extend(int(child) for child in children.read_text().split())
            except (FileNotFoundError, ProcessLookupError):
                pass
    assert any(entry["pid"] == root_pid for entry in sample), "Could not measure app memory"
    return {
        name: sum(entry[name] for entry in sample)
        for name in ("rss_kib", "pss_kib")
    } | {"processes": sorted(sample, key=lambda entry: entry["pid"])}


def normalized(text):
    return re.findall(r"[^\W_]+", text.casefold())


def create_capture(root, audio_path):
    with wave.open(str(audio_path), "rb") as audio:
        assert (audio.getnchannels(), audio.getsampwidth(), audio.getframerate()) == (1, 2, 16000)
        pcm = audio.readframes(audio.getnframes())
    assert len(pcm) > 1600
    (root / "capture.pcm").write_bytes(pcm)
    shim = root / "bin"
    shim.mkdir()
    write_executable(shim / "pw-dump", "#!/bin/sh\nprintf '[]'\n")
    write_executable(shim / "pw-play", "#!/bin/sh\nexit 0\n")
    write_executable(shim / "pw-record", "#!" + sys.executable + "\n" + r"""
import os
from pathlib import Path
import sys
import time

root = Path(os.environ["HYPE_FLOW_BENCHMARK_ROOT"])
pcm = (root / "capture.pcm").read_bytes()
time.sleep(0.06)
sys.stdout.buffer.write(pcm[:1600])
sys.stdout.buffer.flush()
time.sleep(0.08)
sys.stdout.buffer.write(pcm[1600:])
sys.stdout.buffer.flush()
(root / "capture-end-ns").write_text(str(time.monotonic_ns()))
""")
    return shim


def millis(start, end):
    return (end - start) / 1_000_000


def run_app(command, label, root, env, target, reference, assets, loaded):
    config = root / "config" / "hype-flow"
    config.mkdir(parents=True, exist_ok=True)
    model = assets / "models" / "parakeet-unified-en-int8" if loaded else root / "absent-model"
    recognizer = assets / "backend" / "parakeet" / "recognizer" if loaded else root / "absent-worker"
    (config / "config.toml").write_text(f"model = {json.dumps(str(model))}\nserver = {json.dumps(str(recognizer))}\n")
    app_env = env | {"XDG_CONFIG_HOME": str(config.parent)}
    runtime = Path(app_env["XDG_RUNTIME_DIR"]) / "hype-flow"
    startup = root / f"{label}.log"
    target.set_text("")
    capture_end = root / "capture-end-ns"
    capture_end.unlink(missing_ok=True)
    group = None
    with startup.open("wb") as log:
        started = time.monotonic_ns()
        app = subprocess.Popen(command + ["--background"], env=app_env, stdout=log, stderr=log, start_new_session=True)
        try:
            group = Gio.DBusActionGroup.get(
                Gio.bus_get_sync(Gio.BusType.SESSION, None), BUS_NAME, APP_PATH
            )
            times = {}

            def track(_, name, value):
                if name == "status":
                    state = value.unpack()
                    times.setdefault(state[0], time.monotonic_ns())

            group.connect("action-state-changed", track)

            def state():
                if app.poll() is not None:
                    raise RuntimeError(f"Application exited ({app.returncode})\n{startup.read_text(errors='replace')}")
                variant = group.get_action_state("status")
                if variant is None:
                    return None
                assert variant.get_type_string() == "(ssddb)"
                current = variant.unpack()
                if current[0] == "error":
                    raise RuntimeError(current[1])
                times.setdefault(current[0], time.monotonic_ns())
                return current

            def ready():
                current = state()
                return current and current[:2] == ("idle", "Ready" if loaded else SETUP_MESSAGE)

            wait(ready, "application ready", timeout=180)
            ready_ns = time.monotonic_ns()
            time.sleep(0.3)
            memory = process_tree_memory(app.pid)
            assert any(
                entry["executable"] == "recognizer" for entry in memory["processes"]
            ) == loaded, memory
            idle = {"startup_ms": millis(started, ready_ns), **memory}
            if not loaded:
                return idle, None

            subprocess.run(["wl-copy", "--clear"], check=True, timeout=3)
            assert focused()["pid"] == os.getpid(), "Benchmark target lost focus"
            shortcut = time.monotonic_ns()
            with subprocess.Popen(command + ["--toggle"], env=app_env, stdout=log, stderr=log) as control:
                def done():
                    current = state()
                    return current and current[0] == "done" and current

                delivery = wait(done, "prerecorded transcription and paste", timeout=30)
                assert control.wait(timeout=5) == 0
            assert delivery[1] == "Pasted", delivery
            raw = (runtime / "last-raw-transcript.txt").read_text()
            wait(
                lambda: target.get_text(target.get_start_iter(), target.get_end_iter(), True) == raw,
                "target received the exact transcript",
            )
            assert normalized(raw) == reference, (raw, reference)
            finish = int(capture_end.read_text())
            samples = {
                "shortcut_first_frame_ms": millis(shortcut, times["recording"]),
                "decode_cleanup_ms": millis(times["transcribing"], times["delivering"]),
                "delivery_ms": millis(times["delivering"], times["done"]),
                "capture_end_to_paste_ms": millis(finish, times["done"]),
                "raw_transcript_sha256": hashlib.sha256(raw.encode()).hexdigest(),
            }
            assert all(value > 0 for key, value in samples.items() if key.endswith("_ms")), samples
            return idle, samples
        finally:
            if app.poll() is None:
                if group and group.has_action("quit"):
                    group.activate_action("quit", None)
                else:
                    os.killpg(app.pid, signal.SIGTERM)
                try:
                    app.wait(timeout=8)
                except subprocess.TimeoutExpired:
                    os.killpg(app.pid, signal.SIGKILL)
                    app.wait()


def write_report(results, output):
    metadata, samples = results["metadata"], results["samples"]
    output.mkdir(parents=True, exist_ok=True)
    (output / "benchmarks.json").write_text(json.dumps(results, indent=2) + "\n")
    rows = [
        "# Hype Flow migration benchmarks",
        "",
        f'{metadata["repetitions"]} measured repetitions per application and scenario. '
        "Each starts a fresh process after an unmeasured warm-up. "
        "Test order alternates and host filesystem caches are left alone.",
        "",
        "| Measurement | Unit | Python median (range) | Rust median (range) | Rust median change |",
        "| --- | --- | ---: | ---: | ---: |",
    ]
    for scenario, metrics in METRICS.items():
        for field, label, unit in metrics:
            data = {
                app: [sample[field] for sample in cases[scenario]]
                for app, cases in samples.items()
            }
            descriptions = {
                app: f"{statistics.median(values):,.2f} ({min(values):,.2f}–{max(values):,.2f})"
                for app, values in data.items()
            }
            change = (
                f"{(statistics.median(data['rust']) / statistics.median(data['python']) - 1) * 100:+.1f}%"
                if len(data) == 2 else "—"
            )
            rows.append(
                f"| {label} | {unit} | {descriptions.get('python', '—')} | "
                f"{descriptions.get('rust', '—')} | {change} |"
            )
    rows += [
        "",
        "The benchmark runs under nested niri with a private D-Bus session, runtime "
        "directory, configuration, clipboard and GTK text target. The measured applications "
        "use their default settings and the model and worker identified in the JSON metadata. "
        "PipeWire device listing and cue playback use test shims.",
        "",
        "The shortcut measurement runs the normal executable with `--toggle` and "
        "waits for the app's first `recording` status. A `pw-record` shim delivers "
        "the same mono, 16 kHz PCM16 frames and exits to end capture. Decode and "
        "cleanup timing comes from the `transcribing` and `delivering` statuses. "
        "The compositor performs the paste into the target, and the test checks the "
        "visible text against the raw transcript and the corpus reference.",
        "",
        "Idle memory sums each live app descendant's Linux `smaps_rollup` RSS and "
        "PSS 300 ms after the ready status. Loaded samples include the recognizer "
        "tree. Model-free samples use the first-run setup prompt with an absent model "
        "path. Actual physical microphone latency and post-idle timer unloading "
        "are outside this measurement.",
        "",
        "Each sample and the model/worker identity are recorded in "
        "[benchmarks.json](benchmarks.json).",
    ]
    (output / "benchmark-report.md").write_text("\n".join(rows) + "\n")


def benchmark(args):
    root = Path(os.environ["HYPE_FLOW_BENCHMARK_ROOT"])
    private_runtime = root / "runtime"
    assert Path(os.environ["XDG_RUNTIME_DIR"]) == private_runtime
    assert Path(os.environ["NIRI_SOCKET"]).parent == private_runtime
    assets = args.assets
    manifest = args.manifest
    pair = json.loads(manifest.read_text().splitlines()[0])
    audio_path = manifest.parent / pair["audio"]
    reference = normalized(pair["text"])
    shim = create_capture(root, audio_path)
    env = os.environ.copy()
    env["PATH"] = str(shim) + os.pathsep + env["PATH"]
    commands = {"python": [sys.executable, str(REPO / "tools/launch.py")],
                "rust": [str(args.rust)]}
    if args.only:
        commands = {args.only: commands[args.only]}
    results = {
        "metadata": {
            "repetitions": args.repetitions,
            "commands": commands,
            "audio": str(audio_path),
            "audio_sha256": digest(audio_path),
            "worker": str(assets / "backend/parakeet/recognizer"),
            "worker_sha256": digest(assets / "backend/parakeet/recognizer"),
            "model": str(assets / "models/parakeet-unified-en-int8"),
            "rust_sha256": digest(args.rust) if "rust" in commands else None,
        },
        "samples": {app: {name: [] for name in METRICS} for app in commands},
    }

    Gtk.init()
    target_app = Gtk.Application(
        application_id="dev.hype.Flow.MigrationBenchmark",
        flags=Gio.ApplicationFlags.NON_UNIQUE,
    )
    assert target_app.register(None)
    target_window = Gtk.ApplicationWindow(application=target_app, title="Hype Flow benchmark target")
    editor = Gtk.TextView()
    target_window.set_child(editor)
    target_window.set_default_size(600, 350)
    target_window.present()
    editor.grab_focus()
    wait(lambda: (target := focused()) and target.get("pid") == os.getpid(), "controlled focus")
    for index in range(args.repetitions + 1):
        order = list(commands)
        if index % 2:
            order.reverse()
        for loaded in (bool(index % 2), not bool(index % 2)):
            scenario = "ready_with_model" if loaded else "idle_without_model"
            for app in order:
                label = f"{index:02}-{app}-{scenario}"
                print(label, flush=True)
                try:
                    idle, capture = run_app(commands[app], label, root, env, editor.get_buffer(), reference, assets, loaded)
                except Exception:
                    for path in (root / (label + ".log"), private_runtime / "hype-flow/backend.log"):
                        if path.is_file():
                            print(path.name + "\n" + path.read_text(errors="replace"), file=sys.stderr)
                    raise
                if index:
                    results["samples"][app][scenario].append(idle)
                    if capture is not None:
                        results["samples"][app]["prerecorded_dictation"].append(capture)
    write_report(results, args.output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust", type=Path, default=REPO / "target/release/hype-flow")
    parser.add_argument("--assets", type=Path, default=Path.home() / ".local/share/hype-flow")
    parser.add_argument("--manifest", type=Path, default=Path("/tmp/hype-flow-eval/recordings.jsonl"))
    parser.add_argument("--niri-config", type=Path, default=REPO / "tests/fixtures/niri.kdl")
    parser.add_argument("--output", type=Path, default=REPO / ".migration")
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--only", choices=("python", "rust"))
    parser.add_argument("--inside", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    assert args.repetitions > 0
    for field in ("rust", "assets", "manifest", "niri_config", "output"):
        setattr(args, field, getattr(args, field).resolve())
    if args.inside:
        root = Path(os.environ["HYPE_FLOW_BENCHMARK_ROOT"])
        try:
            benchmark(args)
            (root / "complete").touch()
        finally:
            subprocess.run(["niri", "msg", "action", "quit", "--skip-confirmation"], timeout=5)
        return
    if args.only != "python":
        assert args.rust.is_file(), f"Release app is unavailable: {args.rust}"
    parent_display = Path(os.environ["WAYLAND_DISPLAY"])
    if not parent_display.is_absolute():
        parent_display = Path(os.environ["XDG_RUNTIME_DIR"]) / parent_display
    with tempfile.TemporaryDirectory(prefix="hype-flow-benchmark-") as temporary:
        root = Path(temporary)
        runtime = root / "runtime"
        runtime.mkdir(mode=0o700)
        env = os.environ.copy()
        env.pop("DISPLAY", None)
        env.pop("NIRI_SOCKET", None)
        env.pop("DBUS_SESSION_BUS_ADDRESS", None)
        env.update(
            WAYLAND_DISPLAY=str(parent_display), XDG_RUNTIME_DIR=str(runtime),
            HYPE_FLOW_BENCHMARK_ROOT=str(root), GTK_A11Y="none", NO_AT_BRIDGE="1",
        )
        subprocess.run(
            ["dbus-run-session", "--", "niri", "-c", str(args.niri_config), "--",
             sys.executable, str(Path(__file__).resolve()), "--inside", *sys.argv[1:]],
            env=env, check=True, timeout=300,
        )
        assert (root / "complete").exists(), "Benchmark driver did not complete"


if __name__ == "__main__":
    main()
