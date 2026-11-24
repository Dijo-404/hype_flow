#!/usr/bin/env python3
"""Run reproducible one-variable sweeps and an opt-in hotword sanity check.

Audio, sampled manifests and per-case output remain outside the repository.
Only aggregate summaries are written to --out. No preference is changed.
"""
import argparse
from collections import Counter, defaultdict
import hashlib
import json
import os
import platform
from pathlib import Path
import random
import re
import subprocess
import tempfile
import time
import tomllib

SEED = 20261015


def sample(records, per_tag):
    groups = defaultdict(list)
    for record in records:
        tags = record.get("tags", [])
        name = next((tag for tag in tags if tag != "public"), "untagged")
        groups[name].append(record)
    rng = random.Random(SEED)
    return [record for name in sorted(groups) for record in rng.sample(groups[name], min(per_tag, len(groups[name])))]


def configurations(prompt):
    result = [("baseline", {})]
    for key, values in [("threads", [2, 3, 6, 8]), ("blank_penalty", [.5, 1., 2.]),
                        ("max_piece_seconds", [20, 30, 60]), ("vad_threshold", [.2, .4, .5]),
                        ("vad_min_silence", [.3, .6])]:
        result.extend((f"{key}-{value}", {key: value}) for value in values)
    result.extend([("vad-off", {"cut_silence": False}),
                   ("denoise-on", {"noise_suppression": True}),
                   ("vad-off-denoise-on", {"cut_silence": False, "noise_suppression": True}),
                   ("gain-on", {"normalize_gain": True}),
                   ("vocabulary-greedy", {"prompt": prompt}),
                   ("vocabulary-fuzzy", {"prompt": prompt, "fuzzy_vocabulary": True}),
                   ("vocabulary-fuzzy-confidence", {"prompt": prompt, "fuzzy_vocabulary": True, "confidence_filtering": True}),
                   ("anomaly-filter-on", {"hallucination_filtering": True}),
                   ("soft-speech-retry", {"soft_speech_retry": True}),
                   ("numbers-on", {"format_numbers": True})])
    result.extend((f"boost-{score}", {"prompt": prompt, "vocabulary_boosting": True, "boost_score": score})
                  for score in [.5, 1., 1.5, 2., 3.])
    result.append(("beam-paths-8", {"prompt": prompt, "vocabulary_boosting": True, "max_active_paths": 8}))
    return result


def keywords(records):
    # Fixed public-reference terms; this is not a personalized jargon experiment.
    counts = Counter(word for row in records for word in re.findall(r"\b[A-Za-z][A-Za-z'-]{5,}\b", row["text"]))
    return [term for term, _ in sorted(counts.items(), key=lambda item: (item[1], item[0]))[:20]]


def benchmark_environment():
    cpu = next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
                if line.startswith("model name")), "unknown")
    governor = Path("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor")
    return {"platform": platform.platform(), "cpu_model": cpu, "logical_cpus": os.cpu_count(),
            "cpu0_governor": governor.read_text().strip() if governor.exists() else None,
            "limitations": "Power policy is observed, not changed; ambient thermal and other desktop load are uncontrolled."}


def process_tree_memory(pid):
    pending, seen = [pid], set()
    total = {"rss_kib": 0, "pss_kib": 0}
    while pending:
        current = pending.pop()
        if current in seen:
            continue
        seen.add(current)
        directory = Path("/proc") / str(current)
        try:
            values = (directory / "smaps_rollup").read_text()
            for key, field in [("Rss:", "rss_kib"), ("Pss:", "pss_kib")]:
                total[field] += next(int(line.split()[1]) for line in values.splitlines() if line.startswith(key))
            for task in (directory / "task").iterdir():
                try:
                    pending.extend(int(child) for child in (task / "children").read_text().split())
                except (OSError, ValueError):
                    pass
        except (OSError, StopIteration):
            pass
    return total


