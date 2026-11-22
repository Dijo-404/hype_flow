from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

from dictation.shortcuts import install


class ShortcutTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.home = Path(self.temporary.name)
        self.config_root = self.home / ".config"
        self.niri = self.config_root / "niri"
        self.niri.mkdir(parents=True)
        (self.niri / "config.kdl").write_text('binds { Mod+Shift+D { spawn "true"; }; }\n')
        launcher = self.home / ".local/bin/hype-flow"
        launcher.parent.mkdir(parents=True)
        launcher.write_text("#!/bin/sh\nexit 0\n")
        self.environment = patch.dict("os.environ", XDG_CONFIG_HOME=str(self.config_root), NIRI_CONFIG=str(self.niri / "config.kdl"))
        self.environment.start()
        self.home_patch = patch("dictation.shortcuts.Path.home", return_value=self.home)
        self.home_patch.start()

    def tearDown(self):
        self.home_patch.stop()
        self.environment.stop()
        self.temporary.cleanup()

    def test_validated_install_is_idempotent_and_keeps_existing_shortcuts(self):
        install()
        install()
        config = (self.niri / "config.kdl").read_text()
        self.assertEqual(config.count('include '), 1)
        self.assertIn('Mod+Shift+D', config)
        self.assertTrue(list(self.niri.glob('config.kdl.hype-flow-backup-*')))
        bindings = (self.niri / "hype-flow.kdl").read_text()
        self.assertIn("gapplication action dev.hype.Flow toggle 2>/dev/null || exec ", bindings)
        self.assertIn("--cancel", bindings)
        subprocess.run(["niri", "validate", "--config", str(self.niri / "config.kdl")], check=True, capture_output=True)

    def test_invalid_configuration_does_not_change_files(self):
        path = self.niri / "config.kdl"
        path.write_text("broken {\n")
        with self.assertRaisesRegex(RuntimeError, "niri rejected"):
            install()
        self.assertEqual(path.read_text(), "broken {\n")
        self.assertFalse((self.niri / "hype-flow.kdl").exists())

    def test_key_conflict_is_preserved(self):
        path = self.niri / "config.kdl"
        path.write_text('binds { Mod+Ctrl+Space { spawn "true"; }; }\n')
        with self.assertRaisesRegex(RuntimeError, "already bound"):
            install()
        self.assertFalse((self.niri / "hype-flow.kdl").exists())

    def test_conflict_in_included_configuration_is_preserved(self):
        path = self.niri / "config.kdl"
        path.write_text('include "keys.kdl"\n')
        (self.niri / "keys.kdl").write_text('binds { Mod+Ctrl+Space { spawn "true"; }; }\n')
        with self.assertRaisesRegex(RuntimeError, "keys.kdl"):
            install()
        self.assertFalse((self.niri / "hype-flow.kdl").exists())
