#!/usr/bin/env python3
"""Run opt-in real-worker silence invariance, golden and checkpoint checks.

HYPE_FLOW_MODEL_TEST=1 python3 tools/check-model.py manifest.jsonl --evaluator ...
Use --golden to compare transcripts with references (short LCS 1.0, long 0.95).
Manifest entries may provide golden_text, keywords and checkpoints [{text: ...}].
"""

import argparse
import json
import os
import subprocess
import sys
import tempfile
import unicodedata
import wave
from pathlib import Path

RATE = 16000


def tokens(text):
    text = unicodedata.normalize("NFKC", text).casefold()
    return "".join(
        " " if unicodedata.category(char).startswith("P") else char for char in text
    ).split()


def phrase_position(text, phrase, start=0):
    for offset in range(start, len(text) - len(phrase) + 1):
        if text[offset : offset + len(phrase)] == phrase:
            return offset
    return None


def lcs_similarity(expected, actual):
    if not expected:
        return float(not actual)
    previous = [0] * (len(actual) + 1)
    for token in expected:
        current = [0]
        for index, found in enumerate(actual):
            current.append(
                previous[index] + 1
                if token == found
                else max(previous[index + 1], current[-1])
            )
        previous = current
    return previous[-1] / max(len(expected), len(actual), 1)


def write_audio(path, pcm):
    with wave.open(str(path), "wb") as output:
        output.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        output.writeframes(pcm)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--evaluator", default="target/release/hype-flow-evaluate")
    parser.add_argument("--config", type=Path, default=Path("docs/eval.toml"))
    parser.add_argument("--set", action="append", default=[])
    parser.add_argument("--golden", action="store_true")
    parser.add_argument(
        "--word-invariance",
        action="store_true",
        help="Gate word preservation; retain exact-format mismatches as diagnostics",
    )
    parser.add_argument(
        "--report",
        type=Path,
        help="Optional private report with raw transcript differences",
    )
    args = parser.parse_args()
    if os.environ.get("HYPE_FLOW_MODEL_TEST") != "1":
        parser.error("Real-model tests require HYPE_FLOW_MODEL_TEST=1")
    cases = [
        json.loads(line)
        for line in args.manifest.read_text().splitlines()
        if line.strip()
    ]
    if not cases:
        parser.error("Empty manifest")
    with tempfile.TemporaryDirectory(prefix="hype-flow-invariance-") as temporary:
        root = Path(temporary)
        records, originals = [], []
        for index, case in enumerate(cases):
            with wave.open(str(args.manifest.parent / case["audio"]), "rb") as source:
                if (
                    source.getframerate(),
                    source.getnchannels(),
                    source.getsampwidth(),
                ) != (RATE, 1, 2):
                    raise ValueError("Expected mono 16 kHz PCM16 WAV")
                pcm = source.readframes(source.getnframes())
            if not pcm or len(pcm) > 300 * RATE * 2:
                continue
            variants = {
                "base": pcm,
                "trailing": pcm + bytes(8 * RATE * 2),
                "click": pcm
                + bytes(5 * RATE * 2)
                + b"\x00\x40" * 320
                + bytes(RATE * 2),
                "leading": bytes(3 * RATE * 2) + pcm,
                "interior": pcm + bytes(5 * RATE * 2) + pcm,
            }
            for name, data in variants.items():
                if len(data) > 300 * RATE * 2:
                    continue
                path = root / f"{index}-{name}.wav"
                write_audio(path, data)
                records.append({"audio": str(path), "text": ""})
            originals.append((index, case, len(pcm) / (RATE * 2)))
        if not originals:
            raise ValueError("Need a nonempty clip no longer than 300 seconds")
        silence = root / "silence.wav"
        write_audio(silence, bytes(10 * RATE * 2))
        records.append({"audio": str(silence), "text": ""})
        manifest = root / "manifest.jsonl"
        manifest.write_text("".join(json.dumps(case) + "\n" for case in records))
        command = [args.evaluator, str(manifest), "--config", str(args.config)]
        for override in args.set:
            command.extend(["--set", override])
        output = subprocess.check_output(command, text=True)
        scored = [json.loads(line) for line in output.splitlines()]
        transcripts = {
            Path(item["audio"]).stem: item["raw_text"]
            for item in scored
            if item.get("type") == "case"
        }
        summary = next(item for item in scored if item.get("type") == "summary")
        failures, exact_format_failures, observations = [], [], []
        if transcripts["silence"]:
            failures.append("pure silence produced text")
        for index, case, seconds in originals:
            baseline = transcripts[f"{index}-base"]
            for name in ("trailing", "click", "leading", "interior"):
                if f"{index}-{name}" not in transcripts:
                    continue
                expected_text = (
                    baseline + " " + baseline if name == "interior" else baseline
                )
                actual_text = transcripts[f"{index}-{name}"]
                exact = (
                    actual_text.split() == expected_text.split()
                    if name == "interior"
                    else actual_text == expected_text
                )
                normalized = tokens(actual_text) == tokens(expected_text)
                observations.append(
                    {
                        "audio": case["audio"],
                        "variant": name,
                        "exact_match": exact,
                        "normalized_match": normalized,
                        "expected_text": expected_text,
                        "actual_text": actual_text,
                        "lcs_similarity": lcs_similarity(
                            tokens(expected_text), tokens(actual_text)
                        ),
                    }
                )
                if not exact:
                    kind = (
                        "case/punctuation only"
                        if normalized
                        else "normalized words differ"
                    )
                    detail = f"{case['audio']}: {name} changed transcript ({kind})"
                    exact_format_failures.append(detail)
                    if not args.word_invariance or not normalized:
                        failures.append(detail)
            expected = case.get("golden_text", case["text"] if args.golden else None)
            if expected is not None:
                similarity = lcs_similarity(tokens(expected), tokens(baseline))
                if similarity < (1.0 if seconds < 20 else 0.95):
                    failures.append(f"{case['audio']}: golden LCS {similarity:.4f}")
            actual_words = tokens(baseline)
            for phrase in case.get("keywords", []):
                if phrase_position(actual_words, tokens(phrase)) is None:
                    failures.append(f"{case['audio']}: missing keyword {phrase!r}")
            cursor = 0
            for point in case.get("checkpoints", []):
                phrase = point["text"]
                expected_words = tokens(phrase)
                position = phrase_position(actual_words, expected_words, cursor)
                if position is None:
                    failures.append(
                        f"{case['audio']}: missing or reordered checkpoint {phrase!r}"
                    )
                else:
                    cursor = position + len(expected_words)
        if args.report:
            args.report.parent.mkdir(parents=True, exist_ok=True)
            args.report.write_text(
                json.dumps(
                    {
                        "passed": not failures,
                        "gate": "words" if args.word_invariance else "exact",
                        "exact_format_passed": not exact_format_failures,
                        "exact_format_failures": exact_format_failures,
                        "clips": len(originals),
                        "failures": failures,
                        "observations": observations,
                        "provenance": summary.get("provenance"),
                        "config": summary.get("config"),
                    },
                    indent=2,
                )
                + "\n"
            )
            args.report.chmod(0o600)
        if failures:
            raise SystemExit("\n".join(failures))
        print(
            f"Passed {'word' if args.word_invariance else 'exact'} silence invariance and supplied golden/checkpoint checks for {len(originals)} clips"
        )
        if exact_format_failures:
            print(
                f"Exact formatting still differs in {len(exact_format_failures)} comparisons",
                file=sys.stderr,
            )


if __name__ == "__main__":
    main()
