#!/usr/bin/env python3
"""Validate release installation, desktop quoting and rollback in temporary roots."""
import os
from pathlib import Path
import subprocess
import tempfile
import time

import gi
gi.require_version("GioUnix", "2.0")
from gi.repository import Gio, GioUnix

REPO = Path(__file__).resolve().parents[1]

with tempfile.TemporaryDirectory(prefix="hype-flow-install-") as temporary:
    root = Path(temporary)
    prefix = root / "custom prefix ' $ ` % quote\""
    data = root / "separate data"
    launcher = prefix / "bin/hype-flow"
    launcher.parent.mkdir(parents=True)
    original = b"#!/bin/sh\nprintf 'previous launcher'\n"
    launcher.write_bytes(original)
    launcher.chmod(0o755)
    applications = data / "applications"
    applications.mkdir(parents=True)
    desktop = applications / "dev.hype.Flow.desktop"
    desktop.write_text("[Desktop Entry]\nType=Application\nName=Previous\nExec=/bin/true\n")
    previous_desktop = desktop.read_bytes()
    environment = {**os.environ, "HYPE_FLOW_PREFIX": str(prefix), "XDG_DATA_HOME": str(data),
                   "PKG_CONFIG_PATH": str(Path.home() / ".local/lib/pkgconfig")}
    subprocess.run(["sh", "tools/install.sh"], cwd=REPO, env=environment, check=True, capture_output=True)
    library = prefix / "share/hype-flow/lib/libgtk4-layer-shell.so.0"
    library_inode = library.stat().st_ino
    desktop_inode = desktop.stat().st_ino
    subprocess.run(["sh", "tools/install.sh"], cwd=REPO, env=environment, check=True, capture_output=True)
    assert library.stat().st_ino != library_inode, "Mapped libraries must be replaced atomically"
    assert desktop.stat().st_ino != desktop_inode, "Desktop entries must be replaced atomically"
    try:
        info = GioUnix.DesktopAppInfo.new_from_filename(str(desktop))
    except TypeError as error:
        raise AssertionError(desktop.read_text()) from error
    assert info is not None, desktop.read_text()
    runtime_env = {**environment, "PATH": "/usr/bin:/bin"}
    runtime_env.pop("LD_LIBRARY_PATH", None)
    help_text = subprocess.check_output([str(launcher), "--help"], env=runtime_env, timeout=10)
    assert b"--toggle" in help_text
    links = subprocess.check_output(["ldd", str(launcher)], env=runtime_env, text=True)
    private = str(prefix / "share/hype-flow/lib/libgtk4-layer-shell.so.0")
    linked = next(line.split("=>", 1)[1].rsplit(" (", 1)[0].strip() for line in links.splitlines() if "libgtk4-layer-shell.so.0 =>" in line)
    assert Path(linked).resolve() == Path(private).resolve(), links
    launcher.rename(launcher.with_name("hype-flow.real"))
    launcher.write_text('#!/bin/sh\nprintf "%s" "$0" > "$HYPE_FLOW_INSTALL_MARKER"\nexec "$0.real" --help >/dev/null\n')
    launcher.chmod(0o755)
    marker = root / "desktop-launch"
    context = Gio.AppLaunchContext()
    context.setenv("HYPE_FLOW_INSTALL_MARKER", str(marker))
    context.setenv("PATH", "/usr/bin:/bin")
    assert info.launch([], context)
    deadline = time.monotonic() + 5
    while not marker.exists() and time.monotonic() < deadline:
        time.sleep(0.01)
    assert marker.read_text() == str(launcher), desktop.read_text()
    rollback = data / "hype-flow/rollback"
    assert (rollback / "hype-flow").read_bytes() == original
    assert (rollback / "dev.hype.Flow.desktop").read_bytes() == previous_desktop
    assert not (data / "hype-flow/config.toml").exists()
    launcher.write_bytes((rollback / "hype-flow").read_bytes())
    assert subprocess.check_output([str(launcher)]) == b"previous launcher"
    print("Installation, absolute desktop path, private library, idempotence and rollback passed")
