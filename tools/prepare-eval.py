#!/usr/bin/env python3
# /// script
# requires-python = ">=3.11"
# dependencies = ["pyarrow", "soundfile", "numpy", "soxr"]
# ///
"""Prepare reproducible public, augmented, long-form and no-keyword evaluation sets.

Use ``uv run tools/prepare-eval.py public --out ~/.cache/hype-flow-eval`` for
public data. Other commands use only Python's standard library. Audio stays in
an external output directory; never commit personal speech or raw transcripts.
The media command additionally requires the ffmpeg executable.
"""

import argparse
import io
import json
import math
import random
import re
import shutil
import subprocess
import sys
import time
import urllib.error
import urllib.request
import wave
from array import array
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path

RATE = 16000
SEED = 20261015
REVISION = "de79e57adf9983993bcc72586d7313460825f5eb"
SETS = {
    "ami": ("ami/test", 15),
    "earnings22": ("earnings22/test", 5),
    "gigaspeech": ("gigaspeech/test", 19),
    "librispeech_clean": ("librispeech/test.clean", 1),
    "librispeech_other": ("librispeech/test.other", 1),
    "spgispeech": ("spgispeech/test", 38),
    "voxpopuli": ("voxpopuli/test", 4),
}


def read_audio(path):
    with wave.open(str(path), "rb") as source:
        if (source.getframerate(), source.getnchannels(), source.getsampwidth()) != (
            RATE,
            1,
            2,
        ):
            raise ValueError(f"Expected mono 16 kHz PCM16 WAV: {path}")
        samples = array("h", source.readframes(source.getnframes()))
        if sys.byteorder != "little":
            samples.byteswap()
        return samples


def write_audio(path, samples):
    samples = array("h", samples)
    if sys.byteorder != "little":
        samples.byteswap()
    path.parent.mkdir(parents=True, exist_ok=True)
    with wave.open(str(path), "wb") as output:
        output.setparams((1, 2, RATE, 0, "NONE", "not compressed"))
        output.writeframes(samples.tobytes())


def manifest(path):
    records = [
        json.loads(line) for line in path.read_text().splitlines() if line.strip()
    ]
    for record in records:
        record["audio"] = str((path.parent / record["audio"]).resolve())
    return records


def augmented(records, out):
    rng = random.Random(SEED)
    result = []
    for index, record in enumerate(records):
        samples = read_audio(record["audio"])
        rms = math.sqrt(sum(value * value for value in samples) / max(1, len(samples)))
        for snr in (None, 0, 5, 10, 15):
            # Measure actual generated noise RMS, so each requested SNR is reproducible.
            noise = [rng.gauss(0, 1) for _ in samples]
            noise_rms = math.sqrt(
                sum(value * value for value in noise) / max(1, len(noise))
            )
            scale = (
                0 if snr is None else rms / (10 ** (snr / 20) * max(noise_rms, 1e-12))
            )
            mixed = [sample + value * scale for sample, value in zip(samples, noise)]
            # Uniform scaling prevents clipping without changing SNR.
            peak = max([32767] + [abs(value) for value in mixed])
            mixed = [int(value * 32767 / peak) for value in mixed]
            leading, trailing = (
                rng.randrange(0, 5 * RATE + 1),
                rng.randrange(0, 5 * RATE + 1),
            )
            if len(mixed) + leading + trailing > 300 * RATE:
                continue
            path = out / "clips" / f"aug-{index}-{snr}.wav"
            write_audio(path, [0] * leading + mixed + [0] * trailing)
            result.append(
                {
                    **record,
                    "audio": str(path.resolve()),
                    "tags": record.get("tags", []) + ["augmented", f"snr_{snr}"],
                    "snr_db": snr,
                    "leading_silence_seconds": leading / RATE,
                    "trailing_silence_seconds": trailing / RATE,
                }
            )
    return result


def long_form(records, out):
    rng = random.Random(SEED)
    if not records:
        raise ValueError("Cannot stitch an empty manifest")
    usable = [(record, read_audio(record["audio"])) for record in records]
    usable = [
        (record, audio)
        for record, audio in usable
        if record["text"].strip() and 0 < len(audio) <= 60 * RATE
    ]
    if not usable:
        raise ValueError("Need at least one nonempty clip no longer than 60 seconds")
    result = []
    for target in (60, 120, 180):
        samples, texts, checkpoints = array("h"), [], []
        while len(samples) < target * RATE:
            record, audio = rng.choice(usable)
            checkpoints.append(
                {"start_seconds": len(samples) / RATE, "text": record["text"]}
            )
            samples.extend(audio)
            texts.append(record["text"])
            samples.extend([0] * rng.randrange(int(0.2 * RATE), RATE + 1))
        path = out / "clips" / f"long-{target}.wav"
        write_audio(path, samples)
        result.append(
            {
                "audio": str(path.resolve()),
                "text": " ".join(texts),
                "tags": ["long", f"target_{target}s"],
                "checkpoints": checkpoints,
            }
        )
    return result


