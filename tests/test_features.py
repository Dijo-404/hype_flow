"""Features adopted from Handy and OpenWhispr: near-miss vocabulary, cues, media pause, hook, mic hold, idle unload."""

import io
import os
from pathlib import Path
import queue
import tempfile
import threading
import time
import unittest
from unittest.mock import Mock, patch
import wave

from dictation.audio import PREROLL_BYTES, Capture, Recorder
from dictation.backend import Engine
from dictation.cleanup import near_misses, process, soundex
from dictation.config import load
from dictation.controller import Controller
from dictation.feedback import MediaPause, cue_wav
from dictation.hook import run_hook
from dictation.output import deliver

VOCABULARY = ["ChargeBee", "OpenAI", "niri", "Sinead"]


class NearMissTests(unittest.TestCase):
    def test_soundex_reference_values(self):
        self.assertEqual([soundex(word) for word in ("Robert", "Rupert", "Ashcraft", "Tymczak", "Pfister")],
                         ["R163", "R163", "A261", "T522", "P236"])

    def test_misheard_and_split_words_snap_to_vocabulary(self):
        self.assertEqual(near_misses("I use Charge B for billing.", VOCABULARY, 0.18), "I use ChargeBee for billing.")
        self.assertEqual(near_misses("open a i is great", VOCABULARY, 0.18), "OpenAI is great")
        self.assertEqual(near_misses("talk to Shunade", VOCABULARY, 0.18), "talk to Sinead")

    def test_ordinary_words_and_punctuation_boundaries_are_kept(self):
        self.assertEqual(near_misses("near the river", VOCABULARY, 0.18), "near the river")
        self.assertEqual(near_misses("Charge B, che", VOCABULARY, 0.18), "ChargeBee, che")

    def test_explicit_rules_win_and_code_is_untouched(self):
        values = {"prompt": "OpenAI, ChargeBee", "corrections": "open ai tools => the SDK", "blocked_words": "um"}
        self.assertEqual(process("open ai tools, um, via `charge b`", values), "the SDK via `charge b`")
        self.assertEqual(process("open a i works", {**values, "fuzzy_vocabulary": False}), "open a i works")


class DeliveryOptionTests(unittest.TestCase):
    def deliver(self, app_id, **options):
        target = {"id": 1, "app_id": app_id}
        with patch("dictation.output.copy_text") as copy, patch("dictation.output.time.sleep"), \
                patch("dictation.output.read_clipboard", return_value=None), \
                patch("dictation.output.focused_window", return_value=target), \
                patch("dictation.output.threading.Timer"), patch("dictation.output.subprocess.run") as run:
            result = deliver("hello", target, True, **options)
        return result, copy.call_args_list[0].args[0], [call.args[0] for call in run.call_args_list]

    def test_trailing_space_and_enter_in_chat_apps(self):
        result, copied, commands = self.deliver("firefox", trailing_space=True, press_enter=True)
        self.assertEqual((result, copied), ("Pasted and sent", "hello "))
        self.assertIn(["wtype", "-k", "Return"], commands)

    def test_enter_is_never_sent_to_a_terminal(self):
        result, _, commands = self.deliver("Alacritty", press_enter=True)
        self.assertEqual(result, "Pasted")
        self.assertNotIn(["wtype", "-k", "Return"], commands)


class HookTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.environment = patch.dict(os.environ, XDG_CONFIG_HOME=self.temporary.name)
        self.environment.start()
        self.path = Path(self.temporary.name) / "hype-flow/transcript-hook"
        self.path.parent.mkdir()

    def tearDown(self):
        self.environment.stop()
        self.temporary.cleanup()

    def script(self, body, mode=0o700):
        self.path.write_text("#!/bin/sh\n" + body + "\n")
        self.path.chmod(mode)

    def test_no_hook_leaves_text_alone(self):
        self.assertEqual(run_hook("hello"), ("hello", None))

    def test_hook_output_replaces_text(self):
        self.script("tr a-z A-Z")
        self.assertEqual(run_hook("hello"), ("HELLO", None))

    def test_failing_or_untrusted_hook_keeps_cleaned_text(self):
        self.script("exit 3")
        self.assertEqual(run_hook("hello")[0], "hello")
        self.assertIn("status 3", run_hook("hello")[1])
        self.script("tr a-z A-Z", mode=0o722)
        self.assertEqual(run_hook("hello")[0], "hello")
        self.assertIn("skipped", run_hook("hello")[1])


