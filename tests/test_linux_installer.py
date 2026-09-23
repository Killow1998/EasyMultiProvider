"""Exercise the public Linux installer without network or system changes."""

import hashlib
import json
import os
import select
import sys
import tarfile
import tempfile
import time
import unittest
from pathlib import Path

try:
    import pty
except ImportError:  # Windows package CI has no PTY module.
    pty = None


PROJECT_ROOT = Path(__file__).resolve().parents[1]
INSTALLER = PROJECT_ROOT / "packaging" / "install-linux.sh"


@unittest.skipUnless(sys.platform.startswith("linux") and os.getuid() != 0 and pty is not None,
                     "requires a Linux desktop user and PTY")
class LinuxInstallerTests(unittest.TestCase):
    def _run_installer(self, root: Path, answers: bytes, *, migrate: bool = False,
                       existing_config: bool = False, unsafe_entry: bool = False,
                       install_failure: bool = False):
        home = root / "home"
        home.mkdir()
        checkout = home / "EasyMultiProvider"
        checkout.mkdir()
        if migrate:
            (checkout / "config.json").write_text(
                json.dumps({"accounts": [], "providers": []}), encoding="utf-8"
            )
            (checkout / "state").mkdir()
            (checkout / "state" / "marker").write_text("retained", encoding="utf-8")
        if existing_config:
            current = home / ".config/easy-multi-provider"
            (current / "state").mkdir(parents=True)
            (current / "config.json").write_text('{"previous": true}', encoding="utf-8")
            (current / "state" / "marker").write_text("previous", encoding="utf-8")

        source = root / "bundle" / "EMP"
        source.mkdir(parents=True)
        (source / "EMP").write_text('#!/bin/sh\necho "EMP 0.11.8"\n', encoding="utf-8")
        (source / "install-user.sh").write_bytes(
            b"#!/bin/sh\nexit 1\n" if install_failure else
            (PROJECT_ROOT / "packaging" / "install-user.sh").read_bytes()
        )
        (source / "install-user.sh").chmod(0o755)
        (source / "easy-multi-provider.svg").write_text("<svg/>", encoding="utf-8")
        archive = root / "EMP-linux-x86_64.tar.gz"
        with tarfile.open(archive, "w:gz") as bundle:
            bundle.add(source, arcname="EMP")
            if unsafe_entry:
                link = tarfile.TarInfo("EMP/escape")
                link.type = tarfile.SYMTYPE
                link.linkname = "../../outside"
                bundle.addfile(link)
        digest = hashlib.sha256(archive.read_bytes()).hexdigest()
        release = root / "release.json"
        release.write_text(
            json.dumps({
                "tag_name": "v0.11.8", "draft": False, "prerelease": False,
                "assets": [{"name": archive.name, "digest": "sha256:" + digest,
                            "size": archive.stat().st_size}],
            }), encoding="utf-8"
        )
        fake_bin = root / "bin"
        fake_bin.mkdir()
        (fake_bin / "curl").write_text(
            '#!/bin/sh\n'
            'while [ "$#" -gt 0 ]; do\n'
            '  if [ "$1" = --output ]; then output=$2; shift 2; else shift; fi\n'
            'done\n'
            'case "$output" in */release.json) cp "$EMP_TEST_RELEASE" "$output";;'
            ' *) cp "$EMP_TEST_ARCHIVE" "$output";; esac\n', encoding="utf-8"
        )
        (fake_bin / "dpkg-query").write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        (fake_bin / "pgrep").write_text("#!/bin/sh\nexit 1\n", encoding="utf-8")
        for command in ("curl", "dpkg-query", "pgrep"):
            (fake_bin / command).chmod(0o755)

        environment = dict(os.environ, HOME=str(home), PWD=str(checkout),
                           XDG_CONFIG_HOME=str(home / ".config"),
                           XDG_DATA_HOME=str(home / ".local/share"),
                           PATH=str(fake_bin) + os.pathsep + os.environ["PATH"],
                           EMP_TEST_RELEASE=str(release), EMP_TEST_ARCHIVE=str(archive))
        pid, descriptor = pty.fork()
        if pid == 0:
            os.chdir(checkout)
            os.execve("/bin/sh", ("sh", str(INSTALLER)), environment)
        output = bytearray()
        deadline = time.monotonic() + 20
        try:
            os.write(descriptor, answers)
            while time.monotonic() < deadline:
                if not select.select([descriptor], [], [], 0.2)[0]:
                    continue
                try:
                    chunk = os.read(descriptor, 65536)
                except OSError:
                    break
                if not chunk:
                    break
                output.extend(chunk)
            else:
                os.kill(pid, 9)
                self.fail("Linux installer timed out")
        finally:
            os.close(descriptor)
        _, status = os.waitpid(pid, 0)
        return os.waitstatus_to_exitcode(status), output.decode("utf-8", "replace"), home, checkout

    def test_fresh_install_and_source_migration(self):
        with tempfile.TemporaryDirectory(prefix="emp-linux-installer-") as temporary:
            status, output, home, checkout = self._run_installer(
                Path(temporary), b"2\n1\ny\n", migrate=True
            )
            self.assertEqual(status, 0, output)
            self.assertIn("安裝完成", output)
            self.assertEqual(
                (home / ".local/share/easy-multi-provider/EMP").read_text(),
                '#!/bin/sh\necho "EMP 0.11.8"\n',
            )
            self.assertEqual(
                (home / ".config/easy-multi-provider/state/marker").read_text(),
                "retained",
            )
            self.assertTrue((checkout / "config.json").is_file())
            self.assertTrue((home / ".config/easy-multi-provider/config.json").is_file())

    def test_archive_symlink_is_rejected_before_install(self):
        with tempfile.TemporaryDirectory(prefix="emp-linux-installer-") as temporary:
            status, output, home, _ = self._run_installer(
                Path(temporary), b"1\n", unsafe_entry=True
            )
            self.assertNotEqual(status, 0)
            self.assertIn("unsafe entry", output)
            self.assertFalse((home / ".local/share/easy-multi-provider/EMP").exists())

    def test_migration_preserves_existing_user_configuration_in_backup(self):
        with tempfile.TemporaryDirectory(prefix="emp-linux-installer-") as temporary:
            status, output, home, _ = self._run_installer(
                Path(temporary), b"1\n1\ny\n", migrate=True, existing_config=True
            )
            self.assertEqual(status, 0, output)
            self.assertIn("backed up", output)
            current = home / ".config/easy-multi-provider"
            self.assertEqual((current / "state/marker").read_text(), "retained")
            backups = list((home / ".config").glob("easy-multi-provider-backup-*"))
            self.assertEqual(len(backups), 1)
            self.assertEqual((backups[0] / "config.json").read_text(), '{"previous": true}')
            self.assertEqual((backups[0] / "state/marker").read_text(), "previous")

    def test_failed_binary_install_does_not_change_existing_configuration(self):
        with tempfile.TemporaryDirectory(prefix="emp-linux-installer-") as temporary:
            status, _, home, _ = self._run_installer(
                Path(temporary), b"1\n1\ny\n", migrate=True,
                existing_config=True, install_failure=True
            )
            self.assertNotEqual(status, 0)
            current = home / ".config/easy-multi-provider"
            self.assertEqual((current / "config.json").read_text(), '{"previous": true}')
            self.assertEqual((current / "state/marker").read_text(), "previous")
            self.assertFalse(list((home / ".config").glob("easy-multi-provider-backup-*")))


if __name__ == "__main__":
    unittest.main()
