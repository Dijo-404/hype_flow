import os
import json
from pathlib import Path
import tomllib
import math

from .cleanup import DEFAULT_FILTERS, corrections
from .vocabulary import hotwords, terms


UNLOAD_CHOICES = (0, 5, 10, 15, 30, 60)  # 0 keeps the model loaded.


def data_dir():
    return Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "hype-flow"


def runtime_dir():
    base = os.environ.get("XDG_RUNTIME_DIR")
    if not base:
        raise RuntimeError("XDG_RUNTIME_DIR is unset. Run inside your Wayland session.")
    path = Path(base) / "hype-flow"
    path.mkdir(mode=0o700, exist_ok=True)
    if path.is_symlink() or path.stat().st_uid != os.getuid():
        raise RuntimeError("Unsafe runtime directory: " + str(path))
    path.chmod(0o700)
    return path


def load():
    root = data_dir()
    path = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "hype-flow/config.toml"
    values = {
        "model": str(root / "models/parakeet-unified-en-int8"),
        "server": str(root / "backend/parakeet/recognizer"),
        "language": "en",
        "prompt": "",
        "threads": min(4, os.cpu_count() or 4),
        "gpu": False,
        "paste": True,
        "input_target": "auto",
        "vocabulary_boosting": False,
        "boost_score": 1.5,
        "auto_correct": True,
        "corrections": "",
        "filter_words": True,
        "blocked_words": DEFAULT_FILTERS,
        "remove_repetitions": False,
        "fuzzy_vocabulary": False,
        "fuzzy_tolerance": 0.18,
        "sounds": True,
        "pause_media": True,
        "trailing_space": False,
        "press_enter": False,
        "keep_mic_ready": False,
        "unload_minutes": 10,
        "cut_silence": True,
        "noise_suppression": False,
    }
    if path.exists():
        supplied = tomllib.loads(path.read_text())
        unknown = supplied.keys() - values.keys()
        if unknown:
            raise ValueError("Unknown settings: " + ", ".join(sorted(unknown)))
        for key, value in supplied.items():
            if key in ("boost_score", "fuzzy_tolerance") and type(value) in (int, float):
                supplied[key] = float(value)
            elif type(value) is not type(values[key]):
                raise ValueError(f"Invalid type for {key}")
        legacy = {
            "model": str(root / "models/ggml-large-v3.bin"),
            "server": str(root / "backend/whisper-server-linux-x64-vulkan"),
        }
        # Migrate only previous defaults; preserve custom paths and saved vocabulary.
        for key, previous in legacy.items():
            if supplied.get(key) == previous:
                supplied.pop(key)
        values.update(supplied)
    validate(values)
    for key in ("model", "server"):
        values[key] = str(Path(values[key]).expanduser().resolve())
    return values


def validate(values):
    if type(values["threads"]) is not int or not 1 <= values["threads"] <= 64:
        raise ValueError("threads must be between 1 and 64")
    score = values["boost_score"]
    if type(score) not in (int, float) or not math.isfinite(score) or not 0.5 <= score <= 3:
        raise ValueError("Vocabulary strength must be between 0.5 and 3")
    tolerance = values["fuzzy_tolerance"]
    if type(tolerance) not in (int, float) or not math.isfinite(tolerance) or not 0.05 <= tolerance <= 0.4:
        raise ValueError("Near-miss tolerance must be between 0.05 and 0.4")
    if values["unload_minutes"] not in UNLOAD_CHOICES:
        raise ValueError("Unload after must be one of " + ", ".join(map(str, UNLOAD_CHOICES)) + " minutes")
    hotwords(values["prompt"])
    terms(values["blocked_words"])
    corrections(values["corrections"])


def save(values):
    validate(values)
    directory = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "hype-flow"
    directory.mkdir(parents=True, exist_ok=True)
    path = directory / "config.toml"
    if path.exists():
        previous = path.read_text()
        supplied = tomllib.loads(previous)
        if any(supplied.get(key) != values.get(key) for key in ("model", "server")):
            backup = directory / "config.toml.previous"
            backup.touch(mode=0o600, exist_ok=True)
            backup.chmod(0o600)
            backup.write_text(previous)
    temporary = directory / "config.toml.tmp"
    temporary.touch(mode=0o600, exist_ok=True)
    temporary.chmod(0o600)
    temporary.write_text("".join(f"{key} = {json.dumps(value, ensure_ascii=False)}\n" for key, value in values.items()))
    temporary.replace(path)
