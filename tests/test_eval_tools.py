"""Model-free checks for evaluation corpus transformations."""
import importlib.util
import math
import os
import shutil
import json
import subprocess
import sys
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from types import SimpleNamespace


def load(name):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).resolve().parents[1] / "tools" / f"{name}.py")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


prepare = load("prepare-eval")
model = load("check-model")
sweep = load("sweep-eval")


class EvalToolsTests(unittest.TestCase):
    def test_augment_reproducible_and_preserves_transcript(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            audio = root / "source.wav"
            prepare.write_audio(audio, [1000, -1000] * 80)
            records = [{"audio": str(audio), "text": "keep these words", "tags": ["quiet"]}]
            first = prepare.augmented(records, root)
            snapshot = [Path(record["audio"]).read_bytes() for record in first]
            second = prepare.augmented(records, root)
            self.assertEqual(first, second)
            self.assertEqual(snapshot, [Path(record["audio"]).read_bytes() for record in second])
            self.assertEqual(len(first), 5)
            self.assertTrue(all(record["text"] == "keep these words" for record in first))
            self.assertTrue(all("quiet" in record["tags"] for record in first))

    def test_noise_snr_and_long_checkpoint_positions(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            audio = root / "source.wav"
            source = [1000, -1000] * 8000
            prepare.write_audio(audio, source)
            records = [{"audio": str(audio), "text": "checkpoint"}]
            outputs = prepare.augmented(records, root)
            noisy = next(record for record in outputs if record["snr_db"] == 10)
            data = prepare.read_audio(noisy["audio"])
            leading = round(noisy["leading_silence_seconds"] * prepare.RATE)
            speech = data[leading:leading + len(source)]
            noise_power = sum((a - b) ** 2 for a, b in zip(speech, source)) / len(source)
            self.assertAlmostEqual(10 * math.log10(1000 ** 2 / noise_power), 10, delta=.05)
            self.assertEqual(len(data), leading + len(source) + round(noisy["trailing_silence_seconds"] * prepare.RATE))
            long = prepare.long_form(records, root)
            for record, target in zip(long, (60, 120, 180)):
                duration = len(prepare.read_audio(record["audio"])) / prepare.RATE
                self.assertGreaterEqual(duration, target)
                self.assertLess(duration, target + 2)
                self.assertEqual(record["text"].split(), ["checkpoint"] * len(record["checkpoints"]))
                self.assertEqual(record["checkpoints"][0]["start_seconds"], 0)
                self.assertTrue(all(a["start_seconds"] < b["start_seconds"] for a, b in zip(record["checkpoints"], record["checkpoints"][1:])))

    def test_golden_lcs_detects_deleted_inserted_and_reordered_words(self):
        self.assertEqual(model.lcs_similarity("a b c".split(), "a b c".split()), 1)
        self.assertEqual(model.tokens("Hello, WORLD!"), ["hello", "world"])
        self.assertIsNone(model.phrase_position(["trust"], ["rust"]))
        self.assertEqual(model.phrase_position(["a", "b", "a"], ["a"], 1), 2)
        self.assertIsNone(model.phrase_position(["a", "b"], ["a"], 1))
        self.assertLess(model.lcs_similarity("a b c".split(), "a c".split()), 1)
        self.assertLess(model.lcs_similarity("a b c".split(), "a b c d".split()), 1)
        self.assertLess(model.lcs_similarity("a b c".split(), "c b a".split()), 1)

    @unittest.skipUnless(shutil.which("ffmpeg"), "ffmpeg is optional for codec augmentation")
    def test_speed_and_codec_round_trips_keep_reference_and_rate(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            audio = root / "source.wav"
            prepare.write_audio(audio, [round(3000 * math.sin(i / 10)) for i in range(16000)])
            outputs = prepare.media_augmented([{"audio": str(audio), "text": "unchanged reference"}], root)
            self.assertEqual(len(outputs), 4)
            durations = {item["tags"][-1]: len(prepare.read_audio(item["audio"])) / prepare.RATE for item in outputs}
            self.assertGreater(durations["speed_0.9"], 1.05)
            self.assertLess(durations["speed_1.1"], .98)
            for codec in ("mp3", "opus"):
                # MP3 may retain a padded encoder frame at 16 kHz.
                self.assertAlmostEqual(durations[codec], 1, delta=.08)
            self.assertTrue(all(item["text"] == "unchanged reference" for item in outputs))

    def test_sweep_selection_is_deterministic_and_stratified(self):
        records = [{"audio": str(index), "text": "sample reference", "tags": [tag, "public"]}
                   for tag in ("ami", "librispeech_clean") for index in range(10)]
        selected = sweep.sample(records, 3)
        self.assertEqual(selected, sweep.sample(records, 3))
        self.assertEqual(len(selected), 6)
        self.assertEqual(sum("ami" in row["tags"] for row in selected), 3)
        choices = dict(sweep.configurations("NVIDIA,Rust"))
        self.assertEqual(choices["threads-2"], {"threads": 2})
        self.assertEqual(choices["boost-3.0"]["boost_score"], 3)

    def test_partial_download_resumes_only_a_verified_byte_range(self):
        class Response:
            status = 206
            headers = {"Content-Range": "bytes 3-5/6"}
            def __init__(self):
                import io
                self.body = io.BytesIO(b"def")
            def read(self, count):
                return self.body.read(count)
            def __enter__(self):
                return self
            def __exit__(self, *args):
                return False
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            partial = root / "raw" / "revision" / "clip.part"
            partial.parent.mkdir(parents=True)
            partial.write_bytes(b"abc")
            with patch.object(prepare.urllib.request, "urlopen", return_value=Response()) as request:
                path = prepare.fetch_shard(root, "revision", "clip.parquet")
            self.assertEqual(request.call_args.args[0].get_header("Range"), "bytes=3-")
            self.assertEqual(path.read_bytes(), b"abcdef")
            self.assertFalse(partial.exists())

    def test_interrupted_download_resumes_received_bytes_on_retry(self):
        class Response:
            def __init__(self, body, fail=False):
                import io
                self.body = io.BytesIO(body)
                self.status = 200 if fail else 206
                self.headers = {} if fail else {"Content-Range": "bytes 3-5/6"}
                self.fail = fail
            def read(self, count):
                data = self.body.read(count)
                if not data and self.fail:
                    raise TimeoutError("interrupted transfer")
                return data
            def __enter__(self):
                return self
            def __exit__(self, *args):
                return False
        with tempfile.TemporaryDirectory() as temporary:
            with patch.object(prepare.urllib.request, "urlopen", side_effect=[Response(b"abc", True), Response(b"def")]) as request, patch.object(prepare.time, "sleep"):
                path = prepare.fetch_shard(Path(temporary), "revision", "clip.parquet")
            self.assertEqual(path.read_bytes(), b"abcdef")
            self.assertEqual(request.call_args.args[0].get_header("Range"), "bytes=3-")

    def test_word_invariance_retains_failing_exact_format_diagnostics(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            audio = root / "speech.wav"
            prepare.write_audio(audio, [1000, -1000] * 8000)
            manifest = root / "manifest.jsonl"
            manifest.write_text(json.dumps({"audio": str(audio), "text": "hello"}) + "\n")
            evaluator = root / "evaluator"
            evaluator.write_text("#!" + sys.executable + "\n" + """import json, sys
from pathlib import Path
for line in Path(sys.argv[1]).read_text().splitlines():
    row = json.loads(line)
    name = Path(row['audio']).stem
    text = '' if name == 'silence' else ('Hello!' if name.endswith('trailing') else ('hello. hello.' if name.endswith('interior') else 'hello.'))
    print(json.dumps({'type': 'case', 'audio': row['audio'], 'raw_text': text}))
print(json.dumps({'type': 'summary', 'config': {}, 'provenance': {}}))
""")
            evaluator.chmod(0o700)
            report = root / "report.json"
            command = [sys.executable, str(Path(model.__file__)), str(manifest), "--evaluator", str(evaluator), "--report", str(report)]
            environment = dict(os.environ, HYPE_FLOW_MODEL_TEST="1")
            exact = subprocess.run(command, env=environment, capture_output=True, text=True)
            self.assertNotEqual(exact.returncode, 0)
            words = subprocess.run(command + ["--word-invariance"], env=environment, capture_output=True, text=True)
            self.assertEqual(words.returncode, 0, words.stderr)
            result = json.loads(report.read_text())
            self.assertTrue(result["passed"])
            self.assertEqual(result["gate"], "words")
            self.assertFalse(result["exact_format_passed"])
            self.assertEqual(len(result["exact_format_failures"]), 1)

    @unittest.skipUnless(Path("/proc/self/smaps_rollup").exists(), "Linux process accounting required")
    def test_process_tree_accounting_observes_current_process(self):
        memory = sweep.process_tree_memory(os.getpid())
        self.assertGreater(memory["pss_kib"], 0)
        self.assertGreaterEqual(memory["rss_kib"], memory["pss_kib"])
        environment = sweep.benchmark_environment()
        self.assertGreater(environment["logical_cpus"], 0)
        self.assertTrue(environment["platform"])

    def test_session_sweep_keeps_configs_local_and_original_models_unchanged(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            original = root / "original"
            original.mkdir()
            (original / "encoder.int8.onnx").write_bytes(b"model")
            config = root / "base.toml"
            config.write_text('model = "' + str(original) + '"\n')
            scratch = root / "scratch"
            scratch.mkdir()
            choices = sweep.session_configurations(SimpleNamespace(config=config, set=[]), scratch)
            self.assertEqual(len(choices), 3)
            for name, values in choices:
                path = Path(values["provider"].removeprefix("cpu:"))
                self.assertEqual(path.parent, Path(values["model"]))
                self.assertTrue(path.read_text().endswith("=0\n") or "disable_prepacking=1" in path.read_text())
            self.assertEqual(list(original.iterdir()), [original / "encoder.int8.onnx"])


if __name__ == "__main__":
    unittest.main()
