"""Native update smoke test, enabled after packaging with EMP_PACKAGE_UPDATE_SMOKE=1.

Only synthetic, isolated installations are replaced. No real Codex state or
upstream credentials are used; the published package format is extracted too.
"""
import hashlib
import json
import os
import shutil
import socket
import subprocess
import sys
import tempfile
import time
import unittest
from http.client import HTTPConnection
from pathlib import Path

import psutil

from easy_multi_provider import __version__
from easy_multi_provider.self_update import asset_name, prepare_candidate, _child_environment


def stop_test_processes(root):
    owned = []
    for process in psutil.process_iter(["exe"]):
        try:
            executable = process.info["exe"]
            if executable and Path(executable).resolve().is_relative_to(root.resolve()):
                owned.append(process)
        except (psutil.Error, OSError):
            pass
    for process in owned:
        try:
            process.terminate()
        except psutil.NoSuchProcess:
            pass
    _, alive = psutil.wait_procs(owned, timeout=5)
    for process in alive:
        try:
            process.kill()
        except psutil.NoSuchProcess:
            pass
    psutil.wait_procs(alive, timeout=5)


@unittest.skipUnless(os.environ.get("EMP_PACKAGE_UPDATE_SMOKE") == "1", "requires a native build")
class PackagedUpdateSmokeTests(unittest.TestCase):
    def run_scenario(self, fails_startup):
        artifacts = Path(__file__).resolve().parents[1] / "artifacts"
        name = asset_name()
        package = artifacts / name
        binary = artifacts / name.removesuffix(".tar.gz").removesuffix(".dmg")
        self.assertTrue(package.is_file(), "build the native package first")
        self.assertTrue(binary.is_file())
        with tempfile.TemporaryDirectory(prefix="emp-update-smoke-") as temporary:
            # macOS exposes its temporary directory through /var -> /private/var.
            # Use the physical path for the isolated vault; production correctly
            # rejects key paths containing symlink components.
            root = Path(temporary).resolve()
            job = root / ".emp-update-smoke"
            job.mkdir()
            relative = "Contents/Resources/EMP" if sys.platform == "darwin" else ""
            target = root / ("EMP.app" if relative else "EMP.exe" if os.name == "nt" else "EMP")
            installed = target / relative if relative else target
            if relative:
                # Exercise a real DMG-installed bundle, including its launcher,
                # Info.plist and icon, rather than a directory containing only
                # the Python bootloader executable.
                initial = root / "initial-package"
                initial.mkdir()
                initial_package = initial / name
                shutil.copy2(package, initial_package)
                initial_app, _ = prepare_candidate(initial_package, initial, relative)
                initial_app.rename(target)
                shutil.rmtree(initial)
            else:
                installed.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(binary, installed)
            bundle_files = {
                str(path.relative_to(target)): hashlib.sha256(path.read_bytes()).hexdigest()
                for path in target.rglob("*") if path.is_file()
            } if relative else {}
            original = hashlib.sha256(installed.read_bytes()).hexdigest()
            copied_package = job / name
            shutil.copy2(package, copied_package)
            candidate, candidate_binary = prepare_candidate(copied_package, job, relative)
            if fails_startup:
                candidate_binary.write_bytes(b"deliberately invalid executable")
            helper = job / ("worker.exe" if os.name == "nt" else "worker")
            shutil.copy2(binary, helper)
            with socket.socket() as probe:
                probe.bind(("127.0.0.1", 0))
                port = probe.getsockname()[1]
            codex_home = root / "codex-home"
            codex_home.mkdir()
            config = root / "config.json"
            config.write_text(json.dumps({"host": "127.0.0.1", "port": port}), encoding="utf-8")
            plan = {"target": str(target), "candidate": str(candidate), "relative_binary": relative,
                    "parents": [], "args": ["serve", "--config", str(config), "--port", str(port)],
                    "version": __version__, "nonce": "packaged-smoke"}
            plan_path = job / "plan.json"
            plan_path.write_text(json.dumps(plan), encoding="utf-8")
            environment = _child_environment()
            environment.update(CODEX_HOME=str(codex_home), EASY_MULTI_PROVIDER_CONFIG=str(config),
                               EASY_MULTI_PROVIDER_MASTER_KEY="", EASY_MULTI_PROVIDER_MASTER_KEY_FILE=str(root / "master.key"))
            output = root / "worker-output.txt"
            try:
                with output.open("wb") as log:
                    worker = subprocess.Popen([str(helper), "--emp-apply-update", str(plan_path)], env=environment,
                        cwd=root, stdout=log, stderr=subprocess.STDOUT,
                        creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
                    self.assertEqual(worker.wait(timeout=85), 1 if fails_startup else 0,
                                     "packaged update worker failed")
                deadline = time.monotonic() + 25
                while time.monotonic() < deadline:
                    connection = HTTPConnection("127.0.0.1", port, timeout=.5)
                    try:
                        connection.request("GET", "/healthz")
                        response = connection.getresponse()
                        if response.status == 200 and json.loads(response.read()) == {"status": "ok"}:
                            break
                    except (OSError, ValueError):
                        pass
                    finally:
                        connection.close()
                    time.sleep(.2)
                else:
                    self.fail("replacement/rollback service did not become healthy")
                self.assertEqual(hashlib.sha256(installed.read_bytes()).hexdigest(), original)
                if relative:
                    self.assertEqual({
                        str(path.relative_to(target)): hashlib.sha256(path.read_bytes()).hexdigest()
                        for path in target.rglob("*") if path.is_file()
                    }, bundle_files)
                    self.assertTrue(os.access(target / "Contents/MacOS/EMP", os.X_OK))
                    self.assertTrue(os.access(target / "Contents/Resources/launch.command", os.X_OK))
                if fails_startup:
                    self.assertTrue((job / "rolled-back").is_file())
                    logs = list((root / "state" / "logs").glob("*.jsonl"))
                    events = [json.loads(line) for path in logs for line in path.read_text(encoding="utf-8").splitlines()]
                    self.assertTrue(any(event.get("event") == "update_state" and event.get("fields", {}).get("result_class") == "install_rolled_back" for event in events))
                else:
                    deadline = time.monotonic() + 15
                    while job.exists() and time.monotonic() < deadline:
                        time.sleep(.2)
                    self.assertFalse(job.exists(), "successful update staging should be cleaned")
            finally:
                stop_test_processes(root)

    def test_packaged_replacement_starts_and_cleans_staging(self):
        self.run_scenario(False)

    def test_packaged_failed_startup_restores_previous_binary_and_service(self):
        self.run_scenario(True)
