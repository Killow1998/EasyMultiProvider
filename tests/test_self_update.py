import hashlib
import io
import json
import os
import tarfile
import tempfile
import unittest
from pathlib import Path
from unittest.mock import Mock, patch

from easy_multi_provider.self_update import (
    REPO_URL, UpdateError, UpdateGate, asset_name, download_package,
    prepare_candidate, release_asset, run_update_worker, _ReleaseRedirect, _quiet_launch,
)


def release(name="EMP.exe"):
    return {"tag_name": "v0.9.9", "draft": False, "prerelease": False, "assets": [{
        "name": name, "digest": "sha256:" + hashlib.sha256(b"fixture").hexdigest(),
        "size": 7, "browser_download_url": REPO_URL + "/releases/download/v0.9.9/" + name,
    }]}


class SelfUpdateTests(unittest.TestCase):
    @unittest.skipUnless(os.name == "nt", "Windows error mode")
    def test_failed_launch_restores_calling_threads_windows_error_mode(self):
        import ctypes
        kernel = ctypes.WinDLL("kernel32")
        previous = kernel.GetThreadErrorMode()
        with self.assertRaises(RuntimeError):
            with _quiet_launch():
                self.assertEqual(kernel.GetThreadErrorMode() & 0x8003, 0x8003)
                raise RuntimeError("synthetic launch failure")
        self.assertEqual(kernel.GetThreadErrorMode(), previous)

    def test_stable_newer_release_selects_exact_platform_and_digest(self):
        result = release_asset(release(), "EMP.exe", "0.9.8")
        self.assertEqual(result["version"], "0.9.9")
        self.assertEqual(result["size"], 7)
        self.assertIsNone(release_asset(release(), "EMP.exe", "0.9.10"))
        self.assertEqual(asset_name("Darwin", "arm64"), "EMP-macos-arm64.dmg")
        self.assertEqual(asset_name("Darwin", "x86_64"), "EMP-macos-x86_64.dmg")
        self.assertEqual(asset_name("Linux", "x86_64"), "EMP-linux-x86_64.tar.gz")

    def test_untrusted_or_incomplete_metadata_cannot_install(self):
        for mutation in (
            lambda r: r.update(prerelease=True),
            lambda r: r.update(tag_name="v0.9.9-beta"),
            lambda r: r["assets"][0].update(digest=None),
            lambda r: r["assets"][0].update(browser_download_url="https://evil.example/EMP.exe"),
            lambda r: r["assets"].append(dict(r["assets"][0])),
            lambda r: r["assets"][0].update(size=0),
        ):
            fixture = release()
            mutation(fixture)
            with self.assertRaises(UpdateError):
                release_asset(fixture, "EMP.exe", "0.9.8")

    def test_download_checks_complete_size_and_sha256(self):
        asset = release_asset(release(), "EMP.exe", "0.9.8")
        with tempfile.TemporaryDirectory() as temporary:
            target = Path(temporary) / "package.exe"
            download_package(asset, target, opener=lambda url: io.BytesIO(b"fixture"))
            self.assertEqual(target.read_bytes(), b"fixture")
            target.unlink()
            for data in (b"wrong!!", b"short", b"fixture-extra"):
                with self.assertRaises(UpdateError):
                    download_package(asset, target, opener=lambda url: io.BytesIO(data))
                self.assertFalse(target.exists())

    def test_redirect_cannot_escape_https_github_assets(self):
        from urllib.request import Request
        with self.assertRaises(UpdateError):
            _ReleaseRedirect().redirect_request(Request(REPO_URL), None, 302, "", {}, "http://127.0.0.1/private")

    def test_drain_never_discards_running_requests_and_reopens_on_timeout(self):
        gate = UpdateGate()
        self.assertTrue(gate.enter())
        with self.assertRaises(UpdateError) as raised:
            gate.drain(timeout=0)
        self.assertEqual(str(raised.exception), "requests_busy")
        self.assertEqual(gate.active, 1)
        self.assertFalse(gate.draining)
        gate.leave()
        gate.drain(timeout=0)
        self.assertFalse(gate.enter())
        gate.reopen()
        self.assertTrue(gate.enter())
        gate.leave()

    def test_linux_extraction_reads_only_regular_emp_binary(self):
        with tempfile.TemporaryDirectory() as temporary:
            job = Path(temporary)
            package = job / "EMP-linux-x86_64.tar.gz"
            with tarfile.open(package, "w:gz") as archive:
                entry = tarfile.TarInfo("EMP/EMP")
                entry.size = 7
                archive.addfile(entry, io.BytesIO(b"fixture"))
                danger = tarfile.TarInfo("../../outside")
                danger.size = 7
                archive.addfile(danger, io.BytesIO(b"fixture"))
            candidate, binary = prepare_candidate(package, job, "")
            self.assertEqual(candidate, binary)
            self.assertEqual(binary.read_bytes(), b"fixture")
            self.assertFalse((job / "outside").exists())

    def test_update_worker_rolls_back_binary_when_replacement_cannot_start(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "EMP.exe"
            target.write_bytes(b"previous")
            job = root / ".emp-update-fixture"
            job.mkdir()
            candidate = job / "candidate.exe"
            candidate.write_bytes(b"new")
            plan = {"target": str(target), "candidate": str(candidate), "relative_binary": "",
                    "parents": [], "args": ["serve"], "version": "0.9.9", "nonce": "fixture"}
            (job / "plan.json").write_text(json.dumps(plan), encoding="utf-8")
            child = Mock()
            child.poll.return_value = 1
            child.pid = 999999999
            with patch("easy_multi_provider.self_update._spawn", return_value=child) as spawn:
                self.assertEqual(run_update_worker(job / "plan.json"), 1)
            self.assertEqual(target.read_bytes(), b"previous")
            self.assertEqual((job / "failed").read_bytes(), b"new")
            self.assertEqual(spawn.call_count, 2)
            self.assertEqual(spawn.call_args.kwargs['env']['EMP_UPDATE_RESULT'], 'rolled_back')

    def test_locked_installation_relaunches_old_binary_without_moving_it(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            target = root / "EMP.exe"
            target.write_bytes(b"previous")
            job = root / ".emp-update-locked"
            job.mkdir()
            candidate = job / "candidate.exe"
            candidate.write_bytes(b"new")
            (job / "plan.json").write_text(json.dumps({
                "target": str(target), "candidate": str(candidate), "relative_binary": "",
                "parents": [], "args": ["serve"], "version": "0.9.9", "nonce": "fixture",
            }), encoding="utf-8")
            with patch.object(Path, "rename", side_effect=PermissionError), patch("easy_multi_provider.self_update._spawn") as spawn:
                self.assertEqual(run_update_worker(job / "plan.json"), 1)
            self.assertEqual(target.read_bytes(), b"previous")
            self.assertEqual(candidate.read_bytes(), b"new")
            spawn.assert_called_once()
            self.assertEqual(spawn.call_args.args[0], [str(target), "serve"])


class UpdateEndpointTests(unittest.TestCase):
    def test_update_actions_require_management_auth_and_draining_keeps_health_available(self):
        from easy_multi_provider.config import normalize, save
        from easy_multi_provider.server import AppState
        from tests.test_diagnostic_journal_integration import running_server, request
        with tempfile.TemporaryDirectory() as temporary:
            config = Path(temporary) / "config.json"
            save(normalize({}), config)
            state = AppState(config)
            headers = {"Cookie": "emp_session=" + state.session_token, "Content-Type": "application/json"}
            with running_server(state) as server, patch.object(state.updater, "start", return_value={"state": "checking"}) as start:
                for method, path in (("GET", "/api/updates"), ("POST", "/api/updates/check"), ("POST", "/api/updates/install")):
                    status, _ = request(server, method, path, body=b"{}" if method == "POST" else None)
                    self.assertEqual(status, 401)
                start.assert_not_called()
                status, _ = request(server, "POST", "/api/updates/check", b"{}", headers)
                self.assertEqual(status, 202)
                start.assert_called_once_with("check")
                status, _ = request(server, "POST", "/api/updates/check", b"broken json", headers)
                self.assertEqual(status, 400)
                status, _ = request(server, "POST", "/api/updates/check", headers={**headers, "Content-Length": str(5 * 1024 * 1024 + 1)})
                self.assertEqual(status, 413)
                state.updater.gate.drain(timeout=0)
                status, _ = request(server, "POST", "/v1/responses", b"{}", headers)
                self.assertEqual(status, 503)
                status, _ = request(server, "GET", "/healthz")
                self.assertEqual(status, 200)
                status, _ = request(server, "GET", "/api/updates", headers=headers)
                self.assertEqual(status, 200)
                self.assertEqual(state.updater.gate.active, 0)


if __name__ == "__main__":
    unittest.main()
