#!/usr/bin/env python3
"""Run pinned configurations and print a comparable summary table (never saves audio)."""
import argparse
import json
from pathlib import Path
import subprocess


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("manifest", type=Path)
    parser.add_argument("configs", nargs="+", type=Path)
    parser.add_argument("--evaluator", default="target/release/hype-flow-evaluate")
    parser.add_argument("--out", type=Path, default=Path("docs/eval-results"))
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--set", action="append", default=[])
    args = parser.parse_args()
    if len({path.stem for path in args.configs}) != len(args.configs):
        parser.error("configuration basenames must be unique")
    summaries = []
    for config in args.configs:
        output = args.out / (config.stem + ".json")
        command = [args.evaluator, str(args.manifest), "--config", str(config),
                   "--repeats", str(args.repeats), "--summary", str(output)]
        for override in args.set:
            command.extend(["--set", override])
        subprocess.run(command, check=True, stdout=subprocess.DEVNULL)
        summaries.append((config.stem, json.loads(output.read_text())))
    print("config\tstrict WER\tWhisper WER\tformatted WER\tkeyword F1\tp50 s\tp95 s\taudio/process\tpeak RSS KiB")
    for name, summary in summaries:
        metrics = summary["scores"]
        print("\t".join([name, *[f'{metrics[mode]["word_error_rate"]:.4f}' for mode in ("strict", "whisper", "formatted")],
                         f'{summary["keywords"]["f1"]:.4f}', f'{summary["p50_seconds"]:.3f}',
                         f'{summary["p95_seconds"]:.3f}', f'{summary["real_time_factor"]:.2f}',
                         str(summary["peak_worker_rss_kib"])]))


if __name__ == "__main__":
    main()
