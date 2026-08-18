import json
import tempfile
import threading
import unittest
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from unittest.mock import patch
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.server import AppState, make_handler


ensure_test_master_key()


class ServerAccountTests(unittest.TestCase):
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
                    {"Content-Type": "application/json"},
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
                result = state.discover_provider_models("demo")
            self.assertEqual(result["available"], 2)
            self.assertEqual(result["added"], 1)
            models = {item["id"]: item for item in state.config["models"]}
            self.assertFalse(models["demo/hidden"]["enabled"])
            self.assertEqual(models["demo/hidden"]["reasoning_levels"], ["low", "high"])
            self.assertTrue(models["demo/new-model"]["enabled"])

    def test_account_upload_works_without_web_token_and_never_returns_auth_json(self):
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
                    {"Content-Type": "application/json"},
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
                connection.request("GET", "/api/accounts")
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
                with patch("easy_multi_provider.server.read_account_quota", return_value=snapshot):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/accounts/ship/quota",
                        b"{}",
                        {"Content-Type": "application/json"},
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


if __name__ == "__main__":
    unittest.main()
