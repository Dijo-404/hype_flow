"""The only code allowed to download anything; invoked explicitly by setup."""

import hashlib
import os
from pathlib import Path
import shutil
import subprocess
import tarfile
import tempfile
import time
import urllib.request

from .config import data_dir
from .vocabulary import sentencepiece_vocabulary


RELEASE = "1.13.8"
BACKEND_SHA256 = "d0f96c8b65c6cd0974fada22737e337de81bc8cd2abbec2e39caf358b1eec5fc"
MODEL_SHA256 = "99f63605b3a85a54c250c0869670a687b7d6598a47bf2421515e1f839a76e150"
MODEL_NAME = "parakeet-unified-en-int8"
MODEL_FILES = ("encoder.int8.onnx", "decoder.int8.onnx", "joiner.int8.onnx", "tokens.txt")
MODEL_BYTES = 663043117
NATIVE_VERSION = "3"
# Small audio models beside the speech model: Silero VAD for silence cutting, GTCRN for noise suppression.
AUDIO_MODELS = {
    "silero_vad.onnx": ("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/silero_vad.onnx",
                        "9e2449e1087496d8d4caba907f23e0bd3f78d91fa552479bb9c23ac09cbb1fd6"),
    "gtcrn_simple.onnx": ("https://github.com/k2-fsa/sherpa-onnx/releases/download/speech-enhancement-models/gtcrn_simple.onnx",
                          "e77603ac0c23dac3227dd2d7135b3a585cbee2679048aecfa886657d3ae1b534"),
}
TOKENIZER_REVISION = "7a16ff98b72e5b0beba6f182a5357c704d977b15"
TOKENIZER_SHA256 = "07d4e5a63840a53ab2d4d106d2874768143fb3fbdd47938b3910d2da05bfb0a9"


def assets_ready(values):
    model = Path(values["model"])
    backend = Path(values["server"])
    version = backend.with_name("recognizer.version")
    return (backend.is_file() and version.is_file() and version.read_text().strip() == NATIVE_VERSION
            and all((model / name).is_file() for name in (*MODEL_FILES, "bpe.vocab"))
            and all((model.parent / name).is_file() for name in AUDIO_MODELS))


def build_runtime(runtime):
    output = runtime / "recognizer.new"
    try:
        subprocess.run(["cc", "-std=c11", "-O2", "-Wall", "-Wextra", "-Werror",
                        str(Path(__file__).with_name("native") / "recognizer.c"),
                        "-I" + str(runtime / "include"), "-L" + str(runtime / "lib"),
                        "-lsherpa-onnx-c-api", "-Wl,-rpath,$ORIGIN/lib", "-o", str(output)], check=True, capture_output=True)
    except subprocess.CalledProcessError as error:
        raise RuntimeError("Could not build speech runtime: " + error.stderr.decode(errors="replace")[-1000:]) from error
    output.chmod(0o700)
    output.replace(runtime / "recognizer")
    (runtime / "recognizer.version").write_text(NATIVE_VERSION + "\n")


def install_vocabulary(model, root, progress=print):
    tokenizer = root / "downloads" / "parakeet-unified-tokenizer.model"
    download(f"https://huggingface.co/eschmidbauer/parakeet-unified-en-0.6b-onnx/resolve/{TOKENIZER_REVISION}/onnx_int8/tokenizer.model",
             tokenizer, TOKENIZER_SHA256, progress)
    vocabulary = sentencepiece_vocabulary(tokenizer.read_bytes(), model / "tokens.txt")
    temporary = model / "bpe.vocab.new"
    temporary.write_text(vocabulary)
    temporary.replace(model / "bpe.vocab")


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def download(url, path, expected, progress=print):
    if path.is_file() and digest(path) == expected:
        progress(f"Verified {path.name}")
        return
    path.parent.mkdir(parents=True, exist_ok=True)
    partial = path.with_suffix(path.suffix + ".part")
    offset = partial.stat().st_size if partial.exists() else 0
    headers = {"User-Agent": "hype-flow-setup/0.1"}
    if offset:
        headers["Range"] = f"bytes={offset}-"
    request = urllib.request.Request(url, headers=headers)
    progress(f"Downloading {path.name} (resuming at {offset // 1048576} MiB)")
    with urllib.request.urlopen(request, timeout=60) as source:
        append = source.status == 206 and source.headers.get("Content-Range", "").startswith(f"bytes {offset}-")
        done = offset if append else 0
        last_update = time.monotonic()
        with partial.open("ab" if append else "wb") as output:
            while chunk := source.read(1024 * 1024):
                output.write(chunk)
                done += len(chunk)
                if time.monotonic() - last_update > 5:
                    progress(f"Downloaded {done // 1048576} MiB")
                    last_update = time.monotonic()
    if digest(partial) != expected:
        partial.unlink()
        raise RuntimeError(f"SHA-256 mismatch for {path.name}; rejected download")
    partial.replace(path)
    progress(f"Verified {path.name}")


