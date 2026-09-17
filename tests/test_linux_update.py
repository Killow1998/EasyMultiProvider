"""Linux system-install detection and safe migration contracts."""

import hashlib
import io
import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from easy_multi_provider import linux_update as linux
from easy_multi_provider import self_update as update


class LinuxUpdateTests(unittest.TestCase):
    def test_package_ownership_is_read_only_and_exact(self):
        target = Path("/usr/bin/EMP")
        for owner, error in (
            (linux.PACKAGE, None),
            ("different-package", "unsupported_installation"),
        ):
            with self.subTest(owner=owner), patch.object(
                Path, "is_file", return_value=True
            ), patch.object(
                linux.subprocess,
                "run",
                side_effect=[
                    Mock(returncode=0, stdout=owner + ": " + str(target) + "\n"),
                    Mock(returncode=0, stdout="install ok installed\t0.11.2"),
                ],
            ):
                if error:
                    with self.assertRaisesRegex(update.UpdateError, error):
                        linux.package_version(target)
                else:
                    self.assertEqual(linux.package_version(target), "0.11.2")

        with patch.object(Path, "is_file", return_value=True), patch.object(
            linux.subprocess, "run", return_value=Mock(returncode=1, stdout="")
        ):
            self.assertIsNone(linux.package_version(target))

    def test_broken_package_query_fails_closed(self):
        with patch.object(Path, "is_file", return_value=True), patch.object(
            linux.subprocess, "run", return_value=Mock(returncode=2, stdout="")
        ):
            with self.assertRaisesRegex(update.UpdateError, "package_query_failed"):
                linux.package_version(Path("/usr/bin/EMP"))

    def test_frozen_library_path_is_not_given_to_package_query(self):
        with patch.dict(
            os.environ,
            {
                "LD_LIBRARY_PATH": "/tmp/_MEI/libs",
                "LD_LIBRARY_PATH_ORIG": "/usr/local/lib",
            },
        ):
            environment = linux.system_environment()
        self.assertEqual(environment["LD_LIBRARY_PATH"], "/usr/local/lib")
        self.assertNotIn("LD_LIBRARY_PATH_ORIG", environment)
        self.assertEqual(environment["LC_ALL"], "C")

    def test_system_install_check_offers_release_migration_not_background_install(self):
        name = "EMP-linux-x86_64.tar.gz"
        release = {
            "tag_name": "v9.0.0",
            "draft": False,
            "prerelease": False,
            "assets": [{
                "name": name,
                "digest": "sha256:" + hashlib.sha256(b"fixture").hexdigest(),
                "size": 7,
                "browser_download_url": update.REPO_URL + "/releases/download/v9.0.0/" + name,
            }],
        }
        manager = update.UpdateManager(
            Path("config.json"),
            executable=Path("/usr/bin/EMP"),
            opener=lambda _url: io.BytesIO(json.dumps(release).encode("utf-8")),
        )
        with patch.object(update.sys, "platform", "linux"), patch.object(
            update, "asset_name", return_value=name
        ), patch.object(linux, "package_version", return_value="0.11.2"):
            manager._check()
        snapshot = manager.snapshot()
        self.assertEqual(snapshot["state"], "migration_required")
        self.assertEqual(snapshot["manual_update"], "linux_system_migration")
        self.assertEqual(snapshot["release_url"], update.LATEST_RELEASE_URL)
        with self.assertRaisesRegex(update.UpdateError, "update_unavailable"):
            manager.start("install")

    def test_install_rechecks_system_ownership_before_download(self):
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "EMP"
            target.write_bytes(b"old")
            manager = update.UpdateManager(
                Path(temporary) / "config.json", executable=target
            )
            manager.asset = {"name": "EMP-linux-x86_64.tar.gz"}
            with patch.object(update.sys, "platform", "linux"), patch.object(
                linux, "package_version", return_value="0.11.2"
            ), patch.object(update, "download_package") as download:
                with self.assertRaisesRegex(update.UpdateError, "system_install_manual"):
                    manager._install()
            download.assert_not_called()

    def test_runtime_contains_no_privileged_install_path(self):
        source = Path(linux.__file__).read_text(encoding="utf-8")
        self.assertNotIn("pkexec", source)
        self.assertNotIn("/usr/bin/dpkg --install", source)


if __name__ == "__main__":
    unittest.main()
