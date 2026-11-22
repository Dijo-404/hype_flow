"""Keep every captured sample. PipeWire handles devices and resampling."""

from array import array
from collections import deque
import json
import math
import subprocess
import threading

from .backend import MAX_SECONDS, RATE

CHUNK = 1600  # 50 ms of 16 kHz PCM16.
PREROLL_BYTES = RATE  # 0.5 s of audio from before the shortcut, kept only while the microphone is held ready.
HOLD_SECONDS = 30


def level(pcm):
    samples = array("h")
    samples.frombytes(pcm[:len(pcm) // 2 * 2])
    if not samples:
        return 0.0
    rms = math.sqrt(sum(sample * sample for sample in samples) / len(samples)) / 32768
    return min(1.0, rms * 8)


def sources():
    """PipeWire microphones as (node name, description), for the settings picker."""
    try:
        nodes = json.loads(subprocess.run(["pw-dump"], capture_output=True, check=True, timeout=3).stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return []
    found = {}
    for node in nodes:
        props = (node.get("info") or {}).get("props") or {}
        if node.get("type") == "PipeWire:Interface:Node" and props.get("media.class") in ("Audio/Source", "Audio/Source/Virtual"):
            name = props.get("node.name")
            if name:
                found[name] = props.get("node.description") or name
    return sorted(found.items(), key=lambda item: item[1].casefold())


class Capture:
    """One pw-record process. Between dictations it only keeps a short pre-roll ring."""

    idle = None  # The capture held ready after the last dictation, if enabled.
    idle_lock = threading.Lock()

    def __init__(self, target):
        self.target = target
        self.lock = threading.Lock()
        self.session = None
        self.ring = deque()
        self.ring_bytes = 0
        self.timer = None
        self.error = None
        self.closing = False
        self.process = subprocess.Popen([
            "pw-record", "--raw", "--format", "s16", "--rate", str(RATE),
            "--channels", "1", "--quality", "10", "--latency", "50ms",
            "--target", target, "--properties", "{ application.name = HypeFlow media.role = Communication }", "-",
        ], stdout=subprocess.PIPE, stderr=subprocess.PIPE)
        self.thread = threading.Thread(target=self._read, daemon=True)
        self.thread.start()

    def alive(self):
        return self.process.poll() is None and self.error is None

    def _read(self):
        try:
            while data := self.process.stdout.read(CHUNK):
                with self.lock:
                    session = self.session
                    if session is None:
                        self.ring.append(data)
                        self.ring_bytes += len(data)
                        while self.ring_bytes > PREROLL_BYTES:
                            self.ring_bytes -= len(self.ring.popleft())
                if session:
                    session.feed(data)
            self.process.wait()
            detail = self.process.stderr.read().decode(errors="replace").strip()
            # pw-record exits with status 1 when terminated; only an exit nobody asked for is a failure.
            if not self.closing and self.process.returncode not in (0, -15):
                self.error = RuntimeError("Microphone capture failed: " + detail[-400:])
        except (OSError, ValueError) as error:
            self.error = error
        finally:
            self.process.stdout.close()
            self.process.stderr.close()
            with self.lock:
                session = self.session
            if session:
                session.finish()

    def attach(self, session, preroll):
        with self.lock:
            audio = list(self.ring) if preroll else []
            self.ring.clear()
            self.ring_bytes = 0
            self.session = session
        return audio

    def detach(self):
        with self.lock:
            self.session = None

    def close(self):
        self.closing = True
        if self.timer:
            self.timer.cancel()
        if self.process.poll() is None:
            self.process.terminate()
        self.thread.join(timeout=3)
        if self.thread.is_alive():
            self.process.kill()
            self.thread.join(timeout=3)

    @classmethod
    def take(cls, target):
        """Reuse the ready microphone when it matches, otherwise open a new one."""
        with cls.idle_lock:
            capture, cls.idle = cls.idle, None
        if capture:
            capture.timer.cancel()
            if capture.target == target and capture.alive():
                return capture, True
            capture.close()
        return cls(target), False

    def park(self, seconds):
        """Keep this microphone open for the next dictation, then close it."""
        with Capture.idle_lock:
            previous, Capture.idle = Capture.idle, self
        if previous:
            previous.close()
        self.timer = threading.Timer(seconds, Capture.release, (self,))
        self.timer.daemon = True
        self.timer.start()

    @classmethod
    def release(cls, only=None):
        with cls.idle_lock:
            if only is not None and cls.idle is not only:
                return
            capture, cls.idle = cls.idle, None
        if capture:
            capture.close()


class Recorder:
    def __init__(self, target, on_frame, on_end, hold=False):
        self.target = target
        self.on_frame = on_frame
        self.on_end = on_end
        self.hold = hold
        self.capture = None
        self.parts = []
        self.error = None
        self.bytes_recorded = 0
        self.received = threading.Event()
        self.ended = False

    def start(self):
        self.capture, warm = Capture.take(self.target)
        self.parts = self.capture.attach(self, preroll=warm)
        if warm:
            self.received.set()
        else:
            threading.Thread(target=self._check_input, daemon=True).start()

    def _check_input(self):
        if not self.received.wait(5) and self.capture.alive():
            self.error = RuntimeError("No microphone audio. Check the selected input device and PipeWire.")
            self.capture.close()

    def feed(self, data):
        if self.ended:
            return
        self.parts.append(data)
        self.received.set()
        self.bytes_recorded += len(data)
        self.on_frame(level(data), self.bytes_recorded / (RATE * 2))
        if self.bytes_recorded >= MAX_SECONDS * RATE * 2:
            self.finish()

    def finish(self):
        if not self.ended:
            self.ended = True
            self.on_end()

    def stop(self):
        capture = self.capture
        self.ended = True
        capture.detach()
        if self.hold and capture.alive() and not self.error:
            capture.park(HOLD_SECONDS)
        else:
            capture.close()
        error = self.error or capture.error
        if error:
            raise error
        # The backend accepts five minutes; if pre-roll pushes past that, drop the oldest pre-roll.
        return b"".join(self.parts)[-MAX_SECONDS * RATE * 2:]
