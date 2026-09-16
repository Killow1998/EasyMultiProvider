"""Exercise the Linux archive installer in an isolated desktop-user home."""
import os
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.main import resolve_desktop_config_path
from easy_multi_provider.self_update import installation_target, prepare_candidate


@unittest.skipUnless(sys.platform.startswith("linux") and os.getuid() != 0, "requires a Linux desktop user")
class UserInstallTests(unittest.TestCase):
    def test_installed_launcher_preserves_arguments_config_and_update_target(self):
        import tarfile

        with tempfile.TemporaryDirectory(prefix="emp-user-install-") as temporary:
            root = Path(temporary)
            home = root / "desktop user's home $100%"
            source = root / "archive"
            source.mkdir()
            script = Path(__file__).resolve().parents[1] / "packaging" / "install-user.sh"
            (source / "install-user.sh").write_bytes(script.read_bytes().replace(b"\r\n", b"\n"))
            (source / "EMP").write_text('#!/bin/sh\nprintf "%s\\n" "$@"\n', encoding="utf-8")
            (source / "easy-multi-provider.svg").write_text("<svg/>")
            for custom_xdg in (False, True):
                with self.subTest(custom_xdg=custom_xdg):
                    active_home = home / str(custom_xdg)
                    environment = dict(os.environ, HOME=str(active_home))
                    environment.pop("XDG_CONFIG_HOME", None)
                    environment.pop("XDG_DATA_HOME", None)
                    if custom_xdg:
                        environment.update(XDG_CONFIG_HOME=str(active_home / "settings"), XDG_DATA_HOME=str(active_home / "data"))
                    config = resolve_desktop_config_path(environ=environment, user_home=active_home)
                    config.parent.mkdir(parents=True)
                    config.write_bytes(b"existing configuration")
                    run = subprocess.run(["sh", str(source / "install-user.sh")], env=environment, capture_output=True, text=True)
                    self.assertEqual(run.returncode, 0, run.stderr)
                    self.assertEqual(config.read_bytes(), b"existing configuration")
                    launcher = active_home / ".local/bin/EMP"
                    result = subprocess.run([str(launcher), "--version", "argument with spaces", "$(false)"], capture_output=True, text=True)
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stdout.splitlines(), ["--version", "argument with spaces", "$(false)"])
                    data = Path(environment.get("XDG_DATA_HOME", active_home / ".local/share"))
                    binary = data / "easy-multi-provider/EMP"
                    self.assertEqual(installation_target(binary), (binary, ""))
                    job = root / ("update-" + str(custom_xdg))
                    job.mkdir()
                    archive = job / "EMP-linux-x86_64.tar.gz"
                    with tarfile.open(archive, "w:gz") as bundle:
                        bundle.add(binary, arcname="EMP/EMP")
                    _, candidate = prepare_candidate(archive, job, "")
                    self.assertEqual(candidate.read_bytes(), binary.read_bytes())
                    second = subprocess.run(["sh", str(source / "install-user.sh")], env=environment, capture_output=True, text=True)
                    self.assertNotEqual(second.returncode, 0)
                    self.assertEqual(config.read_bytes(), b"existing configuration")
                    self.assertEqual(binary.read_bytes(), candidate.read_bytes())
