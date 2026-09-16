"""Linux installation decisions, privilege boundary and recovery contracts."""
import json
import os
import shutil
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from easy_multi_provider import linux_update as linux
from easy_multi_provider import self_update as update


def fixture_plan(root):
    target = root / "system" / "EMP"
    target.parent.mkdir()
    target.write_bytes(b"old")
    job = root / ".emp-update-test"
    job.mkdir(mode=0o700)
    (job / "candidate").write_bytes(b"new")
    (job / "previous").write_bytes(b"old")
    plan = {"target": str(target), "candidate": str(job / "candidate"), "relative_binary": "",
            "parents": [], "args": ["serve"], "version": "9.0.0", "nonce": "test",
            "linux_install": {"kind": "binary", "uid": 0, "gid": 0,
                              "previous_digest": linux.file_digest(job / "previous"),
                              "candidate_digest": linux.file_digest(job / "candidate")}}
    (job / "plan.json").write_text(json.dumps(plan), encoding="utf-8")
    return target, job, plan


class LinuxUpdateTests(unittest.TestCase):
    def test_package_ownership_selects_package_not_directory_guess(self):
        target = Path("/usr/bin/EMP")
        for owner, error in ((linux.PACKAGE, None), ("different-package", "unsupported_installation")):
            with self.subTest(owner=owner), patch.object(Path, "is_file", return_value=True), patch.object(
                linux.subprocess, "run", side_effect=[
                    Mock(returncode=0, stdout=owner + ": " + str(target) + "\n"),
                    Mock(returncode=0, stdout="install ok installed\t0.11.1"),
                ]
            ):
                if error:
                    with self.assertRaisesRegex(update.UpdateError, error):
                        linux.package_version(target)
                else:
                    self.assertEqual(linux.package_version(target), "0.11.1")
        with patch.object(Path, "is_file", return_value=True), patch.object(
            linux.subprocess, "run", return_value=Mock(returncode=1, stdout="")
        ):
            self.assertIsNone(linux.package_version(target))

    def test_broken_package_query_cannot_silently_become_portable_update(self):
        with patch.object(Path, "is_file", return_value=True), patch.object(
            linux.subprocess, "run", return_value=Mock(returncode=2, stdout="")
        ):
            with self.assertRaisesRegex(update.UpdateError, "package_query_failed"):
                linux.package_version(Path("/usr/bin/EMP"))

    def test_frozen_library_path_is_not_given_to_system_tools(self):
        with patch.dict(os.environ, {"LD_LIBRARY_PATH": "/tmp/_MEI/libs", "LD_LIBRARY_PATH_ORIG": "/usr/local/lib"}):
            environment = linux.system_environment()
        self.assertEqual(environment["LD_LIBRARY_PATH"], "/usr/local/lib")
        self.assertNotIn("LD_LIBRARY_PATH_ORIG", environment)
        self.assertEqual(environment["LC_ALL"], "C")

    def test_os_authorization_cancellation_and_denial_do_not_modify_install(self):
        with tempfile.TemporaryDirectory() as temporary:
            target, job, plan = fixture_plan(Path(temporary))
            for code, error in ((126, "authorization_cancelled"), (127, "authorization_unavailable"),
                                (1, "system_install_failed")):
                with self.subTest(code=code), patch.object(linux.subprocess, "run", return_value=Mock(returncode=code)) as run:
                    with self.assertRaisesRegex(update.UpdateError, error):
                        linux.apply_install(plan, job)
                    self.assertEqual(target.read_bytes(), b"old")
                    command = run.call_args.args[0]
                    self.assertEqual(command[:3], ["/usr/bin/pkexec", "--disable-internal-agent", "/usr/bin/install"])
                    self.assertEqual(command[-2:], [str(job / "candidate"), str(target)])
                    self.assertEqual(run.call_args.kwargs["stdin"], subprocess.DEVNULL)
                    self.assertNotIn("shell", run.call_args.kwargs)

    def test_candidate_changed_after_download_never_requests_privileges(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, job, plan = fixture_plan(Path(temporary))
            (job / "candidate").write_bytes(b"tampered")
            with patch.object(linux.subprocess, "run") as run:
                with self.assertRaisesRegex(update.UpdateError, "checksum_mismatch"):
                    linux.apply_install(plan, job)
                run.assert_not_called()

    def test_installer_success_requires_matching_bytes_and_package_database(self):
        with tempfile.TemporaryDirectory() as temporary:
            target, job, plan = fixture_plan(Path(temporary))
            (job / linux.DEB_ASSET).write_bytes(b"deb")
            plan["linux_install"].update(kind="deb", package_digest=linux.file_digest(job / linux.DEB_ASSET))
            target.write_bytes(b"new")
            with patch.object(linux.subprocess, "run", return_value=Mock(returncode=0)) as run, patch.object(
                linux, "package_version", return_value="0.11.1"
            ):
                with self.assertRaisesRegex(update.UpdateError, "system_install_failed"):
                    linux.apply_install(plan, job)
                self.assertEqual(run.call_args.args[0][2:], ["/usr/bin/dpkg", "--install", str(job / linux.DEB_ASSET)])

    def test_cancelled_install_keeps_service_and_reopens_request_gate(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target, _, _ = fixture_plan(root)
            manager = update.UpdateManager(root / "config.json", executable=target)
            manager.shutdown = Mock()
            manager.asset = {"name": linux.DEB_ASSET, "version": "9.0.0", "sha256": "digest"}

            def candidate(package, job, version):
                path = job / "candidate"
                path.write_bytes(b"new")
                return path, path

            with patch.object(update.sys, "platform", "linux"), patch.object(linux, "package_version", return_value="0.11.1"), \
                 patch.object(update, "download_package"), patch.object(linux, "prepare_deb", side_effect=candidate), \
                 patch.object(update.subprocess, "run", return_value=Mock(returncode=0, stdout=b"EMP 9.0.0\n")), \
                 patch.object(linux, "prepare_rollback", return_value={"kind": "deb"}), \
                 patch.object(linux, "apply_install", side_effect=update.UpdateError("authorization_cancelled")) as install, \
                 patch.object(update, "_spawn") as spawn:
                manager._run(manager._install)
            self.assertEqual(manager.snapshot()["error"], "authorization_cancelled")
            self.assertFalse(manager.gate.draining)
            self.assertTrue(manager.gate.enter())
            manager.gate.leave()
            manager.shutdown.assert_not_called()
            spawn.assert_not_called()
            install.assert_called_once()
            self.assertEqual(target.read_bytes(), b"old")
            self.assertFalse(install.call_args.args[1].exists(), "cancelled job must be cleaned")

    def test_system_worker_restores_install_but_restarts_without_elevation(self):
        with tempfile.TemporaryDirectory() as temporary:
            target, job, _ = fixture_plan(Path(temporary))
            target.write_bytes(b"new")
            child = Mock(pid=999999999)
            child.poll.return_value = 1

            def restore(plan, job, *, rollback):
                self.assertTrue(rollback)
                shutil.copy2(job / "previous", target)

            with patch.object(update.sys, "platform", "linux"), patch.object(linux, "valid_user_job", return_value=True), \
                 patch.object(linux, "apply_install", side_effect=restore) as install, \
                 patch.object(update, "_spawn", return_value=child) as spawn:
                self.assertEqual(update.run_update_worker(job / "plan.json"), 1)
            self.assertEqual(target.read_bytes(), b"old")
            install.assert_called_once()
            self.assertEqual(spawn.call_count, 2)
            self.assertEqual(spawn.call_args.args[0], [str(target), "serve"])
            self.assertEqual(spawn.call_args.kwargs["env"]["EMP_UPDATE_RESULT"], "rolled_back")

    def test_failed_recovery_retains_files_and_does_not_start_broken_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, job, _ = fixture_plan(Path(temporary))
            child = Mock(pid=999999999)
            child.poll.return_value = 1
            with patch.object(update.sys, "platform", "linux"), patch.object(linux, "valid_user_job", return_value=True), \
                 patch.object(linux, "apply_install", side_effect=update.UpdateError("authorization_cancelled")), \
                 patch.object(update, "_spawn", return_value=child) as spawn:
                self.assertEqual(update.run_update_worker(job / "plan.json"), 1)
            spawn.assert_called_once()
            self.assertTrue((job / "previous").is_file())
            self.assertTrue((job / "recovery-required").is_file())

    def test_system_worker_cannot_use_public_staging(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, job, _ = fixture_plan(Path(temporary))
            with patch.object(update.sys, "platform", "linux"), patch.object(linux, "valid_user_job", return_value=False), \
                 patch.object(update, "_spawn") as spawn:
                with self.assertRaisesRegex(update.UpdateError, "invalid_update_plan"):
                    update.run_update_worker(job / "plan.json")
            spawn.assert_not_called()


@unittest.skipUnless(sys.platform.startswith("linux"), "requires Linux dpkg tools")
class NativeLinuxPackageTests(unittest.TestCase):
    def test_real_system_tools_upgrade_and_restore_in_isolated_user_namespace(self):
        if not shutil.which("unshare"):
            self.skipTest("unshare is unavailable")
        namespace = ["unshare", "--user", "--map-root-user"]
        probe = subprocess.run(namespace + ["/usr/bin/true"], capture_output=True)
        if probe.returncode:
            self.skipTest("the host disables unprivileged user namespaces")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            system = root / "system"
            (system / "var/lib/dpkg").mkdir(parents=True)
            for directory in ("info", "updates", "triggers", "parts", "alternatives"):
                (system / "var/lib/dpkg" / directory).mkdir()
            for name in ("status", "available"):
                (system / "var/lib/dpkg" / name).touch()
            packages = []
            for version, content in (("8.0.0", b"old"), ("9.0.0", b"new")):
                stage = root / version
                (stage / "DEBIAN").mkdir(parents=True)
                (stage / "usr/bin").mkdir(parents=True)
                (stage / "DEBIAN/control").write_text(
                    "Package: easy-multi-provider\nVersion: " + version + "\nArchitecture: amd64\n"
                    "Maintainer: Test <test@example.invalid>\nDescription: isolated fixture\n", encoding="utf-8")
                (stage / "usr/bin/EMP").write_bytes(content)
                (stage / "usr/bin/EMP").chmod(0o755)
                package = root / (version + ".deb")
                subprocess.run(["/usr/bin/dpkg-deb", "--build", "--root-owner-group", str(stage), str(package)],
                               check=True, capture_output=True)
                packages.append(package)
            for package, version, expected in ((packages[0], "8.0.0", b"old"), (packages[1], "9.0.0", b"new"),
                                               (packages[0], "8.0.0", b"old")):
                result = subprocess.run(namespace + ["/usr/bin/dpkg", "--root=" + str(system), "--log=" + str(root / "dpkg.log"), "--install", str(package)],
                                        capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)
                self.assertEqual((system / "usr/bin/EMP").read_bytes(), expected)
                status = subprocess.run(["/usr/bin/dpkg-query", "--admindir=" + str(system / "var/lib/dpkg"),
                                         "--show", "--showformat=${Status}\t${Version}", linux.PACKAGE],
                                        check=True, capture_output=True, text=True)
                self.assertEqual(status.stdout, "install ok installed\t" + version)
            # A portable replacement must not truncate a live executable's inode.
            target = root / "EMP"
            shutil.copy2("/usr/bin/sleep", target)
            previous = root / "previous"
            shutil.copy2(target, previous)
            child = subprocess.Popen([str(target), "30"], stdin=subprocess.DEVNULL)
            try:
                for source in (Path("/usr/bin/true"), previous):
                    subprocess.run(namespace + ["/usr/bin/install", "--no-target-directory", "--mode=755",
                                   "--owner=0", "--group=0", "--", str(source), str(target)],
                                   check=True, capture_output=True)
                    self.assertIsNone(child.poll())
                    self.assertEqual(linux.file_digest(target), linux.file_digest(source))
            finally:
                child.terminate()
                child.wait(timeout=5)

    def test_real_deb_probe_does_not_extract_links_or_run_maintainer_scripts(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            stage = root / "deb"
            (stage / "DEBIAN").mkdir(parents=True)
            (stage / "usr/bin").mkdir(parents=True)
            (stage / "DEBIAN/control").write_text(
                "Package: easy-multi-provider\nVersion: 9.0.0\nArchitecture: amd64\n"
                "Maintainer: Test <test@example.invalid>\nDescription: isolated fixture\n", encoding="utf-8")
            marker = root / "must-not-run"
            script = stage / "DEBIAN/postinst"
            script.write_text("#!/bin/sh\ntouch '" + str(marker) + "'\n", encoding="utf-8")
            script.chmod(0o755)
            (stage / "usr/bin/EMP").write_bytes(b"fixture executable")
            (stage / "usr/bin/escape").symlink_to(root / "outside")
            package = root / linux.DEB_ASSET
            subprocess.run(["/usr/bin/dpkg-deb", "--build", "--root-owner-group", str(stage), str(package)],
                           check=True, stdout=subprocess.DEVNULL)
            candidate, _ = linux.prepare_deb(package, root, "9.0.0")
            self.assertEqual(candidate.read_bytes(), b"fixture executable")
            self.assertFalse(marker.exists())
            self.assertFalse((root / "outside").exists())
            self.assertFalse((root / "usr").exists())
            self.assertFalse((root / "data.tar").exists())
            with self.assertRaisesRegex(update.UpdateError, "invalid_package"):
                linux.prepare_deb(package, root, "8.0.0")


if __name__ == "__main__":
    unittest.main()
