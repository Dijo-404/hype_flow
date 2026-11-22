#!/usr/bin/env python3
"""Evaluate local recordings using the same persistent engine as dictation."""

import argparse
import json
from pathlib import Path
import sys
import tempfile
import time
import unicodedata

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "src"))
from dictation.backend import Engine, read_wav
from dictation.config import load, runtime_dir
from dictation.cleanup import process


def words(text):
    text = unicodedata.normalize("NFKC", text).casefold()
    text = "".join(char if unicodedata.category(char)[0] != "P" else " " for char in text)
    return text.split()


def edits(reference, hypothesis):
    previous = list(range(len(hypothesis) + 1))
    for row, expected in enumerate(reference, 1):
        current = [row]
        for column, actual in enumerate(hypothesis, 1):
            current.append(min(previous[column] + 1, current[-1] + 1,
                               previous[column - 1] + (expected != actual)))
        previous = current
    return previous[-1]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path, help='JSONL: {"audio": "relative.wav", "text": "expected words"}')
    parser.add_argument("--max-wer", type=float, default=None, help="Fail if corpus word error rate exceeds this fraction")
    parser.add_argument("--boosting", choices=("on", "off"), help="Override vocabulary boosting to compare its effect")
    parser.add_argument("--boost-score", type=float, help="Override the vocabulary boost score")
    args = parser.parse_args()
    cases = [json.loads(line) for line in args.manifest.read_text().splitlines() if line.strip()]
    if not cases:
        parser.error("manifest contains no recordings")
    errors = raw_errors = total_words = 0
    with tempfile.TemporaryDirectory(prefix="evaluation-", dir=runtime_dir()) as directory:
        values = load()
        if args.boosting:
            values["vocabulary_boosting"] = args.boosting == "on"
        if args.boost_score is not None:
            values["boost_score"] = args.boost_score
        engine = Engine(values, Path(directory))
        try:
            engine.start()
            for case in cases:
                pcm = read_wav(args.manifest.parent / case["audio"])
                start = time.monotonic()
                raw = engine.transcribe(pcm)
                transcript = process(raw, values)
                reference = words(case["text"])
                count = edits(reference, words(transcript))
                raw_count = edits(reference, words(raw))
                errors += count
                raw_errors += raw_count
                total_words += len(reference)
                print(json.dumps({"audio": case["audio"], "text": transcript, "raw_text": raw,
                                  "errors": count, "raw_errors": raw_count,
                                  "words": len(reference), "seconds": round(time.monotonic() - start, 2)}, ensure_ascii=False), flush=True)
        finally:
            engine.stop()
    wer = errors / max(1, total_words)
    print(json.dumps({"word_error_rate": wer, "raw_word_error_rate": raw_errors / max(1, total_words),
                      "errors": errors, "reference_words": total_words}))
    return int(args.max_wer is not None and wer > args.max_wer)


if __name__ == "__main__":
    raise SystemExit(main())
