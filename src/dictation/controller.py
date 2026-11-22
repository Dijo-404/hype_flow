"""All state transitions run on the UI thread; workers only post results."""

import threading

from .audio import Capture, Recorder
from .output import deliver, focused_window
from .cleanup import process
from .feedback import MediaPause, play_cue
from .hook import run_hook


class Controller:
    def __init__(self, config, runtime, engine, view, schedule):
        self.config, self.runtime, self.engine = config, runtime, engine
        self.view, self.schedule = view, schedule
        self.state = "loading"
        self.message = "Loading speech model"
        self.generation = 0
        self.recorder = None
        self.target = None
        self.duration = 0.0
        self.ready = False
        self.busy = False
        self.closing = False
        self.unloaded = False  # The model was released while idle; the next dictation reloads it.
        self.media = MediaPause()

    def change(self, state, message, visible=True):
        self.state, self.message = state, message
        self.view(state, message, visible)

    def work(self, operation, finished):
        self.busy = True

        def run():
            try:
                result, error = operation(), None
            except Exception as failure:
                result, error = None, failure
            self.schedule(complete, result, error)

        def complete(result, error):
            self.busy = False
            if self.closing:
                self.engine.stop()
            else:
                finished(result, error)
            return False

        threading.Thread(target=run, daemon=True).start()

    def warm(self):
        self.engine.prepare()
        self.unloaded = False
        self.change("loading", "Loading speech model", visible=False)

        def finished(_, error):
            self.ready = error is None
            if error:
                self.engine.stop()
                self.change("error", str(error))
            else:
                self.change("idle", "Ready", visible=False)

        self.work(self.engine.ensure, finished)

    def unload(self):
        """Free the model's memory while idle. Recording can start at once; the model reloads alongside it."""
        if self.state in ("idle", "done") and not self.busy and self.recorder is None and self.ready:
            self.engine.stop()
            self.unloaded = True

    def command(self, command):
        if command == "status":
            return {"state": self.state, "message": self.message, "seconds": round(self.duration, 1)}
        if command == "cancel":
            self.cancel()
        elif command == "toggle":
            if self.state in ("recording", "starting"):
                self.stop()
            elif self.busy:
                self.view(self.state, self.message, True)
            elif not self.ready:
                self.warm()
            else:
                self.start()
        elif command != "quit":
            raise ValueError("Unknown command")
        return {"state": self.state, "message": self.message}

    def start(self):
        self.generation += 1
        generation = self.generation
        self.duration = 0.0
        self.change("starting", "Opening microphone")

        def frame(volume, duration):
            self.schedule(self.frame, generation, volume, duration)

        def ended():
            self.schedule(self.capture_ended, generation)

        # Open the microphone first: PipeWire takes about 200 ms to deliver audio.
        self.recorder = Recorder(self.config["input_target"], frame, ended, hold=self.config.get("keep_mic_ready", False))
        try:
            self.recorder.start()
        except OSError as error:
            self.recorder = None
            self.change("error", str(error))
            return
        self.target = focused_window()
        if self.config.get("pause_media", False):
            self.media.pause()
        if self.unloaded:
            self.unloaded = False

            def preload():
                try:
                    self.engine.ensure()
                except Exception:
                    pass  # Transcription loads again and reports the error.

            threading.Thread(target=preload, daemon=True).start()

    def frame(self, generation, volume, duration):
        if generation == self.generation and self.state in ("starting", "recording"):
            self.duration = duration
            if self.state == "starting":
                self.change("recording", "Listening")
                if self.config.get("sounds", False):
                    play_cue("start", self.runtime)  # Only once audio flows, so the first word is never clipped.
            self.view("recording", "Listening", True, volume, duration)
        return False

    def capture_ended(self, generation):
        if generation == self.generation and self.state in ("starting", "recording"):
            self.stop()
        return False

    def stop(self):
        recorder, self.recorder = self.recorder, None
        if recorder is None:
            return
        generation, target = self.generation, self.target
        self.change("transcribing", "Transcribing")
        self.media.resume()
        if self.config.get("sounds", False):
            play_cue("stop", self.runtime)

        def transcribe():
            pcm = recorder.stop()
            if generation != self.generation:
                return ""
            self.engine.ensure()
            raw = self.engine.transcribe(pcm)
            text, note = run_hook(process(raw, self.config)) if raw else ("", None)
            return raw, text, note, not raw and not any(pcm)

        def finished(result, error):
            if generation != self.generation:
                return
            if error:
                self.ready = False
                self.engine.stop()
                self.change("error", str(error))
            else:
                raw, text, note, silent = result
                if not raw:
                    # Pure digital zero means a muted or dead input, not quiet speech.
                    self.change("done", "Microphone is silent · check it is not muted" if silent else "No speech detected")
                    return
                try:
                    for name, content in (("last-raw-transcript.txt", raw), ("last-transcript.txt", text)):
                        path = self.runtime / name
                        path.touch(mode=0o600, exist_ok=True)
                        path.chmod(0o600)
                        path.write_text(content)
                except OSError as failure:
                    self.change("error", "Could not save transcript for recovery. " + str(failure))
                    return
                if not text:
                    self.change("done", "All words filtered · original saved")
                    return
                self.change("delivering", "Copying transcript")

                def delivered(message, failure):
                    if generation != self.generation:
                        return
                    if failure:
                        self.change("error", "Paste failed. Use Copy last transcript in Hype Flow. " + str(failure))
                    else:
                        self.change("done", message + (" · " + note if note else ""))

                self.work(lambda: deliver(text, target, self.config["paste"],
                                          trailing_space=self.config.get("trailing_space", False),
                                          press_enter=self.config.get("press_enter", False)), delivered)

        self.work(transcribe, finished)

    def cancel(self):
        if self.state == "delivering":
            return  # Clipboard delivery has already started; do not claim it was cancelled.
        if self.state == "loading":
            self.view(self.state, self.message, False)
            return  # Warming is shared readiness work; hiding the overlay need not abort it.
        self.generation += 1
        self.media.resume()
        if self.recorder:
            recorder, self.recorder = self.recorder, None
            try:
                recorder.stop()
            except (OSError, RuntimeError):
                pass
        if self.busy:
            # Kill inference, then reload on the next toggle. Stale results cannot paste.
            self.ready = False
            self.engine.stop()
        self.change("idle", "Cancelled", visible=False)

    def close(self):
        self.closing = True
        self.cancel()
        self.engine.stop()
        Capture.release()