def media_augmented(records, out):
    """Keep references unchanged while exercising speed and lossy audio paths."""
    ffmpeg = shutil.which("ffmpeg")
    if ffmpeg is None:
        raise ValueError(
            "Speed/codec augmentation requires ffmpeg with libmp3lame and libopus"
        )
    variants = [
        ("speed_0.9", ["-af", "atempo=0.9"]),
        ("speed_1.1", ["-af", "atempo=1.1"]),
    ]
    result = []
    for index, record in enumerate(records):
        for tag, filters in variants:
            path = out / "clips" / f"media-{index}-{tag}.wav"
            path.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                [
                    ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-y",
                    "-i",
                    record["audio"],
                    *filters,
                    "-ar",
                    str(RATE),
                    "-ac",
                    "1",
                    "-c:a",
                    "pcm_s16le",
                    str(path),
                ],
                check=True,
            )
            if len(read_audio(path)) > 300 * RATE:
                path.unlink()
                continue
            result.append(
                {
                    **record,
                    "audio": str(path.resolve()),
                    "tags": record.get("tags", []) + ["augmented", tag],
                }
            )
        for codec, encoder, bitrate in [
            ("mp3", "libmp3lame", "64k"),
            ("opus", "libopus", "32k"),
        ]:
            encoded = out / "clips" / f"media-{index}.{codec}"
            path = encoded.with_suffix(f".{codec}.wav")
            encoded.parent.mkdir(parents=True, exist_ok=True)
            subprocess.run(
                [
                    ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-y",
                    "-i",
                    record["audio"],
                    "-c:a",
                    encoder,
                    "-b:a",
                    bitrate,
                    str(encoded),
                ],
                check=True,
            )
            subprocess.run(
                [
                    ffmpeg,
                    "-nostdin",
                    "-v",
                    "error",
                    "-y",
                    "-i",
                    str(encoded),
                    "-ar",
                    str(RATE),
                    "-ac",
                    "1",
                    "-c:a",
                    "pcm_s16le",
                    str(path),
                ],
                check=True,
            )
            result.append(
                {
                    **record,
                    "audio": str(path.resolve()),
                    "tags": record.get("tags", []) + ["augmented", codec],
                }
            )
    return result


def fetch_shard(out, revision, relative):
    cache = out / "raw" / revision / relative
    cache.parent.mkdir(parents=True, exist_ok=True)
    if cache.exists():
        return cache
    temporary = cache.with_suffix(".part")
    url = (
        "https://huggingface.co/datasets/hf-audio/open-asr-leaderboard/resolve/"
        + revision
        + "/"
        + relative
    )
    for attempt in range(5):
        offset = temporary.stat().st_size if temporary.exists() else 0
        request = urllib.request.Request(
            url, headers={"Range": f"bytes={offset}-"} if offset else {}
        )
        try:
            with urllib.request.urlopen(request, timeout=90) as source:
                append = offset and source.status == 206
                if append and not source.headers.get("Content-Range", "").startswith(
                    f"bytes {offset}-"
                ):
                    raise ValueError(
                        "Dataset server returned an incorrect partial range"
                    )
                with temporary.open("ab" if append else "wb") as target:
                    shutil.copyfileobj(source, target, length=1024 * 1024)
                length = source.headers.get("Content-Length")
                expected = (offset if append else 0) + int(length) if length else None
                if expected is not None and temporary.stat().st_size != expected:
                    raise OSError(f"Incomplete dataset download: {relative}")
            temporary.replace(cache)
            break
        except (OSError, urllib.error.URLError):
            if attempt == 4:
                raise
            # Retain received bytes and resume instead of restarting a large shard.
            time.sleep(min(2**attempt, 8))
    return cache