def session_configurations(args, root):
    values = tomllib.loads(args.config.read_text())
    for setting in args.set:
        values.update(tomllib.loads(setting))
    default_data = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share"))
    if "model" in values:
        original = Path(values["model"]).expanduser().resolve()
    else:
        legacy = values.get("feature_dim") == 128 or values.get("vocabulary_boosting", False)
        variant = values.get("model_variant", "unified" if legacy else "parakeet-ctc")
        name = {"unified": "parakeet-unified-en-int8", "low-memory": "parakeet-110m-en-int8",
                "parakeet-ctc": "parakeet-110m-ctc-en-int8"}[variant]
        original = (default_data / "hype-flow/models" / name).resolve()
    model = root / original.name
    model.mkdir()
    for asset in original.iterdir():
        if asset.is_file():
            (model / asset.name).symlink_to(asset)
    for name in ("silero_vad.onnx", "gtcrn_simple.onnx"):
        if (original.parent / name).is_file():
            (root / name).symlink_to(original.parent / name)
    result = []
    # Pinned sherpa session.cc supports these keys without a patched runtime.
    for name, entry in [("ort-arena-off", "EnableCpuMemArena=0"),
                        ("ort-pattern-off", "EnableMemPattern=0"),
                        ("ort-prepacking-off", "SessionConfig.session.disable_prepacking=1")]:
        config = model / (name + ".cfg")
        config.write_text(entry + "\n")
        result.append((name, {"model": str(model), "provider": "cpu:" + str(config)}))
    return result


def run(args, manifest, name, settings):
    summary = args.out / (name + ".json")
    command = [args.evaluator, str(manifest), "--config", str(args.config), "--summary", str(summary),
               "--repeats", str(args.repeats)]
    for value in args.set:
        command.extend(["--set", value])
    for key, value in settings.items():
        command.extend(["--set", key + "=" + json.dumps(value)])
    print(name, flush=True)
    peak = {"rss_kib": 0, "pss_kib": 0}
    samples = 0
    with tempfile.TemporaryFile(mode="w+") as details:
        with subprocess.Popen(command, stdout=details) as process:
            while process.poll() is None:
                memory = process_tree_memory(process.pid)
                peak = {name: max(value, memory[name]) for name, value in peak.items()}
                samples += 1
                time.sleep(.1)
            if process.returncode:
                raise subprocess.CalledProcessError(process.returncode, command)
        details.seek(0)
        cases = [row for line in details if (row := json.loads(line)).get("type") == "case"]
    result = json.loads(summary.read_text())
    result["sampled_peak_process_tree_memory"] = {**peak, "samples": samples, "interval_seconds": .1,
        "window": "entire evaluator lifetime including loading and scoring",
        "limitations": "Sampled peaks may miss short transients; sampling overhead applies to timing"}
    terms = [term.strip() for term in result["config"]["prompt"].split(",") if term.strip()]
    controls = [row for row in cases if "no_vocabulary" in row.get("tags", [])]
    count = lambda text: sum(len(re.findall(r"(?<!\w)" + re.escape(term) + r"(?!\w)", text, re.I)) for term in terms)
    result["no_vocabulary_control"] = {"cases": len(controls),
        "keyword_false_insertions": sum(count(row["text"]) for row in controls),
        "raw_keyword_false_insertions": sum(count(row["raw_text"]) for row in controls)}
    summary.write_text(json.dumps(result, indent=2) + "\n")
    return result


