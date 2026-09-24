import json
import os
import tempfile
import threading
import unittest
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider.config import normalize, save
from easy_multi_provider.server import AppState, make_handler
from easy_multi_provider.support_report import build_support_report


ensure_test_master_key()


class SupportReportTests(unittest.TestCase):
    def test_custom_xdg_directory_is_masked(self):
        with tempfile.TemporaryDirectory(prefix="private-xdg-marker-") as directory:
            xdg = Path(directory) / "private-xdg-root"
            state = SimpleNamespace(
                path=xdg / "easy-multi-provider" / "config.json",
                codex_compatibility_snapshot=lambda: {},
                runtime_sync_snapshot=lambda: {},
                accounts_snapshot=lambda: {"native_account": {}, "accounts": []},
            )
            with patch.dict(os.environ, {"XDG_CONFIG_HOME": str(xdg)}), patch(
                "easy_multi_provider.support_report.proxy_for_url", return_value=None
            ):
                report = build_support_report(state)
            self.assertEqual(report["configuration"]["location"], "desktop_default")
            self.assertEqual(report["configuration"]["path"], "<xdg-config>/easy-multi-provider/config.json")
            self.assertNotIn(directory, json.dumps(report))

    def test_packaged_desktop_paths_are_classified_on_mac_and_windows(self):
        home = Path.home()
        cases = (
            ("darwin", home / "Library" / "Application Support" / "EasyMultiProvider" / "config.json",
             "~/Library/Application Support/EasyMultiProvider/config.json"),
            ("win32", home / "AppData" / "Local" / "EasyMultiProvider" / "config.json",
             "<user-config>/EasyMultiProvider/config.json"),
        )
        for platform_name, path, display in cases:
            with self.subTest(platform=platform_name), patch(
                "easy_multi_provider.support_report.sys.platform", platform_name
            ), patch.dict(os.environ, {"LOCALAPPDATA": "", "APPDATA": ""}), patch(
                "easy_multi_provider.support_report.proxy_for_url", return_value=None
            ):
                state = SimpleNamespace(
                    path=path,
                    codex_compatibility_snapshot=lambda: {},
                    runtime_sync_snapshot=lambda: {},
                    accounts_snapshot=lambda: {"native_account": {}, "accounts": []},
                )
                report = build_support_report(state)
                self.assertEqual(report["configuration"]["location"], "desktop_default")
                self.assertEqual(report["configuration"]["path"], display)

    def test_report_classifies_known_failures_without_exporting_private_fields(self):
        with tempfile.TemporaryDirectory(prefix="private-home-marker-") as directory:
            root = Path(directory)
            state = SimpleNamespace(
                path=root / "private-config-marker" / "config.json",
                codex_compatibility_snapshot=lambda: {
                    "installed": "0.156.1", "status": "recommended", "source": "path_cli",
                    "runtimes": [{"installed": "0.156.1", "status": "recommended",
                                  "source": "path_cli", "path": "/secret/runtime-path",
                                  "name": "private-runtime-name", "helper": True}],
                },
                runtime_sync_snapshot=lambda: {
                    "state": "emp_loaded", "target": "emp", "verified": True,
                    "detail": "Bearer private-runtime-detail",
                },
                accounts_snapshot=lambda: {
                    "native_account": {"credential_set": True, "quota": None,
                                       "email": "native-secret@example.com"},
                    "accounts": [
                        {"id": "private-account-one", "name": "Alice Secret",
                         "credential_set": True, "credential_status": "invalid",
                         "quota": {"access_token": "private-token"}},
                        {"id": "private-account-two", "name": "Bob Secret",
                         "credential_set": True, "credential_status": "invalid", "quota": None},
                    ],
                    "refresh_errors": {
                        "private-account-one": "quota_auth_required",
                        "private-account-two": "quota_transport_error",
                    },
                },
            )
            with patch("easy_multi_provider.support_report.proxy_for_url", return_value="socks5h://private-user:private-password@private-proxy:1080"):
                report = build_support_report(state, "system")
            encoded = json.dumps(report)
            for secret in (
                directory, "private-config-marker", "/secret/runtime-path",
                "private-runtime-name", "private-runtime-detail", "native-secret@example.com",
                "private-account-one", "private-account-two", "Alice Secret", "Bob Secret",
                "private-token", "private-user", "private-password", "private-proxy",
            ):
                self.assertNotIn(secret, encoded)
            self.assertEqual(report["configuration"]["path"], "<custom>/config.json")
            self.assertEqual(report["codex"]["version"], "0.156.1")
            self.assertEqual(report["codex"]["state"], "emp_loaded")
            self.assertEqual(report["network"]["proxy_scheme"], "socks5h")
            self.assertEqual(report["network"]["connectivity_probe"], "not_run")
            self.assertEqual(report["accounts"]["native"]["quota_status"], "not_checked")
            self.assertEqual([item["quota_status"] for item in report["accounts"]["imported"]],
                             ["auth_required", "transport_error"])

    def test_report_endpoint_requires_management_session_and_is_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            state.proxy_source_at_startup = "direct"
            state.codex_compatibility_snapshot = lambda: {"installed": "0.156.1", "status": "recommended", "source": "path_cli"}
            state.refresh_account = lambda *_: self.fail("report must not refresh imported quota")
            state.refresh_native_account = lambda: self.fail("report must not refresh native quota")
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/support-report")
                denied = connection.getresponse()
                denied.read()
                connection.close()
                self.assertEqual(denied.status, 401)

                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/support-report", headers={
                    "Cookie": "emp_session=" + state.session_token,
                })
                response = connection.getresponse()
                report = json.loads(response.read().decode("utf-8"))
                connection.close()
                self.assertEqual(response.status, 200)
                self.assertEqual(response.getheader("Cache-Control"), "no-store")
                self.assertIn("attachment", response.getheader("Content-Disposition"))
                self.assertEqual(report["schema_version"], 1)
                self.assertEqual(report["network"]["source_at_startup"], "direct")
                self.assertEqual(report["accounts"]["imported_count"], 0)
                self.assertNotIn(directory, json.dumps(report))
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
