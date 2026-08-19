import base64
import json
import os
import tempfile
import threading
import unittest
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from unittest.mock import patch
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.server import AppState, WEB_FILE, configure_proxy_environment, make_handler


ensure_test_master_key()


class ServerAccountTests(unittest.TestCase):
    def test_system_proxy_is_imported_when_environment_has_none(self):
        proxy_keys = {
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        }
        clean_environment = {
            key: value for key, value in os.environ.items() if key not in proxy_keys
        }
        settings = {
            "http": "http://127.0.0.1:7897",
            "https": "http://127.0.0.1:7897",
            "all": "socks5://127.0.0.1:7897",
            "no": "localhost,127.0.0.1",
        }
        with patch.dict(os.environ, clean_environment, clear=True), patch(
            "easy_multi_provider.server.getproxies", return_value={}
        ), patch("easy_multi_provider.server._gnome_proxy_settings", return_value=settings):
            self.assertEqual(configure_proxy_environment(), "system")
            self.assertEqual(os.environ["HTTPS_PROXY"], settings["https"])
            self.assertEqual(os.environ["ALL_PROXY"], settings["all"])
            self.assertEqual(os.environ["NO_PROXY"], settings["no"])

    def test_explicit_proxy_environment_wins(self):
        with patch.dict(os.environ, {"HTTPS_PROXY": "http://proxy.invalid"}, clear=True), patch(
            "easy_multi_provider.server.getproxies"
        ) as system_proxies:
            self.assertEqual(configure_proxy_environment(), "environment")
            system_proxies.assert_not_called()

    def test_proxy_requests_are_not_serialized_by_state_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            entered_upstream = threading.Barrier(2, timeout=2)
            statuses = []

            def fake_proxy(*args):
                entered_upstream.wait()
                return {"kind": "json", "status": 200, "content_type": "application/json"}, b"{}"

            def request():
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/v1/responses",
                    b'{"model":"demo/fixed","input":"hello"}',
                    {
                        "Content-Type": "application/json",
                        "Cookie": "emp_session=" + state.session_token,
                    },
                )
                statuses.append(connection.getresponse().status)
                connection.close()

            try:
                with patch("easy_multi_provider.server.proxy", side_effect=fake_proxy):
                    workers = [threading.Thread(target=request) for _ in range(2)]
                    for worker in workers:
                        worker.start()
                    for worker in workers:
                        worker.join(3)
                self.assertTrue(all(not worker.is_alive() for worker in workers))
                self.assertEqual(sorted(statuses), [200, 200])
            finally:
                server.shutdown()
                server.server_close()

    def test_account_delete_removes_only_private_account_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize({"account_store_path": str(root / "state" / "accounts")}),
                config_path,
            )
            state = AppState(config_path)
            account = state.import_account(
                {"id": "ship", "name": "Ship", "prefix": "ship"},
                {"auth_mode": "chatgpt", "tokens": {"access_token": "account-secret"}},
            )
            auth_path = Path(account["auth_file"])
            self.assertTrue(auth_path.exists())
            state.delete_account("ship")
            self.assertFalse(auth_path.exists())
            self.assertFalse(auth_path.parent.exists())
            self.assertEqual(state.config["accounts"], [])
            self.assertTrue(config_path.exists())

    def test_web_config_update_keeps_api_key_out_of_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({"secret_store_path": str(root / "state" / "secrets")}), config_path)
            state = AppState(config_path)
            state.update({
                "providers": [{
                    "id": "demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "chat_completions",
                    "auth_mode": "api_key",
                    "api_key": "provider-secret",
                }],
                "models": [{"id": "demo/model", "provider": "demo"}],
            })
            self.assertNotIn("provider-secret", config_path.read_text(encoding="utf-8"))
            self.assertEqual(state.config["providers"][0]["api_key"], "")
            self.assertEqual(api_key(load(config_path)["providers"][0]), "provider-secret")

    def test_modal_submission_errors_are_visible_inside_modal(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertIn('id="modal_status"', html)
        self.assertIn("catch (error) { $('modal_status').textContent = error.message; }", html)
        self.assertIn("/api/integration/generate", html)
        self.assertIn("info.command", html)
        self.assertIn("position:fixed", html)
        self.assertIn("EMP 配置已生成", html)
        self.assertIn("/api/migration/export", html)
        self.assertIn("easy-multi-provider-0.2.0.emp", html)

    def test_integration_generation_endpoint_writes_profile(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {"Cookie": "emp_session=" + state.session_token}
                with patch.dict(os.environ, {"CODEX_HOME": str(root / "codex")}):
                    with patch(
                        "easy_multi_provider.server.generated_catalog_path",
                        return_value=root / "generated" / "codex-models.json",
                    ):
                        connection = HTTPConnection(*server.server_address)
                        connection.request("GET", "/api/integration", headers=headers)
                        response = connection.getresponse()
                        self.assertEqual(response.status, 200)
                        response.read()
                        connection.close()

                        connection = HTTPConnection(*server.server_address)
                        connection.request(
                            "POST",
                            "/api/integration/generate",
                            b"{}",
                            {**headers, "Content-Type": "application/json"},
                        )
                        response = connection.getresponse()
                        payload = json.loads(response.read().decode("utf-8"))
                        self.assertEqual(response.status, 200)
                        self.assertTrue(Path(payload["profile_path"]).exists())
                        self.assertTrue(Path(payload["catalog_path"]).exists())
                        connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_migration_endpoints_export_and_import_emp_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "secret_store_path": str(root / "state" / "secrets"),
                        "providers": [
                            {
                                "id": "demo",
                                "base_url": "https://example.com/v1",
                                "api_key": "provider-secret",
                            }
                        ],
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            headers = {
                "Cookie": "emp_session=" + state.session_token,
                "Content-Type": "application/json",
            }
            try:
                with patch(
                    "easy_multi_provider.server.generated_catalog_path",
                    return_value=root / "generated" / "catalog.json",
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/migration/export",
                        json.dumps({"password": "migration-pass-3"}).encode(),
                        headers,
                    )
                    response = connection.getresponse()
                    bundle = response.read()
                    self.assertEqual(response.status, 200)
                    self.assertIn("easy-multi-provider-0.2.0.emp", response.getheader("Content-Disposition"))
                    self.assertTrue(bundle.startswith(b"EMP-MIGRATION"))
                    connection.close()

                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/migration/import",
                        json.dumps(
                            {
                                "password": "migration-pass-3",
                                "bundle": base64.b64encode(bundle).decode("ascii"),
                            }
                        ).encode(),
                        headers,
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode())
                    self.assertEqual(response.status, 200)
                    self.assertEqual(payload["providers"], 1)
                    connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_provider_discovery_adds_models_and_preserves_hidden_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize({
                    "native_catalog_path": str(root / "missing-native.json"),
                    "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
                    "models": [{
                        "id": "demo/hidden",
                        "provider": "demo",
                        "enabled": False,
                    }],
                }),
                config_path,
            )
            state = AppState(config_path)
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=[
                    {
                        "upstream_id": "hidden",
                        "reasoning_levels": ["low", "high"],
                        "context_window": 123,
                    },
                    {
                        "upstream_id": "new-model",
                        "display_name": "New model",
                        "reasoning_levels": ["medium"],
                        "context_window": 456,
                    },
                ],
            ), patch("easy_multi_provider.server.write_catalog", return_value=root / "catalog.json"):
                preview = state.discover_provider_models("demo")
                self.assertEqual(preview["available"], 2)
                result = state.discover_provider_models("demo", ["hidden", "new-model"])
            self.assertEqual(result["added"], 1)
            models = {item["id"]: item for item in state.config["models"]}
            self.assertFalse(models["demo/hidden"]["enabled"])
            self.assertEqual(models["demo/hidden"]["reasoning_levels"], ["low", "high"])
            self.assertTrue(models["demo/new-model"]["enabled"])

    def test_account_upload_needs_no_manual_web_token_and_never_returns_auth_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "account_store_path": str(root / "state" / "accounts"),
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                body = json.dumps(
                    {
                        "id": "ship",
                        "name": "Ship",
                        "prefix": "ship",
                        "auth_json": {
                            "auth_mode": "chatgpt",
                            "tokens": {"access_token": "account-secret"},
                        },
                    }
                ).encode("utf-8")
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/accounts/import",
                    body,
                    {
                        "Content-Type": "application/json",
                        "Cookie": "emp_session=" + state.session_token,
                    },
                )
                response = connection.getresponse()
                payload = response.read().decode("utf-8")
                self.assertEqual(response.status, 200)
                self.assertNotIn("account-secret", payload)
                self.assertNotIn("auth_json", payload)
                connection.close()
                self.assertNotIn("account-secret", config_path.read_text(encoding="utf-8"))
                self.assertNotIn("refresh_token", config_path.read_text(encoding="utf-8"))
                self.assertTrue((root / "state" / "accounts" / "ship" / "auth.json.enc").exists())
                self.assertFalse((root / "state" / "accounts" / "ship" / "auth.json").exists())

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/accounts",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                listing = json.loads(response.read().decode("utf-8"))
                self.assertEqual(listing["accounts"][0]["prefix"], "ship")
                self.assertTrue(listing["accounts"][0]["credential_set"])
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "DELETE",
                    "/api/accounts/ship",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read().decode("utf-8"))["status"], "ok")
                connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_quota_refresh_persists_only_safe_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "account_store_path": str(root / "state" / "accounts"),
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            state.import_account(
                {"id": "ship", "name": "Ship", "prefix": "ship"},
                {"auth_mode": "chatgpt", "tokens": {"access_token": "account-secret"}},
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                snapshot = {
                    "account_label": "s***@example.com",
                    "plan_type": "plus",
                    "rate_limits": {"primary": {"usedPercent": 10}},
                    "updated_at": 123,
                }
                with patch("easy_multi_provider.server.refresh_account_quota", return_value=snapshot):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/accounts/ship/quota",
                        b"{}",
                        {
                            "Content-Type": "application/json",
                            "Cookie": "emp_session=" + state.session_token,
                        },
                    )
                    response = connection.getresponse()
                    payload = response.read().decode("utf-8")
                    self.assertEqual(response.status, 200)
                    self.assertIn('"usedPercent": 10', payload)
                    self.assertNotIn("account-secret", payload)
                    connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_management_and_proxy_require_automatic_session_or_caller_auth(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/config")
                self.assertEqual(connection.getresponse().status, 401)
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/config",
                    headers={
                        "Cookie": "emp_session=" + state.session_token,
                        "Host": "evil.example:%d" % server.server_address[1],
                        "Origin": "http://evil.example:%d" % server.server_address[1],
                    },
                )
                self.assertEqual(connection.getresponse().status, 403)
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/v1/responses",
                    b'{"model":"demo/fixed","input":"hello"}',
                    {
                        "Content-Type": "application/json",
                        "Authorization": "Bearer attacker-controlled",
                    },
                )
                self.assertEqual(connection.getresponse().status, 401)
                connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_web_root_requires_bootstrap_url_before_issuing_session(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/")
                response = connection.getresponse()
                self.assertEqual(response.status, 401)
                self.assertIsNone(response.getheader("Set-Cookie"))
                response.read()
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/?bootstrap=" + state.bootstrap_token)
                response = connection.getresponse()
                self.assertEqual(response.status, 303)
                self.assertIn("emp_session=", response.getheader("Set-Cookie"))
                response.read()
                connection.close()
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
