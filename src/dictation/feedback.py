"""Start/stop cues and media pausing. Every helper runs off the UI thread and never raises."""

import io
import math
import struct
import subprocess
import threading
import wave

# OpenWhispr's dictation cues: two short sine notes, rising to start and falling to stop.
NOTES = {"start": (523.25, 659.25), "stop": (587.33, 440.0)}
NOTE_SECONDS, GAP_SECONDS, ATTACK_SECONDS, GAIN, RATE = 0.09, 0.025, 0.015, 0.2, 48000


def cue_wav(name):
    frames = bytearray()
    note = int(NOTE_SECONDS * RATE)
    attack = int(ATTACK_SECONDS * RATE)
    for frequency in NOTES[name]:
        for index in range(note):
            # Linear attack, exponential release: no clicks at the note edges.
            envelope = min(1.0, index / attack) * math.exp(-4.0 * index / note)
            frames += struct.pack("<h", int(32767 * GAIN * envelope * math.sin(2 * math.pi * frequency * index / RATE)))
        frames += bytes(2 * int(GAP_SECONDS * RATE))
    output = io.BytesIO()
    with wave.open(output, "wb") as audio:
        audio.setnchannels(1)
        audio.setsampwidth(2)
        audio.setframerate(RATE)
        audio.writeframes(bytes(frames))
    return output.getvalue()


def _background(function, *args):
    threading.Thread(target=function, args=args, daemon=True).start()


def play_cue(name, runtime):
    def play():
        try:
            path = runtime / f"cue-{name}.wav"
            if not path.exists():
                path.write_bytes(cue_wav(name))
            subprocess.run(["pw-play", "--media-role", "Notification", str(path)],
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, timeout=3)
        except (OSError, subprocess.SubprocessError):
            pass  # A missing cue never blocks dictation.
    _background(play)


class MediaPause:
    """Pause only players that are playing, then resume exactly those (OpenWhispr's MPRIS approach)."""

    def __init__(self):
        self.paused = []
        self.lock = threading.Lock()

    def pause(self):
        def run():
            with self.lock:
                try:
                    listed = subprocess.run(["playerctl", "--list-all"], capture_output=True, text=True, timeout=2)
                    for player in listed.stdout.split():
                        status = subprocess.run(["playerctl", "--player", player, "status"],
                                                capture_output=True, text=True, timeout=2)
                        if status.stdout.strip() == "Playing":
                            subprocess.run(["playerctl", "--player", player, "pause"], capture_output=True, timeout=2)
                            self.paused.append(player)
                except (OSError, subprocess.SubprocessError):
                    pass
        _background(run)

    def resume(self):
        def run():
            with self.lock:
                players, self.paused = self.paused, []
                for player in players:
                    try:
                        subprocess.run(["playerctl", "--player", player, "play"], capture_output=True, timeout=2)
                    except (OSError, subprocess.SubprocessError):
                        pass
        _background(run)
