#!/usr/bin/env python3
"""Measure startup, memory and paste latency in an isolated niri session."""

import argparse
import hashlib
import json
import os
import math
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
    "startup_with_model": [
        ("startup_ms", "Launch to ready with installed model", "ms"),
        ("rss_kib", "Ready idle tree RSS", "KiB"),
        ("pss_kib", "Ready idle tree PSS", "KiB"),
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
        ("rss_kib", "Loaded tree RSS after paste", "KiB"),
        ("pss_kib", "Loaded tree PSS after paste", "KiB"),
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
if os.environ.get("HYPE_FLOW_BENCHMARK_PACE") == "1":
    started = time.monotonic()
    for offset in range(0, len(pcm), 1600):
        time.sleep(max(0, started + offset / 32000 - time.monotonic()))
        sys.stdout.buffer.write(pcm[offset:offset + 1600])
        sys.stdout.buffer.flush()
else:
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


def run_app(executable, label, root, env, target, reference, assets, installed):
    config = root / "config" / "hype-flow"
    config.mkdir(parents=True, exist_ok=True)
    model = assets / "models" / "parakeet-unified-en-int8" if installed else root / "absent-model"
    recognizer = assets / "backend" / "parakeet" / "recognizer" if installed else root / "absent-worker"
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
        app = subprocess.Popen([executable, "--background"], env=app_env, stdout=log, stderr=log, start_new_session=True)
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
                return current and current[:2] == ("idle", "Ready" if installed else SETUP_MESSAGE)

            wait(ready, "application ready", timeout=180)
            ready_ns = time.monotonic_ns()
            time.sleep(0.3)
            memory = process_tree_memory(app.pid)
            assert not any(
                entry["executable"] == "recognizer" for entry in memory["processes"]
            ), memory
            idle = {"startup_ms": millis(started, ready_ns), **memory}
            if not installed:
                return idle, None

            subprocess.run(["wl-copy", "--clear"], check=True, timeout=3)
            assert focused()["pid"] == os.getpid(), "Benchmark target lost focus"
            shortcut = time.monotonic_ns()
            with subprocess.Popen([executable, "--toggle"], env=app_env, stdout=log, stderr=log) as control:
                def done():
                    current = state()
                    return current and current[0] == "done" and current

                replay_seconds = (root / "capture.pcm").stat().st_size / 32000 if env.get("HYPE_FLOW_BENCHMARK_PACE") == "1" else 0
                delivery = wait(done, "prerecorded transcription and paste", timeout=30 + replay_seconds)
                assert control.wait(timeout=5) == 0
            assert delivery[1] == "Pasted", delivery
            raw = (runtime / "last-raw-transcript.txt").read_text()
            wait(
                lambda: target.get_text(target.get_start_iter(), target.get_end_iter(), True) == raw,
                "target received the exact transcript",
            )
            assert normalized(raw) == reference, (raw, reference)
            finish = int(capture_end.read_text())
            loaded = process_tree_memory(app.pid)
            assert any(entry["executable"] == "recognizer" for entry in loaded["processes"]), loaded
            samples = {
                "shortcut_first_frame_ms": millis(shortcut, times["recording"]),
                "decode_cleanup_ms": millis(times["transcribing"], times["delivering"]),
                "delivery_ms": millis(times["delivering"], times["done"]),
                "capture_end_to_paste_ms": millis(finish, times["done"]),
                "raw_transcript_sha256": hashlib.sha256(raw.encode()).hexdigest(),
                **loaded,
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
    metadata, cases = results["metadata"], results["samples"]["rust"]
    measured = sorted(sample["capture_end_to_paste_ms"] for sample in cases["prerecorded_dictation"])
    cases["capture_end_to_paste_percentiles_ms"] = {
        f"p{percent}": measured[math.ceil(len(measured) * percent / 100) - 1]
        for percent in (50, 95)
    }
    output.mkdir(parents=True, exist_ok=True)
    (output / "benchmarks.json").write_text(json.dumps(results, indent=2) + "\n")
    rows = [
        "# Hype Flow desktop benchmarks",
        "",
        f'{metadata["repetitions"]} measured repetitions per scenario after one unmeasured run. '
        "Each starts a fresh process. Scenario order alternates, and host filesystem caches stay in place.",
        "",
        "| Measurement | Unit | Median (range) |",
        "| --- | --- | ---: |",
    ]
    for scenario, metrics in METRICS.items():
        for field, label, unit in metrics:
            values = [sample[field] for sample in cases[scenario]]
            rows.append(f"| {label} | {unit} | {statistics.median(values):,.2f} ({min(values):,.2f}–{max(values):,.2f}) |")
    rows += [
        "",
        "The benchmark runs under nested niri with a private D-Bus session, runtime "
        "directory, configuration, clipboard and GTK text target. The app uses its "
        "default settings and the model and worker identified in the JSON metadata. "
        "PipeWire device listing and cue playback use test shims.",
        "",
        "The shortcut measurement runs the normal executable with `--toggle` and "
        "waits for the app's first `recording` status. A `pw-record` shim delivers "
        "the same mono, 16 kHz PCM16 frames and exits to end capture. Decode and "
        "cleanup timing comes from the `transcribing` and `delivering` statuses. "
        "The compositor performs the paste into the target, and the test checks the "
        "visible text against the raw transcript and the corpus reference.",
        "",
        "Idle memory sums the app process tree's Linux `smaps_rollup` RSS and PSS "
        "300 ms after startup readiness. The worker starts on first dictation, "
        "and loaded memory is sampled after paste. Model-free samples use the first-run setup prompt with an absent model "
        "path. Actual physical microphone latency and post-idle timer unloading "
        "are outside this measurement.",
        "",
        "Each sample and the model/worker identity are recorded in "
        "[benchmarks.json](benchmarks.json).",
    ]
    (output / "benchmark-report.md").write_text("\n".join(rows) + "\n")


def replay_case(manifest):
    pair = json.loads(manifest.read_text().splitlines()[0])
    return manifest.parent / pair["audio"], normalized(pair["text"])


def benchmark(args):
    root = Path(os.environ["HYPE_FLOW_BENCHMARK_ROOT"])
    private_runtime = root / "runtime"
    assert Path(os.environ["XDG_RUNTIME_DIR"]) == private_runtime
    assert Path(os.environ["NIRI_SOCKET"]).parent == private_runtime
    assets = args.assets
    audio_path, reference = replay_case(args.manifest)
    shim = create_capture(root, audio_path)
    env = os.environ.copy()
    env["PATH"] = str(shim) + os.pathsep + env["PATH"]
    env["HYPE_FLOW_BENCHMARK_PACE"] = "1" if args.pace else "0"
    cases = {name: [] for name in METRICS}
    results = {
        "metadata": {
            "repetitions": args.repetitions,
            "commands": {"rust": [str(args.rust)]},
            "audio": str(audio_path),
            "audio_sha256": digest(audio_path),
            "audio_seconds": (root / "capture.pcm").stat().st_size / 32000,
            "paced_capture": args.pace,
            "worker": str(assets / "backend/parakeet/recognizer"),
            "worker_sha256": digest(assets / "backend/parakeet/recognizer"),
            "model": str(assets / "models/parakeet-unified-en-int8"),
            "rust_sha256": digest(args.rust),
        },
        "samples": {"rust": cases},
    }

    Gtk.init()
    target_app = Gtk.Application(
        application_id="dev.hype.Flow.DesktopBenchmark",
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
        installed_first = index % 2 == 0
        for installed in (installed_first, not installed_first):
            scenario = "startup_with_model" if installed else "idle_without_model"
            label = f"{index:02}-{scenario}"
            print(label, flush=True)
            try:
                idle, capture = run_app(str(args.rust), label, root, env, editor.get_buffer(), reference, assets, installed)
            except Exception:
                for path in (root / (label + ".log"), private_runtime / "hype-flow/backend.log"):
                    if path.is_file():
                        print(path.name + "\n" + path.read_text(errors="replace"), file=sys.stderr)
                raise
            if index:
                cases[scenario].append(idle)
                if capture is not None:
                    cases["prerecorded_dictation"].append(capture)
    write_report(results, args.output)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust", type=Path, default=REPO / "target/release/hype-flow")
    parser.add_argument("--assets", type=Path, default=Path.home() / ".local/share/hype-flow")
    parser.add_argument("--manifest", type=Path, default=Path("/tmp/hype-flow-eval/recordings.jsonl"))
    parser.add_argument("--niri-config", type=Path, default=REPO / "tests/fixtures/niri.kdl")
    parser.add_argument("--output", type=Path, default=REPO / ".migration")
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--pace", action="store_true", help="Replay capture at 16 kHz instead of in a burst")
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
    assert args.rust.is_file(), f"Release app is unavailable: {args.rust}"
    timeout = 300
    if args.pace:
        audio_path, _ = replay_case(args.manifest)
        with wave.open(str(audio_path), "rb") as audio:
            duration = audio.getnframes() / audio.getframerate()
        timeout += duration * (args.repetitions + 1)
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
        driver_log = root / "driver.log"
        subprocess.run(
            ["dbus-run-session", "--", "niri", "-c", str(args.niri_config), "--",
             "sh", "-c", 'log=$1; shift; exec "$@" > "$log" 2>&1', "sh", str(driver_log),
             sys.executable, str(Path(__file__).resolve()), "--inside", *sys.argv[1:]],
            env=env, check=True, timeout=timeout,
        )
        if not (root / "complete").exists():
            details = driver_log.read_text(errors="replace") if driver_log.exists() else "Driver did not start"
            raise RuntimeError(f"Benchmark driver did not complete\n{details}")


if __name__ == "__main__":
    main()
