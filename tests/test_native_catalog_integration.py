import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.codex_runtime import CodexRuntimeController
from easy_multi_provider.config import normalize, save
from easy_multi_provider.integration import IntegrationManager
from easy_multi_provider.server import AppState, make_handler


class _ForbiddenRuntimeAccess:
    def run(self, *_args, **_kwargs):
        raise AssertionError("native display settings must not run Codex commands")

    def model_list(self, *_args, **_kwargs):
        raise AssertionError("model IDs cannot verify native display settings")

    def stop_stale_codex_hosts(self):
        raise AssertionError("native display settings must not stop Codex")


class NativeCatalogIntegrationTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.codex_home = self.root / "codex"
        self.codex_home.mkdir()
        environment = patch.dict(os.environ, {"CODEX_HOME": str(self.codex_home)})
        environment.start()
        self.addCleanup(environment.stop)
        self.native_path = self.codex_home / "models_cache.json"
        self.native_path.write_text(json.dumps({"models": [
            {"slug": "gpt-native-a", "display_name": "Native A"},
            {"slug": "gpt-native-b", "display_name": "Native B"},
            {"slug": "codex-auto-review", "visibility": "hide"},
        ]}), encoding="utf-8")
        self.config_path = self.codex_home / "config.toml"
        self.original_config = b'model = "gpt-native-a"\n# Keep user settings\n'
        self.config_path.write_bytes(self.original_config)
        self.auth_path = self.codex_home / "auth.json"
        self.auth_path.write_text('{"test_fixture": true}', encoding="utf-8")
        self.original_auth = self.auth_path.read_bytes()
        self.original_cache = self.native_path.read_bytes()
        self.emp_path = self.root / "emp.json"
        save(normalize({"native_catalog_path": str(self.native_path)}), self.emp_path)
        manager = IntegrationManager(
            self.config_path,
            self.codex_home / "easy-multi-provider/integration/lease.json",
            instance_id="native-catalog-test",
        )
        forbidden = _ForbiddenRuntimeAccess()
        self.state = AppState(
            self.emp_path,
            integration_manager=manager,
            runtime_controller=CodexRuntimeController(
                runner=forbidden,
                model_catalog_probe=forbidden,
                host_stopper=forbidden,
                target_codex_home=self.codex_home,
            ),
        )
        self.state.codex_compatibility_snapshot = lambda: {"status": "unknown"}
        self.state.mark_service_ready()

    def request(self, path, body=None):
        handler = object.__new__(make_handler(self.state))
        handler.path = path
        handler.headers = {}
        handler.server = type("Server", (), {"server_address": ("127.0.0.1", 4201)})()
        handler._management_allowed = lambda: True
        handler._body = lambda _limit: body or {}
        captured = {}
        handler._send = lambda status, data, *args, **kwargs: captured.update(
            status=status, payload=json.loads(data)
        )
        handler.do_POST()
        return captured["status"], captured["payload"]

    def save_display(self, hidden, alias):
        candidate = self.state.snapshot()
        candidate["native_hidden_models"] = hidden
        candidate["catalog_family_presentations"] = {
            "gpt-native-a": {"catalog_alias": alias, "show_context": False}
        }
        status, _ = self.request("/api/config", candidate)
        self.assertEqual(status, 200)

    def assert_native_files_untouched(self):
        self.assertEqual(self.auth_path.read_bytes(), self.original_auth)
        self.assertEqual(self.native_path.read_bytes(), self.original_cache)

    def test_native_only_enable_applies_hidden_models_and_names_then_restores(self):
        self.save_display(["gpt-native-b"], "Daily coding")
        status, payload = self.request("/api/integration/enable", {"confirm_reload": True})
        self.assertEqual(status, 200)
        self.assertEqual(payload["configuration"]["state"], "emp_applied")
        self.assertEqual(payload["runtime"]["state"], "catalog_unverified")
        self.assertFalse(payload["runtime"]["verified"])
        self.assertTrue(payload["runtime"]["action_required"])
        catalog = json.loads(self.state.integration_catalog_path.read_text(encoding="utf-8"))
        models = {item["slug"]: item for item in catalog["models"]}
        self.assertEqual(models["gpt-native-a"]["display_name"], "Daily coding")
        self.assertNotIn("gpt-native-b", models)
        self.assertEqual(models["codex-auto-review"]["visibility"], "hide")
        self.assertIn("model_catalog_json", self.config_path.read_text(encoding="utf-8"))
        self.assert_native_files_untouched()
        record = self.state.runtime_recovery_store.load()
        self.assertEqual(record.state, "catalog_unverified")
        self.assertEqual(record.expected_models, ())

        for operation in ("reload", "verify", "restore"):
            status, payload = self.request("/api/integration/" + operation, {"confirm_reload": True})
            self.assertEqual(status, 200)
            self.assertEqual(payload["runtime"]["state"], "catalog_unverified")
            self.assertFalse(payload["runtime"]["verified"])
        self.assertEqual(self.config_path.read_bytes(), self.original_config)
        self.assert_native_files_untouched()

    def test_native_rename_only_can_enable_and_later_refresh(self):
        self.save_display([], "First name")
        status, _ = self.request("/api/integration/enable", {"confirm_reload": True})
        self.assertEqual(status, 200)
        self.save_display(["gpt-native-b"], "Revised name")
        status, _ = self.request("/api/catalog/refresh")
        self.assertEqual(status, 200)
        catalog = json.loads(self.state.integration_catalog_path.read_text(encoding="utf-8"))
        visible = [item for item in catalog["models"] if item.get("visibility", "list") == "list"]
        self.assertEqual([(item["slug"], item["display_name"]) for item in visible],
                         [("gpt-native-a", "Revised name")])
        self.assert_native_files_untouched()

    def test_native_only_enable_still_requires_confirmation(self):
        status, _ = self.request("/api/integration/enable")
        self.assertEqual(status, 409)
        self.assertEqual(self.config_path.read_bytes(), self.original_config)
        self.assertFalse(self.state.integration_catalog_path.exists())
        self.assert_native_files_untouched()

    def test_empty_or_fully_hidden_catalog_is_rejected_without_writing(self):
        for missing in (False, True):
            with self.subTest(missing=missing):
                if missing:
                    self.native_path.unlink()
                self.save_display(["gpt-native-a", "gpt-native-b"], "Daily coding")
                status, payload = self.request("/api/integration/enable", {"confirm_reload": True})
                self.assertEqual(status, 409)
                self.assertEqual(payload["error"]["code"], "empty_emp_catalog")
                self.assertEqual(self.config_path.read_bytes(), self.original_config)
                self.assertFalse(self.state.integration_catalog_path.exists())
                self.assertEqual(self.auth_path.read_bytes(), self.original_auth)


if __name__ == "__main__":
    unittest.main()