def public(out, per_set, revision, download_workers=4):
    import numpy as np
    import pyarrow.parquet as pq
    import soundfile as sf
    import soxr

    rng = random.Random(SEED)
    result = []
    relatives = [
        f"{prefix}-{shard:05d}-of-{count:05d}.parquet"
        for prefix, count in SETS.values()
        for shard in dict.fromkeys((0, count // 2, count - 1))
    ]
    with ThreadPoolExecutor(max_workers=download_workers) as pool:
        # Downloads are independent; sampling remains sequential and deterministic.
        futures = [
            pool.submit(fetch_shard, out, revision, relative) for relative in relatives
        ]
        for relative, future in zip(relatives, futures):
            future.result()
            print(f"cached {relative}", file=sys.stderr, flush=True)
    for name, (prefix, count) in SETS.items():
        shards = list(dict.fromkeys((0, count // 2, count - 1)))
        tables = {}
        candidates = []
        for shard in shards:
            relative = f"{prefix}-{shard:05d}-of-{count:05d}.parquet"
            cache = fetch_shard(out, revision, relative)
            references = pq.read_table(cache, columns=["text"])
            candidates.extend(
                (shard, index)
                for index, text in enumerate(references.column("text").to_pylist())
                if text and text.strip()
            )
        rng.shuffle(candidates)
        prepared = 0
        for shard, row_index in candidates:
            if shard not in tables:
                relative = f"{prefix}-{shard:05d}-of-{count:05d}.parquet"
                tables[shard] = pq.read_table(
                    fetch_shard(out, revision, relative),
                    columns=["audio", "text", "id"],
                )
            row = tables[shard].slice(row_index, 1).to_pylist()[0]
            identifier = re.sub(r"[^a-zA-Z0-9_.-]", "_", str(row["id"]))
            path = out / "clips" / revision / f"{name}-{shard}-{identifier}.wav"
            if not path.exists():
                audio, rate = sf.read(
                    io.BytesIO(row["audio"]["bytes"]), dtype="float32", always_2d=True
                )
                audio = audio.mean(axis=1)
                if rate != RATE:
                    audio = soxr.resample(audio, rate, RATE)
                if len(audio) > 300 * RATE:
                    continue
                path.parent.mkdir(parents=True, exist_ok=True)
                sf.write(path, np.clip(audio, -1, 1), RATE, subtype="PCM_16")
            result.append(
                {
                    "audio": str(path.resolve()),
                    "text": row["text"],
                    "tags": [name, "public"],
                }
            )
            prepared += 1
            if prepared == per_set:
                break
        if prepared != per_set:
            raise ValueError(
                f"{name}: only {prepared} eligible clips, expected {per_set}"
            )
        print(f"{name}: prepared", file=sys.stderr)
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "kind", choices=("public", "augment", "media", "long", "control")
    )
    parser.add_argument("--manifest", type=Path)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--per-set", type=int, default=200)
    parser.add_argument("--download-workers", type=int, default=4)
    parser.add_argument(
        "--revision", default=REVISION, help="Immutable Hugging Face dataset commit"
    )
    parser.add_argument(
        "--keywords", default="", help="Comma-separated vocabulary for the control set"
    )
    args = parser.parse_args()
    if args.per_set < 1 or not 1 <= args.download_workers <= 16:
        parser.error(
            "--per-set must be positive; --download-workers must be between 1 and 16"
        )
    if args.kind != "public" and args.manifest is None:
        parser.error("--manifest is required except for public")
    if args.out.resolve().is_relative_to(Path(__file__).resolve().parents[1]):
        parser.error(
            "Keep evaluation audio outside the repository; choose an external --out path"
        )
    if not re.fullmatch(r"[0-9a-f]{40}", args.revision):
        parser.error("--revision must be an immutable 40-character commit SHA")
    args.out.mkdir(parents=True, exist_ok=True)
    if args.kind == "public":
        records = public(args.out, args.per_set, args.revision, args.download_workers)
    else:
        records = manifest(args.manifest)
        if args.kind == "augment":
            records = augmented(records, args.out)
        elif args.kind == "media":
            records = media_augmented(records, args.out)
        elif args.kind == "long":
            records = long_form(records, args.out)
        else:
            terms = [
                re.compile(
                    r"(?<!\w)" + re.escape(term.strip()) + r"(?!\w)", re.IGNORECASE
                )
                for term in args.keywords.split(",")
                if term.strip()
            ]
            records = [
                {**record, "tags": record.get("tags", []) + ["no_vocabulary"]}
                for record in records
                if not any(term.search(record["text"]) for term in terms)
            ]
    output = args.out / f"{args.kind}.jsonl"
    output.write_text(
        "".join(json.dumps(record, ensure_ascii=False) + "\n" for record in records)
    )
    (args.out / f"{args.kind}-metadata.json").write_text(
        json.dumps(
            {
                "seed": SEED,
                "kind": args.kind,
                "dataset_revision": args.revision if args.kind == "public" else None,
                "per_set": args.per_set if args.kind == "public" else None,
                "source_manifest": str(args.manifest.resolve())
                if args.manifest
                else None,
                "cases": len(records),
            },
            indent=2,
        )
        + "\n"
    )
    print(f"{len(records)} cases: {output}")


if __name__ == "__main__":
    main()
