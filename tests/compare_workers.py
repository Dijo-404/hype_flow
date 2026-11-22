#!/usr/bin/env python3
"""Compare the retained C worker and Rust worker against the same native runtime."""
import argparse
import hashlib
import json
import os
from pathlib import Path
import select
import shutil
import statistics
import struct
import subprocess
import time
import wave


def exact(stream, size, deadline):
    result = bytearray()
    while len(result) < size:
        remaining = deadline - time.monotonic()
        assert remaining > 0, "Worker response timed out"
        if not select.select([stream], [], [], remaining)[0]:
            raise TimeoutError("Worker response timed out")
        chunk = os.read(stream.fileno(), size - len(result))
        assert chunk, "Worker closed its response pipe"
        result.extend(chunk)
    return bytes(result)


def frame(stream, timeout=180):
    deadline = time.monotonic() + timeout
    size = struct.unpack("<I", exact(stream, 4, deadline))[0]
    assert 0 < size <= 1048576, size
    return json.loads(exact(stream, size, deadline))


def pcm(path):
    with wave.open(str(path), "rb") as source:
        assert (source.getnchannels(), source.getsampwidth(), source.getframerate()) == (1, 2, 16000)
        return source.readframes(source.getnframes())


def memory(pid):
    lines = Path(f"/proc/{pid}/smaps_rollup").read_text().splitlines()
    return {line.split(":", 1)[0].lower() + "_kib": int(line.split()[1]) for line in lines if line.startswith(("Pss:", "Rss:"))}


