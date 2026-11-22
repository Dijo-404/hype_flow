"""One explicit settings action installs validated, reversible compositor bindings."""

import json
import os
from pathlib import Path
import re
import shlex
import shutil
import subprocess
import tempfile
import time


HEADER = "// Managed by Hype Flow.\n"


def install():
    root = Path(os.environ.get("XDG_CONFIG_HOME", Path.home() / ".config")) / "niri"
    config = Path(os.environ.get("NIRI_CONFIG", root / "config.kdl"))
    launcher = Path.home() / ".local/bin/hype-flow"
    if not launcher.is_file():
        raise RuntimeError("Install Hype Flow before enabling desktop shortcuts")
    original = config.read_text()
    target = config.parent / "hype-flow.kdl"
    if target.exists() and not target.read_text().startswith(HEADER):
        raise RuntimeError("hype-flow.kdl already exists and belongs to another configuration")
    include = "include " + json.dumps(str(target))
    visited = set()

    def check_conflicts(path):
        resolved = path.resolve()
        if resolved in visited or resolved == target.resolve():
            return
        visited.add(resolved)
        content = path.read_text()
        for key in ("Mod+Ctrl+Space", "Mod+Ctrl+Escape"):
            if re.search(re.escape(key) + r"\s", content):
                raise RuntimeError(f"{key} is already bound in {path.name}. Existing shortcuts were kept.")
        for match in re.finditer(r'^\s*include\s+("(?:[^"\\]|\\.)*")', content, re.MULTILINE):
            check_conflicts(path.parent / json.loads(match.group(1)))

    check_conflicts(config)
    executable = json.dumps(str(launcher))

    def command(action):
        # Signal the running app over D-Bus (~3 ms); start it only when it is not running.
        script = f"gapplication action dev.hype.Flow {action} 2>/dev/null || exec {shlex.quote(str(launcher))} --{action}"
        return "spawn \"sh\" \"-c\" " + json.dumps(script)

    bindings = HEADER + f'''spawn-at-startup {executable} "--background"
binds {{
    Mod+Ctrl+Space repeat=false hotkey-overlay-title="Dictate offline" {{ {command("toggle")}; }}
    Mod+Ctrl+Escape repeat=false hotkey-overlay-title="Cancel dictation" {{ {command("cancel")}; }}
}}
window-rule {{
    match app-id="^dev[.]hype[.]Flow$"
    open-floating true
    default-column-width {{ fixed 960; }}
    default-window-height {{ fixed 680; }}
}}
'''
    with tempfile.TemporaryDirectory(prefix=".hype-flow-", dir=config.parent) as temporary:
        temporary = Path(temporary)
        generated = temporary / "bindings.kdl"
        generated.write_text(bindings)
        # Keep validation beside the real config so its relative includes resolve identically.
        candidate = config.parent / f".hype-flow-check-{os.getpid()}.kdl"
        checked = original.replace(include, "include " + json.dumps(str(generated))) if include in original else original + "\ninclude " + json.dumps(str(generated)) + "\n"
        candidate.write_text(checked)
        try:
            result = subprocess.run(["niri", "validate", "--config", str(candidate)], capture_output=True, text=True, timeout=10)
            if result.returncode:
                raise RuntimeError("niri rejected the shortcut configuration: " + (result.stderr or result.stdout)[-700:])
        finally:
            candidate.unlink(missing_ok=True)
        backup = config.with_name(config.name + f".hype-flow-backup-{time.time_ns()}")
        shutil.copy2(config, backup)
        generated.replace(target)
        if include not in original:
            updated = temporary / "config.kdl"
            updated.write_text(original + "\n" + include + "\n")
            updated.chmod(config.stat().st_mode & 0o777)
            updated.replace(config)
    return "Shortcuts ready. Super + Ctrl + Space starts dictation. A backup of your niri config was saved."
