"""Optional user post-processing: an executable reads the transcript on stdin and prints the replacement."""

import os
from pathlib import Path
import subprocess

TIMEOUT_SECONDS = 30


def hook_path():
    return Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "hype-flow/transcript-hook"


def run_hook(text):
    """Return (text, note). Any failure keeps the cleaned transcript and explains why in the note."""
    path = hook_path()
    if not path.exists():
        return text, None
    info = path.stat()
    # Trust boundary: only run a script the user owns that nobody else can modify.
    if info.st_uid != os.getuid() or info.st_mode & 0o022 or not os.access(path, os.X_OK):
        return text, "transcript hook skipped: it must be your own executable, not writable by others"
    try:
        result = subprocess.run([str(path)], input=text, capture_output=True, text=True, timeout=TIMEOUT_SECONDS)
    except subprocess.TimeoutExpired:
        return text, f"transcript hook timed out after {TIMEOUT_SECONDS} s"
    except OSError as error:
        return text, f"transcript hook failed: {error}"
    if result.returncode:
        return text, f"transcript hook exited with status {result.returncode}"
    return result.stdout.strip(), None