def sanity(args, records, root):
    if os.environ.get("HYPE_FLOW_MODEL_TEST") != "1":
        raise ValueError("Ground-truth hotword sanity requires HYPE_FLOW_MODEL_TEST=1")
    errors = {"greedy": 0, "boosted": 0}
    words = 0
    for index, record in enumerate(records):
        terms = list(dict.fromkeys(re.findall(r"\b[A-Za-z][A-Za-z'-]*\b", record["text"])))
        if not terms or len(terms) > 30:
            continue
        path = root / "sanity.jsonl"
        path.write_text(json.dumps(record) + "\n")
        for label, setting in [("greedy", {"prompt": ",".join(terms)}),
                               ("boosted", {"prompt": ",".join(terms), "vocabulary_boosting": True, "boost_score": 1.5})]:
            result = run(args, path, f"sanity-{index}-{label}", setting)
            errors[label] += result["raw_scores"]["whisper"]["errors"]
            if label == "greedy":
                words += result["raw_scores"]["whisper"]["reference_words"]
            # Ground-truth words become prompts; retain only aggregate sanity results.
            (args.out / f"sanity-{index}-{label}.json").unlink()
    if words == 0:
        raise ValueError("Sanity corpus contains no clips with at most 30 reference terms")
    passed = errors["boosted"] < errors["greedy"] or errors["greedy"] == errors["boosted"] == 0
    report = {"type": "ground_truth_hotword_sanity", "errors": errors, "reference_words": words,
              "passed": passed, "zero_error_baseline": errors["greedy"] == 0,
              "requirement": "Boosting reduces aggregate raw Whisper errors, or preserves an already perfect baseline"}
    (args.out / "sanity-summary.json").write_text(json.dumps(report, indent=2) + "\n")
    return passed


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("--config", type=Path, default=Path("docs/eval.toml"))
    parser.add_argument("--evaluator", default="target/release/hype-flow-evaluate")
    parser.add_argument("--out", type=Path, default=Path("docs/eval-results/sweep"))
    parser.add_argument("--per-tag", type=int, default=5)
    parser.add_argument("--repeats", type=int, default=1)
    parser.add_argument("--set", action="append", default=[])
    parser.add_argument("--only", action="append", help="Run selected named configurations")
    parser.add_argument("--sanity", action="store_true")
    parser.add_argument("--ort", action="store_true", help="Also compare model-local CPU arena, pattern and prepacking configs")
    args = parser.parse_args()
    if args.per_tag < 1 or args.repeats < 1:
        parser.error("--per-tag and --repeats must be positive")
    contents = args.manifest.read_bytes()
    records = [json.loads(line) for line in contents.splitlines() if line.strip()]
    for record in records:
        record["audio"] = str((args.manifest.parent / record["audio"]).resolve())
    selected = sample(records, args.per_tag)
    if not selected:
        parser.error("Manifest contains no recordings")
    terms = keywords(selected)
    for record in selected:
        present = any(re.search(r"(?<!\w)" + re.escape(term) + r"(?!\w)", record["text"], re.I) for term in terms)
        record["tags"] = record.get("tags", []) + (["keyword_speech"] if present else ["no_vocabulary"])
    args.out.mkdir(parents=True, exist_ok=True)
    (args.out / "selection.json").write_text(json.dumps({"seed": SEED, "per_tag": args.per_tag,
        "cases": len(selected), "source_manifest_sha256": hashlib.sha256(contents).hexdigest(),
        "keywords": terms, "environment": benchmark_environment(), "limitations": "Deterministic public sample, not personal jargon; one-variable changes, no automatic default selection"}, indent=2) + "\n")
    with tempfile.TemporaryDirectory(prefix="hype-flow-sweep-") as temporary:
        root = Path(temporary)
        manifest = root / "sample.jsonl"
        manifest.write_text("".join(json.dumps(record) + "\n" for record in selected))
        if args.sanity:
            if not sanity(args, selected, root):
                raise SystemExit("Ground-truth boosting did not improve raw WER; see sanity-summary.json")
            return
        rows = []
        choices = configurations(",".join(terms))
        if args.ort:
            choices.extend(session_configurations(args, root))
        for name, settings in choices:
            if args.only and name not in args.only:
                continue
            summary = run(args, manifest, name, settings)
            rows.append({"name": name, "whisper_wer": summary["scores"]["whisper"]["word_error_rate"],
                         "keyword_f1": summary["keywords"]["f1"], "p50_seconds": summary["p50_seconds"],
                         "p95_seconds": summary["p95_seconds"], "peak_worker_rss_kib": summary["peak_worker_rss_kib"],
                         "sampled_peak_process_tree_memory": summary["sampled_peak_process_tree_memory"],
                         "no_vocabulary_control": summary["no_vocabulary_control"],
                         "by_tag": summary["by_tag"]})
        if not rows:
            parser.error("No matching configurations")
        (args.out / "comparison.json").write_text(json.dumps(rows, indent=2) + "\n")
        if any(row["name"] == "vocabulary-greedy" for row in rows):
            greedy = json.loads((args.out / "vocabulary-greedy.json").read_text())
            decisions = []
            for row in rows:
                if not row["name"].startswith("boost-"):
                    continue
                boosted = json.loads((args.out / (row["name"] + ".json")).read_text())
                enough = bool(greedy["keywords"]["true_positives"] + greedy["keywords"]["false_negatives"]) and bool(greedy["no_vocabulary_control"]["cases"])
                control_errors = lambda summary: summary["by_tag"].get("no_vocabulary", {}).get("whisper", {}).get("errors", 0)
                safer = (control_errors(boosted) <= control_errors(greedy)
                         and boosted["no_vocabulary_control"]["raw_keyword_false_insertions"] <= greedy["no_vocabulary_control"]["raw_keyword_false_insertions"])
                decisions.append({"name": row["name"], "sufficient_groups": enough,
                    "control_did_not_regress": safer,
                    "keyword_f1_improved": boosted["keywords"]["f1"] > greedy["keywords"]["f1"],
                    "passes_guard": enough and safer and boosted["keywords"]["f1"] > greedy["keywords"]["f1"]})
            (args.out / "boosting-guards.json").write_text(json.dumps({"decisions": decisions,
                "limitations": "Exploratory public sample; personalized jargon must be tested before choosing a production boost score"}, indent=2) + "\n")


if __name__ == "__main__":
    main()
