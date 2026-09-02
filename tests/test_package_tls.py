import importlib.util
import json
import os
import ssl
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch


PROJECT_ROOT = Path(__file__).resolve().parents[1]


def _load(name, filename):
    spec = importlib.util.spec_from_file_location(name, PROJECT_ROOT / "packaging" / filename)
    module = importlib.util.module_from_spec(spec)
    sys.modules[name] = module
    spec.loader.exec_module(module)
    return module


builder = _load("emp_build_tls_test", "build.py")
windows_tls = _load("emp_windows_tls_test", "windows_tls.py")


class PackageTlsTests(unittest.TestCase):
    def _probe(self, report):
        result = subprocess.CompletedProcess([], 0, json.dumps(report), "")
        target = builder.Target("Windows", "windows", "x86_64", ".exe")
        with patch.object(builder.subprocess, "run", return_value=result):
            builder._smoke_tls(Path("test.exe"), target)

    def test_frozen_tls_must_match_build_runtime_and_keep_verification(self):
        healthy = {
            "openssl": ssl.OPENSSL_VERSION,
            "verify_required": True,
            "check_hostname": True,
            "certificates": 1,
        }
        self._probe(healthy)
        for field, value in (
            ("openssl", "foreign OpenSSL"),
            ("verify_required", False),
            ("check_hostname", False),
            ("certificates", 0),
        ):
            with self.subTest(field=field), self.assertRaises(RuntimeError):
                self._probe(dict(healthy, **{field: value}))

    def test_frozen_trust_store_failure_blocks_packaging(self):
        with patch.object(
            builder.subprocess, "run", side_effect=subprocess.CalledProcessError(1, [])
        ), self.assertRaises(subprocess.CalledProcessError):
            builder._smoke_tls(Path("test.exe"), builder.current_target())

    def test_other_platforms_keep_normal_dependency_collection(self):
        with patch.object(windows_tls.sys, "platform", "linux"):
            self.assertEqual(windows_tls.collect_windows_tls_binaries(), [])

    @unittest.skipUnless(
        sys.platform == "win32" and importlib.util.find_spec("PyInstaller"),
        "Windows packaging dependencies required",
    )
    def test_foreign_dlls_on_path_cannot_replace_loaded_tls_libraries(self):
        originals = windows_tls.collect_windows_tls_binaries()
        if not originals:
            self.skipTest("Python does not dynamically link OpenSSL")
        with tempfile.TemporaryDirectory() as temporary:
            for source, _ in originals:
                (Path(temporary) / Path(source).name).write_bytes(b"foreign DLL")
            with patch.dict(os.environ, {"PATH": temporary + os.pathsep + os.environ["PATH"]}):
                self.assertEqual(windows_tls.collect_windows_tls_binaries(), originals)


if __name__ == "__main__":
    unittest.main()
