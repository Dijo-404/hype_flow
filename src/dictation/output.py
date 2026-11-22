import json
import os
import re
import subprocess
import threading
import time


# Terminals paste with Ctrl+Shift+V. List from OpenWhispr's LINUX_TERMINAL_CLASSES.
TERMINALS = ("konsole", "gnome-terminal", "terminal", "kitty", "alacritty", "terminator", "xterm", "urxvt",
             "rxvt", "tilix", "terminology", "wezterm", "foot", "st", "yakuake", "ghostty", "guake", "tilda",
             "hyper", "tabby", "sakura", "warp", "termius", "waveterm", "ptyxis", "kgx", "org.gnome.console",
             "cosmicterm", "xst", "stterm")
RESTORE_SECONDS = 0.8  # OpenWhispr's Linux delay: long enough for the target to read the paste.
CLIPBOARD_LIMIT = 32 * 1024 * 1024
SENSITIVE = {**os.environ, "CLIPBOARD_STATE": "sensitive"}


def focused_window():
    try:
        result = subprocess.run(["niri", "msg", "--json", "focused-window"], capture_output=True, check=True, timeout=2)
        return json.loads(result.stdout)
    except (OSError, ValueError, subprocess.SubprocessError):
        return None


def is_terminal(app_id):
    # A short name like "st" must be a whole word, or it would match "steam".
    app_id = (app_id or "").lower()
    words = re.split(r"[^a-z0-9]+", app_id)
    return any(name in app_id if len(name) > 2 else name in words for name in TERMINALS)


def copy_text(text, primary=False):
    args = ["wl-copy", "--type", "text/plain;charset=utf-8"] + (["--primary"] if primary else [])
    subprocess.run(args, input=text.encode(), env=SENSITIVE, check=True, timeout=3)


def read_clipboard():
    """The clipboard's preferred content as (type, bytes), or None when empty or unreadable."""
    try:
        listed = subprocess.run(["wl-paste", "--list-types"], capture_output=True, timeout=1)
        offered = listed.stdout.decode(errors="replace").split() if listed.returncode == 0 else []
        if not offered:
            return None
        kind = next((name for name in offered if name.startswith("text/plain")), offered[0])
        data = subprocess.run(["wl-paste", "--no-newline", "--type", kind], capture_output=True, check=True, timeout=1).stdout
        return (kind, data) if len(data) <= CLIPBOARD_LIMIT else None
    except (OSError, subprocess.SubprocessError):
        return None


def restore_clipboard(previous, transcript):
    """Put the earlier clipboard back unless something new was copied after the transcript."""
    try:
        current = subprocess.run(["wl-paste", "--no-newline", "--type", "text"], capture_output=True, timeout=1)
        if current.returncode != 0 or current.stdout.decode(errors="replace") != transcript:
            return
        if previous is None:
            subprocess.run(["wl-copy", "--clear"], env=SENSITIVE, check=True, timeout=3)
        else:
            kind, data = previous
            # Marked sensitive so clipboard managers do not record the same item a second time.
            subprocess.run(["wl-copy", "--type", kind], input=data, env=SENSITIVE, check=True, timeout=3)
    except (OSError, subprocess.SubprocessError):
        pass  # The transcript stays on the clipboard, and Copy last transcript still recovers it.


def refocus(target):
    try:
        subprocess.run(["niri", "msg", "action", "focus-window", "--id", str(int(target["id"]))],
                       capture_output=True, check=True, timeout=2)
    except (OSError, ValueError, KeyError, subprocess.SubprocessError):
        return False
    time.sleep(0.1)
    current = focused_window()
    return bool(current) and current.get("id") == target["id"]


def deliver(text, target, paste, trailing_space=False, press_enter=False):
    if trailing_space and not text[-1:].isspace():
        text += " "  # Consecutive dictations stay separate words.
    previous = read_clipboard() if paste else None
    copy_text(text)
    if not paste:
        copy_text(text, primary=True)  # Shift+Insert also works for a manual paste.
        return "Copied"
    # Let the compositor release the shortcut's modifiers before synthesizing paste.
    time.sleep(0.18)
    current = focused_window()
    if not target or not current or target["id"] != current["id"]:
        # Return to the window dictation started in, as OpenWhispr's niri patch does.
        if not target or not refocus(target):
            copy_text(text, primary=True)
            return "Copied · focus changed"
    terminal = is_terminal(target.get("app_id"))
    args = ["wtype", "-M", "ctrl"]
    if terminal:
        args += ["-M", "shift"]
    args += ["-P", "v", "-p", "v"]
    if terminal:
        args += ["-m", "shift"]
    args += ["-m", "ctrl"]
    subprocess.run(args, check=True, timeout=3)
    submitted = press_enter and not terminal  # In a terminal, Enter would run dictated text as a command.
    if submitted:
        time.sleep(0.05)
        subprocess.run(["wtype", "-k", "Return"], check=True, timeout=3)
    restore = threading.Timer(RESTORE_SECONDS, restore_clipboard, (previous, text))
    restore.daemon = True
    restore.start()
    return "Pasted and sent" if submitted else "Pasted"
