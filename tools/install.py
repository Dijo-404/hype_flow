#!/usr/bin/env python3
"""Install the app and launcher. Leave compositor settings to the user."""

import os
from pathlib import Path
import shutil


source = Path(__file__).resolve().parents[1]
data = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "hype-flow"
app = data / "app"
shutil.copytree(source / "src/dictation", app / "dictation", dirs_exist_ok=True,
                ignore=shutil.ignore_patterns("__pycache__"))
shutil.copytree(source / "docs", app / "docs", dirs_exist_ok=True)
launcher = Path.home() / ".local/bin/hype-flow"
launcher.parent.mkdir(parents=True, exist_ok=True)
launcher.write_text("#!/usr/bin/env python3\nimport sys\nsys.path.insert(0, " + repr(str(app)) + ")\nfrom dictation.app import main\nraise SystemExit(main())\n")
launcher.chmod(0o755)
icons = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "icons/hicolor/scalable/apps"
icons.mkdir(parents=True, exist_ok=True)
shutil.copyfile(source / "src/dictation/logo.svg", icons / "dev.hype.Flow.svg")
applications = Path(os.environ.get("XDG_DATA_HOME", Path.home() / ".local/share")) / "applications"
applications.mkdir(parents=True, exist_ok=True)
desktop = applications / "dev.hype.Flow.desktop"
escaped = str(launcher).replace("\\", "\\\\").replace('"', '\\"').replace("`", "\\`").replace("$", "\\$").replace("%", "%%")
desktop.write_text('[Desktop Entry]\nType=Application\nName=Hype Flow\nComment=Offline voice dictation for niri\nExec="' + escaped + '"\nIcon=dev.hype.Flow\nTerminal=false\nCategories=AudioVideo;Audio;\nStartupNotify=true\nStartupWMClass=dev.hype.Flow\n')
print(f"Installed {launcher}")
print(f"Installed {desktop}. Open Hype Flow from your app launcher.")
