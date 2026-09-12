"""Optional contract check against an official binary, with no user account or server."""
import os
from pathlib import Path
import subprocess
import tempfile
import time
import unittest

import psutil

from easy_multi_provider.codex_runtime import UnixSocketModelCatalogProbe


@unittest.skipUnless(os.environ.get("EMP_TEST_CODEX_BIN"), "official Codex test binary not configured")
class LiveCodexCatalogTests(unittest.TestCase):
    def test_existing_local_backend_reports_model_presentation(self):
        executable = Path(os.environ["EMP_TEST_CODEX_BIN"]).resolve(strict=True)
        with tempfile.TemporaryDirectory(prefix="emp-") as temporary:
            root = Path(temporary)
            environment = dict(os.environ, CODEX_HOME=str(root))
            for key in ("OPENAI_API_KEY", "CODEX_API_KEY"):
                environment.pop(key, None)
            with (root / "test-server.log").open("wb") as log:
                process = subprocess.Popen(
                    [str(executable), "app-server", "--listen", "unix://"],
                    cwd=root, env=environment, stdin=subprocess.DEVNULL,
                    stdout=log, stderr=log,
                    creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0,
                )
                try:
                    path = UnixSocketModelCatalogProbe._socket_path(root)
                    deadline = time.monotonic() + 15
                    while not path.exists() and process.poll() is None and time.monotonic() < deadline:
                        time.sleep(0.05)
                    self.assertIsNone(process.poll(), "isolated official backend exited during startup")
                    self.assertTrue(path.exists(), "isolated official control socket was not created")
                    models = UnixSocketModelCatalogProbe().model_list(root, 3)
                    self.assertTrue(models)
                    for model in models:
                        self.assertIsInstance(model["displayName"], str)
                        self.assertIsInstance(model["description"], str)
                finally:
                    try:
                        children = psutil.Process(process.pid).children(recursive=True)
                    except psutil.NoSuchProcess:
                        children = []
                    for child in children:
                        try:
                            child.terminate()
                        except psutil.NoSuchProcess:
                            pass
                    process.terminate()
                    try:
                        process.wait(5)
                    except subprocess.TimeoutExpired:
                        process.kill()
                        process.wait(5)
                    _, remaining = psutil.wait_procs(children, timeout=5)
                    for child in remaining:
                        child.kill()
                    psutil.wait_procs(remaining, timeout=5)