def setup(progress=print):
    if os.uname().machine != "x86_64":
        raise RuntimeError("The bundled speech runtime supports Linux x86_64")
    if not shutil.which("cc"):
        raise RuntimeError("A C compiler is required once for setup. Install base-devel, then retry.")
    root = data_dir()
    archive = root / "downloads" / f"sherpa-onnx-{RELEASE}.tar.bz2"
    download(f"https://github.com/k2-fsa/sherpa-onnx/releases/download/v{RELEASE}/sherpa-onnx-v{RELEASE}-linux-x64-shared-no-tts.tar.bz2", archive, BACKEND_SHA256, progress)
    model_archive = root / "downloads" / (MODEL_NAME + ".tar.bz2")
    download("https://github.com/k2-fsa/sherpa-onnx/releases/download/asr-models/sherpa-onnx-nemo-parakeet-unified-en-0.6b-int8-non-streaming.tar.bz2", model_archive, MODEL_SHA256, progress)
    destination = root / "backend" / "parakeet"
    destination.parent.mkdir(parents=True, exist_ok=True)
    models = root / "models"
    models.mkdir(exist_ok=True)
    for name, (url, checksum) in AUDIO_MODELS.items():
        download(url, models / name, checksum, progress)
    with tempfile.TemporaryDirectory(prefix=".parakeet-", dir=root) as temporary:
        stage = Path(temporary)
        progress("Installing speech runtime")
        with tarfile.open(archive) as bundle:
            selected = [entry for entry in bundle.getmembers() if "/lib/" in entry.name or entry.name.endswith("/c-api.h")]
            bundle.extractall(stage / "runtime", members=selected, filter="data")
        runtime = stage / "runtime" / f"sherpa-onnx-v{RELEASE}-linux-x64-shared-no-tts"
        build_runtime(runtime)
        progress("Installing Parakeet Unified English INT8")
        model = stage / "model"
        model.mkdir()
        with tarfile.open(model_archive) as bundle:
            for entry in bundle:
                name = Path(entry.name).name
                if entry.isfile() and name in MODEL_FILES:
                    with bundle.extractfile(entry) as source, (model / name).open("wb") as target:
                        shutil.copyfileobj(source, target)
        if not all((model / name).is_file() for name in MODEL_FILES):
            raise RuntimeError("Incomplete model archive; installation rejected")
        size = sum((model / name).stat().st_size for name in MODEL_FILES)
        if size != MODEL_BYTES or size >= 1_000_000_000:
            raise RuntimeError("Unexpected model size; installation rejected")
        install_vocabulary(model, root, progress)
        # Stage complete sets first; keep older models and a rollback copy of this runtime.
        for prepared, target in ((runtime, destination), (model, models / MODEL_NAME)):
            previous = target.with_name(target.name + ".previous")
            if target.exists():
                if previous.exists():
                    shutil.rmtree(previous)
                target.replace(previous)
            prepared.replace(target)
    progress("Speech model ready")
    return {"model": str(models / MODEL_NAME), "server": str(destination / "recognizer")}
