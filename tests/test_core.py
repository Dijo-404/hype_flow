from array import array
import io
import json
from pathlib import Path
import queue
import tempfile
import time
import unittest
from unittest.mock import Mock, patch
import wave

from dictation.audio import Recorder, level
from dictation.backend import Engine, read_wav, sandbox, wav_bytes
from dictation.config import load, save
from dictation.controller import Controller
from dictation.output import deliver, is_terminal, restore_clipboard
from dictation.setup import download


class CoreTests(unittest.TestCase):
    def test_wav_preserves_every_sample(self):
        pcm = array("h", [-32768, -1, 0, 1, 32767]).tobytes()
        with wave.open(io.BytesIO(wav_bytes(pcm))) as audio:
            self.assertEqual((audio.getnchannels(), audio.getsampwidth(), audio.getframerate()), (1, 2, 16000))
            self.assertEqual(audio.readframes(100), pcm)

    def test_reject_wrong_sample_rate(self):
        with tempfile.TemporaryDirectory() as root:
            path = Path(root) / "audio.wav"
            with wave.open(str(path), "wb") as audio:
                audio.setparams((1, 2, 48000, 0, "NONE", "not compressed"))
                audio.writeframes(b"\x00\x00" * 100)
            with self.assertRaisesRegex(ValueError, "16 kHz"):
                read_wav(path)

    def test_pipe_request_preserves_pcm_even_with_partial_writes(self):
        import struct
        engine = Engine({}, Path("/tmp"))
        engine.process = Mock()
        sent = bytearray()

        def write(data):
            count = min(7, len(data))
            sent.extend(data[:count])
            return count

        engine.process.stdin.write.side_effect = write
        engine._result = Mock(return_value={"text": "Recognized words."})
        pcm = array("h", [-32768, -1, 0, 1, 32767]).tobytes()
        self.assertEqual(engine.transcribe(pcm), "Recognized words.")
        self.assertEqual(sent, struct.pack("<I", 5) + pcm)

    def test_malformed_native_response_is_rejected(self):
        import struct
        engine = Engine({}, Path("/tmp"))
        engine._read = Mock(return_value=struct.pack("<I", 2 * 1024 * 1024))
        with self.assertRaisesRegex(RuntimeError, "response size"):
            engine._result(1)

    def test_level(self):
        self.assertEqual(level(b"\x00\x00" * 100), 0)
        self.assertEqual(level(array("h", [32767] * 100).tobytes()), 1)

    def test_digital_silence_never_reaches_decoder(self):
        engine = Engine({}, Path("/tmp"))
        engine.process = Mock()
        self.assertEqual(engine.transcribe(b"\x00\x00" * 16000), "")
        engine.process.stdin.write.assert_not_called()

    def test_invalid_pcm_rejected_before_native_request(self):
        engine = Engine({}, Path("/tmp"))
        with self.assertRaisesRegex(ValueError, "PCM16"):
            engine.transcribe(b"\x01")

    def test_missing_microphone_times_out_without_hanging(self):
        recorder = Recorder("missing", Mock(), Mock())
        recorder.capture = Mock()
        recorder.capture.alive.return_value = True
        recorder.received = Mock()
        recorder.received.wait.return_value = False
        recorder._check_input()
        self.assertIsInstance(recorder.error, RuntimeError)
        recorder.capture.close.assert_called_once()

    def test_stopped_engine_cannot_spawn_after_shutdown(self):
        engine = Engine({"server": __file__, "model": __file__, "language": "auto", "threads": 1, "gpu": False}, Path("/tmp"))
        engine.stop()
        with patch("dictation.backend.subprocess.Popen") as spawn:
            with self.assertRaisesRegex(RuntimeError, "cancelled"):
                engine.start()
            spawn.assert_not_called()

    def deliver_with(self, target, focused, refocused=True):
        """Run deliver with niri, wl-clipboard, wtype and the restore timer replaced."""
        with patch("dictation.output.copy_text") as copy, patch("dictation.output.time.sleep"), \
                patch("dictation.output.read_clipboard", return_value=("text/plain", b"before")), \
                patch("dictation.output.focused_window", side_effect=focused), \
                patch("dictation.output.threading.Timer") as timer, \
                patch("dictation.output.subprocess.run") as run:
            if not refocused:
                run.side_effect = lambda args, **_: (_ for _ in ()).throw(OSError("gone")) if args[:3] == ["niri", "msg", "action"] else None
            result = deliver("hello", target, True)
        typed = [call.args[0] for call in run.call_args_list if call.args[0][0] == "wtype"]
        return result, copy, typed, timer

    def test_focus_change_returns_to_original_window_and_pastes(self):
        target = {"id": 1, "app_id": "firefox"}
        result, _, typed, timer = self.deliver_with(target, [{"id": 2}, target])
        self.assertEqual(result, "Pasted")
        self.assertEqual(len(typed), 1)
        self.assertEqual(timer.call_args.args[2], (("text/plain", b"before"), "hello"))

    def test_closed_original_window_copies_without_typing(self):
        result, copy, typed, timer = self.deliver_with({"id": 1}, [{"id": 2}, {"id": 2}], refocused=False)
        self.assertEqual(result, "Copied · focus changed")
        self.assertEqual(typed, [])
        copy.assert_any_call("hello", primary=True)
        timer.assert_not_called()  # The transcript stays on the clipboard for a manual paste.

    def test_terminal_paste(self):
        target = {"id": 1, "app_id": "Alacritty"}
        result, _, typed, _ = self.deliver_with(target, [target])
        self.assertEqual(result, "Pasted")
        self.assertIn("shift", typed[0])
        self.assertNotIn("hello", typed[0])

    def test_terminal_names_match_whole_words_when_short(self):
        for app_id in ("Alacritty", "org.gnome.Console", "st-256color", "com.mitchellh.ghostty", "org.kde.konsole"):
            self.assertTrue(is_terminal(app_id), app_id)
        for app_id in ("steam", "com.system76.CosmicEdit", "firefox", "", None):
            self.assertFalse(is_terminal(app_id), app_id)

    def test_clipboard_restores_only_while_transcript_is_current(self):
        def clipboard(text):
            return lambda args, **_: Mock(returncode=0, stdout=text.encode()) if args[0] == "wl-paste" else Mock()
        with patch("dictation.output.subprocess.run", side_effect=clipboard("hello")) as run:
            restore_clipboard(("image/png", b"png"), "hello")
        self.assertEqual(run.call_args.args[0], ["wl-copy", "--type", "image/png"])
        self.assertEqual(run.call_args.kwargs["input"], b"png")
        with patch("dictation.output.subprocess.run", side_effect=clipboard("hello")) as run:
            restore_clipboard(None, "hello")
        self.assertEqual(run.call_args.args[0], ["wl-copy", "--clear"])
        with patch("dictation.output.subprocess.run", side_effect=clipboard("copied later")) as run:
            restore_clipboard(("text/plain", b"before"), "hello")
        self.assertEqual(run.call_count, 1)  # Only the read; a newer copy is never overwritten.

    def test_sandbox_falls_back_without_bubblewrap(self):
        with patch("dictation.backend.shutil.which", return_value=None):
            self.assertEqual(sandbox(), [])
        with patch("dictation.backend.shutil.which", return_value="/usr/bin/bwrap"), \
                patch("dictation.backend.subprocess.run", return_value=Mock(returncode=1)):
            self.assertEqual(sandbox(), [])
        with patch("dictation.backend.shutil.which", return_value="/usr/bin/bwrap"), \
                patch("dictation.backend.subprocess.run", return_value=Mock(returncode=0)):
            self.assertEqual(sandbox()[:2], ["/usr/bin/bwrap", "--unshare-net"])

    def test_invalid_config_fails(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            directory = Path(root) / "hype-flow"
            directory.mkdir()
            (directory / "config.toml").write_text('threads = true\n')
            with self.assertRaisesRegex(ValueError, "threads"):
                load()

    def test_cleanup_preferences_are_validated_before_saving(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            values = load()
            values["corrections"] = "missing separator"
            with self.assertRaisesRegex(ValueError, "Correction line"):
                save(values)
            values["corrections"] = ""
            values["boost_score"] = 8.0
            with self.assertRaisesRegex(ValueError, "Vocabulary strength"):
                save(values)

    def test_boost_score_accepts_integer_toml_values(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            directory = Path(root) / "hype-flow"
            directory.mkdir()
            (directory / "config.toml").write_text("boost_score = 2\n")
            self.assertEqual(load()["boost_score"], 2.0)

    def test_saved_settings_roundtrip(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            values = load()
            values.update(language="hi", prompt='नमस्ते, "niri"\nproject', paste=False)
            save(values)
            self.assertEqual(load(), values)

    def test_previous_default_paths_migrate_without_overwriting_preferences(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root, XDG_DATA_HOME=root):
            directory = Path(root) / "hype-flow"
            directory.mkdir()
            original = f'model = "{directory}/models/ggml-large-v3.bin"\nserver = "{directory}/backend/whisper-server-linux-x64-vulkan"\nprompt = "project names"\npaste = false\n'
            path = directory / "config.toml"
            path.write_text(original)
            values = load()
            self.assertEqual(values["model"], str(directory / "models/parakeet-unified-en-int8"))
            self.assertEqual(values["server"], str(directory / "backend/parakeet/recognizer"))
            self.assertEqual(values["prompt"], "project names")
            self.assertFalse(values["paste"])
            self.assertEqual(path.read_text(), original)

    def test_custom_model_paths_are_preserved(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            directory = Path(root) / "hype-flow"
            directory.mkdir()
            (directory / "config.toml").write_text('model = "/custom/parakeet"\nserver = "/custom/recognizer"\n')
            self.assertEqual(load()["model"], "/custom/parakeet")
            self.assertEqual(load()["server"], "/custom/recognizer")

    def test_applying_new_model_saves_previous_config(self):
        with tempfile.TemporaryDirectory() as root, patch.dict("os.environ", XDG_CONFIG_HOME=root):
            directory = Path(root) / "hype-flow"
            directory.mkdir()
            path = directory / "config.toml"
            original = 'model = "/custom/old"\nserver = "/custom/backend"\nprompt = "Saved names"\n'
            path.write_text(original)
            values = load()
            values.update(model="/new/parakeet", server="/new/recognizer")
            save(values)
            backup = directory / "config.toml.previous"
            self.assertEqual(backup.read_text(), original)
            self.assertEqual(backup.stat().st_mode & 0o777, 0o600)
            self.assertEqual(load()["prompt"], "Saved names")

    def test_corrupt_download_never_replaces_model(self):
        with tempfile.TemporaryDirectory() as root:
            target = Path(root) / "model.bin"
            target.write_bytes(b"original")
            response = Mock()
            response.status, response.headers = 200, {}
            response.read.side_effect = [b"bad download", b""]
            response.__enter__ = Mock(return_value=response)
            response.__exit__ = Mock(return_value=False)
            with patch("dictation.setup.urllib.request.urlopen", return_value=response):
                with self.assertRaisesRegex(RuntimeError, "SHA-256 mismatch"):
                    download("https://example.invalid/model.bin", target, "0" * 64)
            self.assertEqual(target.read_bytes(), b"original")
            self.assertFalse(target.with_suffix(".bin.part").exists())


class ControllerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.runtime = Path(self.temporary.name)
        self.pending = queue.Queue()
        self.engine = Mock()
        self.view = Mock()
        self.config = {"input_target": "auto", "paste": True}
        self.controller = Controller(self.config, self.runtime, self.engine, self.view,
                                     lambda function, *args: self.pending.put((function, args)))
        self.controller.state, self.controller.ready = "idle", True

    def tearDown(self):
        self.temporary.cleanup()

    def drain(self):
        deadline = time.monotonic() + 2
        while time.monotonic() < deadline:
            try:
                function, args = self.pending.get(timeout=0.02)
                function(*args)
            except queue.Empty:
                if not self.controller.busy:
                    return
        self.fail("worker did not finish")

    def test_failed_delivery_preserves_transcript(self):
        recorder = Mock()
        recorder.stop.return_value = b"\0\0" * 100
        self.controller.recorder = recorder
        self.engine.transcribe.return_value = "Recovered words"
        with patch("dictation.controller.deliver", side_effect=OSError("wtype unavailable")):
            self.controller.stop()
            self.drain()
        self.assertEqual(self.controller.state, "error")
        self.assertIn("Copy last transcript", self.controller.message)
        path = self.runtime / "last-transcript.txt"
        self.assertEqual(path.read_text(), "Recovered words")
        self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_cleanup_happens_before_paste_and_original_is_recoverable(self):
        self.controller.config.update(corrections="open ai => OpenAI")
        self.controller.recorder = Mock()
        self.engine.transcribe.return_value = "Um, open ai works."
        with patch("dictation.controller.deliver", return_value="Pasted") as deliver_text:
            self.controller.stop()
            self.drain()
        self.assertEqual(deliver_text.call_args.args[0], "OpenAI works.")
        self.assertEqual((self.runtime / "last-transcript.txt").read_text(), "OpenAI works.")
        raw = self.runtime / "last-raw-transcript.txt"
        self.assertEqual(raw.read_text(), "Um, open ai works.")
        self.assertEqual(raw.stat().st_mode & 0o777, 0o600)

    def test_fully_filtered_text_does_not_paste(self):
        self.controller.recorder = Mock()
        self.engine.transcribe.return_value = "Um. Uh."
        with patch("dictation.controller.deliver") as deliver_text:
            self.controller.stop()
            self.drain()
            deliver_text.assert_not_called()
        self.assertEqual((self.runtime / "last-raw-transcript.txt").read_text(), "Um. Uh.")
        self.assertEqual((self.runtime / "last-transcript.txt").read_text(), "")
        self.assertIn("filtered", self.controller.message)

    def test_cancelled_inference_cannot_deliver_late_text(self):
        import threading
        started, release = threading.Event(), threading.Event()

        def transcribe(_):
            started.set()
            release.wait(2)
            return "stale text"

        self.engine.transcribe.side_effect = transcribe
        self.controller.recorder = Mock()
        with patch("dictation.controller.deliver") as deliver_text:
            self.controller.stop()
            self.assertTrue(started.wait(1))
            self.controller.cancel()
            release.set()
            self.drain()
            deliver_text.assert_not_called()
        self.assertFalse((self.runtime / "last-transcript.txt").exists())
        self.assertFalse((self.runtime / "last-raw-transcript.txt").exists())
        self.assertEqual(self.controller.state, "idle")

    def test_capture_end_and_hotkey_stop_only_transcribe_once(self):
        recorder = Mock()
        recorder.stop.return_value = b"\0\0" * 100
        self.controller.recorder = recorder
        self.engine.transcribe.return_value = ""
        self.controller.state = "recording"
        self.controller.command("toggle")
        self.controller.capture_ended(self.controller.generation)
        self.drain()
        self.engine.transcribe.assert_called_once()

    def test_old_capture_callback_cannot_stop_new_recording(self):
        self.controller.generation = 2
        self.controller.state = "recording"
        self.controller.recorder = Mock()
        self.controller.capture_ended(1)
        self.assertEqual(self.controller.state, "recording")
        self.controller.recorder.stop.assert_not_called()

    def test_cancelled_warmup_only_hides_overlay(self):
        self.controller.state = "loading"
        self.controller.busy = True
        self.controller.cancel()
        self.engine.stop.assert_not_called()
        self.assertTrue(self.controller.busy)
        self.view.assert_called_with("loading", "Loading speech model", False)


if __name__ == "__main__":
    unittest.main()
