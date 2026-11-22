"""Persistent Parakeet inference through anonymous pipes; no network listener."""

import io
import json
import os
from pathlib import Path
import select
import shutil
import struct
import subprocess
import threading
import time
import wave

from .segments import split_at_pauses
from .setup import MODEL_FILES
from .vocabulary import hotwords

RATE = 16000
MAX_SECONDS = 300
SANDBOX_ARGS = ["--unshare-net", "--unshare-pid", "--dev-bind", "/", "/", "--proc", "/proc", "--"]


def sandbox():
    """bubblewrap arguments that remove network access, or [] when user namespaces are unavailable."""
    executable = shutil.which("bwrap")
    if not executable:
        return []
    try:
        probe = subprocess.run([executable, *SANDBOX_ARGS, "true"], capture_output=True, timeout=5)
    except (OSError, subprocess.SubprocessError):
        return []
    return [executable, *SANDBOX_ARGS] if probe.returncode == 0 else []


def wav_bytes(pcm):
    output = io.BytesIO()
    with wave.open(output, "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(RATE)
        audio.writeframes(pcm)
    return output.getvalue()


def read_wav(path):
    with wave.open(str(path), "rb") as audio:
        if (audio.getnchannels(), audio.getsampwidth(), audio.getframerate(), audio.getcomptype()) != (1, 2, RATE, "NONE"):
            raise ValueError("Expected mono, 16 kHz, signed 16-bit PCM WAV. Convert with ffmpeg first.")
        if audio.getnframes() > RATE * MAX_SECONDS:
            raise ValueError(f"Audio exceeds {MAX_SECONDS} seconds")
        return audio.readframes(audio.getnframes())


class Engine:
    def __init__(self, config, runtime):
        self.config = config
        self.runtime = runtime
        self.process = None
        self.lock = threading.Lock()
        self.stopped = threading.Event()
        self.starting = threading.Lock()
        self.hotwords_path = None

    def prepare(self):
        self.stopped.clear()

    def ensure(self):
        """Load the worker unless it is running; concurrent callers share one load."""
        with self.starting:
            process = self.process
            if process is not None and process.poll() is None:
                return
            self.stopped.clear()
            self.start()

    def start(self):
        with self.lock:
            if self.stopped.is_set():
                raise RuntimeError("Speech backend startup cancelled")
            model = Path(self.config["model"])
            if not Path(self.config["server"]).is_file() or not all((model / name).is_file() for name in MODEL_FILES):
                raise RuntimeError("Parakeet is not installed. Open Speech model and download the model.")
            if self.config["language"] not in ("en", "auto"):
                raise RuntimeError("Parakeet Unified supports English only. Choose English in settings.")
            args = [self.config["server"], str(model), str(self.config["threads"])]
            vocabulary = hotwords(self.config.get("prompt", "")) if self.config.get("vocabulary_boosting", False) else ""
            if vocabulary:
                if not (model / "bpe.vocab").is_file():
                    raise RuntimeError("Vocabulary support needs an update. Open Speech model and update speech support.")
                self.hotwords_path = self.runtime / "context-vocabulary.txt"
                self.hotwords_path.touch(mode=0o600, exist_ok=True)
                self.hotwords_path.chmod(0o600)
                self.hotwords_path.write_text(vocabulary)
                args += ["--hotwords", str(self.hotwords_path), str(self.config.get("boost_score", 1.5))]
            for key, flag, name in (("cut_silence", "--vad", "silero_vad.onnx"), ("noise_suppression", "--denoise", "gtcrn_simple.onnx")):
                if self.config.get(key, False):
                    path = model.parent / name
                    if not path.is_file():
                        raise RuntimeError("Silence cutting and noise suppression need an update. Open Speech model and update speech support.")
                    args += [flag, str(path)]
            isolation = sandbox()
            log_path = self.runtime / "backend.log"
            with log_path.open("wb") as log:
                log_path.chmod(0o600)
                log.write(b"Network sandbox: bubblewrap; the recognizer has no network access\n" if isolation else
                          b"Network sandbox: unavailable (install bubblewrap with user namespaces enabled)\n")
                log.flush()
                self.process = subprocess.Popen(isolation + args,
                                                stdin=subprocess.PIPE, stdout=subprocess.PIPE, stderr=log, bufsize=0)
        result = self._result(180)
        if result != {"ready": True}:
            self.stop()
            raise RuntimeError("Speech backend did not become ready")

    def _read(self, size, deadline):
        output = bytearray()
        process = self.process
        if process is None:
            raise RuntimeError("Speech backend is not running")
        descriptor = process.stdout.fileno()
        while len(output) < size:
            if self.stopped.is_set():
                raise RuntimeError("Speech inference cancelled")
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise RuntimeError("Speech inference timed out")
            if not select.select([descriptor], [], [], min(remaining, 0.1))[0]:
                continue
            data = os.read(descriptor, size - len(output))
            if not data:
                raise RuntimeError(f"Speech backend exited. See {self.runtime / 'backend.log'}")
            output.extend(data)
        return bytes(output)

    def _result(self, timeout):
        deadline = time.monotonic() + timeout
        size = struct.unpack("<I", self._read(4, deadline))[0]
        if not 0 < size <= 1024 * 1024:
            raise RuntimeError("Invalid response size from speech backend")
        result = json.loads(self._read(size, deadline))
        if not isinstance(result, dict):
            raise RuntimeError("Invalid speech backend response")
        return result

    def transcribe(self, pcm):
        if len(pcm) % 2 or len(pcm) > RATE * MAX_SECONDS * 2:
            raise ValueError("Invalid PCM16 audio length")
        # Digital zero cannot contain speech; do not send it to a hallucination-prone decoder.
        if not any(pcm):
            return ""
        pieces = []
        for segment in split_at_pauses(pcm):
            if self.stopped.is_set() or self.process is None:
                raise RuntimeError("Speech inference cancelled")
            if not any(segment):
                continue
            payload = memoryview(struct.pack("<I", len(segment) // 2) + segment)
            # Unbuffered pipes may write only part of a large audio frame.
            while payload:
                written = self.process.stdin.write(payload)
                if not written:
                    raise RuntimeError("Speech backend closed its input")
                payload = payload[written:]
            result = self._result(600)
            if not isinstance(result.get("text"), str):
                raise RuntimeError("Speech backend returned no transcript")
            pieces.append(result["text"])
        return " ".join(" ".join(pieces).split())

    def stop(self):
        self.stopped.set()
        with self.lock:
            process, self.process = self.process, None
        if process:
            if process.poll() is None:
                process.terminate()
                try:
                    process.wait(timeout=3)
                except subprocess.TimeoutExpired:
                    process.kill()
                    process.wait()
            process.stdin.close()
            process.stdout.close()
        if self.hotwords_path is not None:
            self.hotwords_path.unlink(missing_ok=True)
            self.hotwords_path = None