class Worker:
    def __init__(self, binary, model, options, log):
        self.log = log.open("wb")
        began = time.monotonic()
        self.process = subprocess.Popen([str(binary), str(model), "4", *map(str, options)], stdin=subprocess.PIPE,
                                        stdout=subprocess.PIPE, stderr=self.log, bufsize=0)
        try:
            assert frame(self.process.stdout) == {"ready": True}
        except BaseException:
            self.close()
            raise
        self.startup_ms = (time.monotonic() - began) * 1000
        self.loaded = memory(self.process.pid)
        os.set_blocking(self.process.stdin.fileno(), False)

    def transcribe(self, audio):
        began = time.monotonic()
        data = memoryview(struct.pack("<I", len(audio) // 2) + audio)
        deadline = began + 600
        while data:
            remaining = deadline - time.monotonic()
            assert remaining > 0
            assert select.select([], [self.process.stdin], [], remaining)[1]
            try:
                written = os.write(self.process.stdin.fileno(), data)
            except BlockingIOError:
                continue
            assert written > 0
            data = data[written:]
        result = frame(self.process.stdout, 600)
        assert isinstance(result.get("text"), str)
        return {"text": result["text"], "decode_ms": (time.monotonic() - began) * 1000, **memory(self.process.pid)}

    def close(self):
        if self.process.poll() is None:
            self.process.stdin.close()
            try:
                self.process.wait(timeout=8)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait()
        self.process.stdout.close()
        self.log.close()
        assert self.process.returncode == 0, self.process.returncode


def summary(values):
    return {"median": statistics.median(values), "min": min(values), "max": max(values), "samples": values}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rust", type=Path, default=Path("target/release/hype-flow-recognizer"))
    parser.add_argument("--c", type=Path, default=Path(".migration/phase1-bin/recognizer-c"))
    parser.add_argument("--assets", type=Path, default=Path.home() / ".local/share/hype-flow")
    parser.add_argument("--manifest", type=Path, default=Path("/tmp/hype-flow-eval/recordings.jsonl"))
    parser.add_argument("--repetitions", type=int, default=5)
    parser.add_argument("--output", type=Path, default=Path(".migration"))
    args = parser.parse_args()
    assert args.repetitions >= 1
    args.output.mkdir(parents=True, exist_ok=True)
    model = args.assets / "models/parakeet-unified-en-int8"
    library = args.assets / "backend/parakeet/lib"
    binaries = {}
    for name, binary, version in (("c", args.c, "3"), ("rust", args.rust, "4")):
        stage = args.output / "worker-stage" / name
        stage.mkdir(parents=True, exist_ok=True)
        binaries[name] = stage / "recognizer"
        shutil.copy2(binary, binaries[name])
        binaries[name].chmod(0o700)
        if not (stage / "lib").exists():
            (stage / "lib").symlink_to(library.resolve(), target_is_directory=True)
        (stage / "recognizer.version").write_text(version + "\n")
    hotwords = args.output / "worker-stage/hotwords.txt"
    hotwords.write_text("niri\nOpenAI\nHype Flow\n")
    hotwords.chmod(0o600)
    cases = [(row["audio"], pcm(args.manifest.parent / row["audio"])) for line in args.manifest.read_text().splitlines()
             if line.strip() and (row := json.loads(line))]
    vad = ["--vad", model.parent / "silero_vad.onnx"]
    denoise = ["--denoise", model.parent / "gtcrn_simple.onnx"]
    modes = {"greedy": [], "boosting": ["--hotwords", hotwords.resolve(), "1.5"],
             "vad": vad, "denoise": denoise, "combined": [*vad, *denoise]}
    results = {"repetitions": args.repetitions, "worker_sha256": {name: hashlib.sha256(path.read_bytes()).hexdigest() for name, path in binaries.items()}, "modes": {}, "startup": {}}
    startup = {name: [] for name in binaries}
    for index in range(args.repetitions):
        order = list(binaries) if index % 2 == 0 else list(reversed(binaries))
        for name in order:
            binary = binaries[name]
            worker = Worker(binary.resolve(), model.resolve(), [], args.output / f"worker-{name}-startup.log")
            startup[name].append({"milliseconds": worker.startup_ms, **worker.loaded})
            worker.close()
    for name, values in startup.items():
        results["startup"][name] = {key: summary([sample[key] for sample in values]) for key in values[0]}
    for mode_index, (mode, options) in enumerate(modes.items()):
        print("Compare mode:", mode, flush=True)
        outputs = {name: [] for name in binaries}
        workers = {}
        try:
            order = list(binaries) if mode_index % 2 == 0 else list(reversed(binaries))
            for name in order:
                worker = Worker(binaries[name].resolve(), model.resolve(), options, args.output / f"worker-{name}-{mode}.log")
                workers[name] = worker
                worker.transcribe(cases[0][1])
            for index in range(args.repetitions):
                order = list(binaries) if index % 2 == 0 else list(reversed(binaries))
                for name in order:
                    outputs[name].append([{"audio": audio, **workers[name].transcribe(samples)} for audio, samples in cases])
        finally:
            for worker in workers.values():
                worker.close()
        for left, right in zip(outputs["c"], outputs["rust"], strict=True):
            assert [(row["audio"], row["text"]) for row in left] == [(row["audio"], row["text"]) for row in right], mode
        results["modes"][mode] = outputs
    results["all_transcripts_identical"] = True
    results["measurement_order"] = "Counterbalanced startup pairs and alternating decode pairs, both workers loaded per mode."
    (args.output / "worker-comparison.json").write_text(json.dumps(results, indent=2))
    lines = ["# C/Rust worker comparison", "", f"{args.repetitions} repetitions per mode, identical pinned native runtime and model.",
             "All raw transcripts matched for greedy decoding, boosting, VAD, denoising and combined options.", "",
             "| Mode | C median corpus decode | Rust median corpus decode | Change |", "| --- | ---: | ---: | ---: |"]
    for mode, output in results["modes"].items():
        medians = {name: statistics.median(sum(case["decode_ms"] for case in run) for run in rows) for name, rows in output.items()}
        lines.append(f"| {mode} | {medians['c']:.1f} ms | {medians['rust']:.1f} ms | {(medians['rust']/medians['c']-1)*100:+.1f}% |")
    lines += ["", "Startup, process memory, ranges and individual samples are in worker-comparison.json.",
              "These are raw-worker checks using public prerecorded clips. Application silence filtering and text cleanup are verified separately.",
              "The wrapper still calls native inference libraries; this comparison does not establish accuracy for the owner's voice."]
    (args.output / "worker-comparison.md").write_text("\n".join(lines) + "\n")
    print("All worker transcript comparisons passed", flush=True)


if __name__ == "__main__":
    main()