class FeedbackTests(unittest.TestCase):
    def test_cues_are_short_valid_wavs(self):
        for name in ("start", "stop"):
            with wave.open(io.BytesIO(cue_wav(name))) as audio:
                self.assertAlmostEqual(audio.getnframes() / audio.getframerate(), 0.23, places=2)

    def test_only_playing_players_are_paused_and_resumed(self):
        def run(args, **_):
            if args[1] == "--list-all":
                return Mock(stdout="spotify\nmpv\n")
            if args[-1] == "status":
                return Mock(stdout="Playing\n" if args[2] == "spotify" else "Paused\n")
            return Mock(stdout="")

        media = MediaPause()
        with patch("dictation.feedback.subprocess.run", side_effect=run) as calls, \
                patch("dictation.feedback._background", side_effect=lambda function, *args: function(*args)):
            media.pause()
            media.resume()
        actions = [call.args[0][1:] for call in calls.call_args_list if call.args[0][-1] in ("pause", "play")]
        self.assertEqual(actions, [["--player", "spotify", "pause"], ["--player", "spotify", "play"]])


class MicrophoneTests(unittest.TestCase):
    def test_ready_microphone_keeps_only_recent_preroll(self):
        chunk = b"\x01\x00" * 800
        process = Mock(returncode=0)
        process.stdout = io.BytesIO(chunk * 40)
        process.stderr = io.BytesIO(b"")
        process.poll.return_value = None
        with patch("dictation.audio.subprocess.Popen", return_value=process):
            capture = Capture("auto")
            capture.thread.join(2)
        self.assertLessEqual(capture.ring_bytes, PREROLL_BYTES)
        self.assertEqual(b"".join(capture.attach(Mock(), preroll=True)), chunk * (PREROLL_BYTES // len(chunk)))

    def test_requested_close_is_not_a_capture_failure(self):
        for closing, failed in ((True, False), (False, True)):
            capture = Capture.__new__(Capture)
            capture.lock, capture.session, capture.ring, capture.ring_bytes = threading.Lock(), None, [], 0
            capture.error, capture.closing = None, closing
            # pw-record exits with status 1 when terminated.
            capture.process = Mock(returncode=1, stdout=io.BytesIO(b""), stderr=io.BytesIO(b""))
            capture._read()
            self.assertEqual(capture.error is not None, failed)

    def test_held_microphone_is_reused_with_preroll(self):
        capture = Mock(target="auto", timer=Mock())
        capture.alive.return_value = True
        capture.attach.return_value = [b"before"]
        Capture.idle = capture
        try:
            recorder = Recorder("auto", Mock(), Mock())
            recorder.start()
        finally:
            Capture.idle = None
        self.assertIs(recorder.capture, capture)
        capture.attach.assert_called_once_with(recorder, preroll=True)
        self.assertEqual(recorder.parts, [b"before"])


class EngineLifecycleTests(unittest.TestCase):
    def test_concurrent_ensure_loads_once(self):
        engine = Engine({}, Path("/tmp"))

        def start():
            time.sleep(0.05)
            engine.process = Mock()
            engine.process.poll.return_value = None

        with patch.object(engine, "start", side_effect=start) as spawn:
            threads = [threading.Thread(target=engine.ensure) for _ in range(4)]
            for thread in threads:
                thread.start()
            for thread in threads:
                thread.join()
        spawn.assert_called_once()

    def test_unloaded_model_reloads_alongside_recording(self):
        engine, pending = Mock(), queue.Queue()
        with tempfile.TemporaryDirectory() as runtime:
            controller = Controller({"input_target": "auto", "paste": True}, Path(runtime), engine, Mock(),
                                    lambda function, *args: pending.put((function, args)))
            controller.state, controller.ready = "idle", True
            controller.unload()
            engine.stop.assert_called_once()
            self.assertTrue(controller.unloaded)
            with patch("dictation.controller.Recorder"), patch("dictation.controller.focused_window"):
                controller.start()
                deadline = time.monotonic() + 1
                while not engine.ensure.called and time.monotonic() < deadline:
                    time.sleep(0.01)
        engine.ensure.assert_called()
        self.assertFalse(controller.unloaded)

    def test_silent_microphone_is_reported(self):
        engine, pending = Mock(), queue.Queue()
        engine.transcribe.return_value = ""
        with tempfile.TemporaryDirectory() as runtime:
            controller = Controller({"input_target": "auto", "paste": True}, Path(runtime), engine, Mock(),
                                    lambda function, *args: pending.put((function, args)))
            controller.state, controller.ready = "recording", True
            controller.recorder = Mock()
            controller.recorder.stop.return_value = bytes(3200)
            controller.stop()
            deadline = time.monotonic() + 2
            while time.monotonic() < deadline and controller.state == "transcribing":
                try:
                    function, args = pending.get(timeout=0.05)
                    function(*args)
                except queue.Empty:
                    pass
        self.assertIn("silent", controller.message)


class AudioProcessingTests(unittest.TestCase):
    def engine_args(self, models=("silero_vad.onnx", "gtcrn_simple.onnx"), **options):
        from dictation.setup import MODEL_FILES
        with tempfile.TemporaryDirectory() as root:
            model = Path(root) / "models/parakeet"
            model.mkdir(parents=True)
            for name in MODEL_FILES:
                (model / name).write_text("")
            for name in models:
                (model.parent / name).write_text("")
            values = {"model": str(model), "server": __file__, "language": "en", "threads": 2, **options}
            engine = Engine(values, Path(root))
            with patch("dictation.backend.sandbox", return_value=[]), \
                    patch("dictation.backend.subprocess.Popen") as spawn, \
                    patch.object(Engine, "_result", return_value={"ready": True}):
                engine.start()
            return [str(part).rsplit("/", 1)[-1] for part in spawn.call_args.args[0]]

    def test_silence_cutting_and_noise_suppression_reach_the_recognizer(self):
        self.assertNotIn("--vad", self.engine_args())
        args = self.engine_args(cut_silence=True, noise_suppression=True)
        self.assertEqual(args[args.index("--vad") + 1], "silero_vad.onnx")
        self.assertEqual(args[args.index("--denoise") + 1], "gtcrn_simple.onnx")

    def test_missing_audio_model_asks_for_an_update(self):
        with self.assertRaisesRegex(RuntimeError, "update speech support"):
            self.engine_args(models=(), cut_silence=True)

    def test_assets_need_the_audio_models(self):
        from dictation.setup import MODEL_FILES, NATIVE_VERSION, assets_ready
        with tempfile.TemporaryDirectory() as root:
            model, server = Path(root) / "models/parakeet", Path(root) / "recognizer"
            model.mkdir(parents=True)
            for name in (*MODEL_FILES, "bpe.vocab"):
                (model / name).write_text("")
            server.write_text("")
            server.with_name("recognizer.version").write_text(NATIVE_VERSION + "\n")
            values = {"model": str(model), "server": str(server)}
            self.assertFalse(assets_ready(values))
            for name in ("silero_vad.onnx", "gtcrn_simple.onnx"):
                (model.parent / name).write_text("")
            self.assertTrue(assets_ready(values))


class ConfigTests(unittest.TestCase):
    def test_new_settings_are_validated(self):
        with tempfile.TemporaryDirectory() as root, patch.dict(os.environ, XDG_CONFIG_HOME=root):
            path = Path(root) / "hype-flow/config.toml"
            path.parent.mkdir()
            path.write_text("fuzzy_tolerance = 0.9\n")
            with self.assertRaisesRegex(ValueError, "tolerance"):
                load()
            path.write_text("unload_minutes = 7\n")
            with self.assertRaisesRegex(ValueError, "Unload"):
                load()
            path.write_text("unload_minutes = 0\nfuzzy_tolerance = 0.2\nsounds = false\n")
            values = load()
            self.assertEqual((values["unload_minutes"], values["fuzzy_tolerance"], values["sounds"]), (0, 0.2, False))


if __name__ == "__main__":
    unittest.main()
