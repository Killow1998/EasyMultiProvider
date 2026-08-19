import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider.accounts import public_accounts
from easy_multi_provider.catalog import build_catalog, integration_info, write_catalog, write_codex_profile
from easy_multi_provider.config import normalize
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class CatalogTests(unittest.TestCase):
    def test_merges_native_and_external_models(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [{
                "slug": "gpt-native",
                "display_name": "Native",
                "base_instructions": "Be useful",
                "model_messages": {},
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [{"effort": "low", "description": "low"}],
            }]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
                "models": [{"id": "demo/model", "provider": "demo", "reasoning_levels": ["medium"]}],
            })
            catalog = build_catalog(config)
            self.assertEqual([item["slug"] for item in catalog["models"]], ["gpt-native", "demo/model"])
            self.assertEqual(catalog["models"][1]["supported_reasoning_levels"][0]["effort"], "medium")

            output = Path(directory) / "generated" / "models.json"
            write_catalog(config, output)
            self.assertTrue(output.exists())
            self.assertEqual(json.loads(output.read_text(encoding="utf-8"))["models"][-1]["slug"], "demo/model")

    def test_integration_snippet_uses_dedicated_non_websocket_provider(self):
        config = normalize({"host": "127.0.0.1", "port": 4200})
        info = integration_info(config, Path("generated/codex-models.json"))
        self.assertIn('model_provider = "easy-multi-provider"', info["snippet"])
        self.assertIn('[model_providers.easy-multi-provider]', info["snippet"])
        self.assertIn('supports_websockets = false', info["snippet"])
        self.assertIn('requires_openai_auth = true', info["snippet"])
        self.assertNotIn("openai_base_url", info["snippet"])

    def test_writes_emp_profile_under_codex_home(self):
        config = normalize({"host": "127.0.0.1", "port": 4200})
        with tempfile.TemporaryDirectory() as directory, patch.dict(
            os.environ, {"CODEX_HOME": str(Path(directory) / "codex")}
        ):
            catalog_path = Path(directory) / "generated" / "codex-models.json"
            profile_path = write_codex_profile(config, catalog_path)
            self.assertEqual(profile_path, Path(directory) / "codex" / "emp.config.toml")
            contents = profile_path.read_text(encoding="utf-8")
            self.assertIn('model_provider = "easy-multi-provider"', contents)
            self.assertIn('model_catalog_json = ', contents)
            self.assertEqual(profile_path.stat().st_mode & 0o777, 0o600)

    def test_subscription_accounts_get_prefixed_native_aliases(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [{
                "slug": "gpt-native",
                "display_name": "Native",
                "base_instructions": "Be useful",
                "model_messages": {},
                "default_reasoning_level": "low",
                "supported_reasoning_levels": [{"effort": "low", "description": "low"}],
            }]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "accounts": [
                    {"id": "ship", "name": "Ship", "prefix": "ship", "auth_file": "/tmp/ship-auth.json"},
                    {"id": "plus", "name": "Plus", "prefix": "plus258", "auth_file": "/tmp/plus-auth.json"},
                ],
            })
            slugs = [item["slug"] for item in build_catalog(config)["models"]]
            self.assertEqual(slugs, ["gpt-native", "ship/gpt-native", "plus258/gpt-native"])

    def test_duplicate_subscription_is_marked_and_filtered_from_catalog(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native_path = root / "native.json"
            native_path.write_text(json.dumps({"models": [{
                "slug": "gpt-native",
                "display_name": "Native",
                "base_instructions": "Be useful",
                "model_messages": {},
            }]}), encoding="utf-8")
            codex_home = root / "codex"
            codex_home.mkdir()
            (codex_home / "auth.json").write_text(json.dumps({
                "tokens": {"access_token": "current-token", "account_id": "same-account"},
            }), encoding="utf-8")
            duplicate_path = root / "duplicate.enc"
            unique_path = root / "unique.enc"
            write_encrypted_json(duplicate_path, {
                "tokens": {"access_token": "other-token", "account_id": "same-account"},
            })
            write_encrypted_json(unique_path, {
                "tokens": {"access_token": "unique-token", "account_id": "different-account"},
            })
            config = normalize({
                "native_catalog_path": str(native_path),
                "accounts": [
                    {"id": "same", "prefix": "same", "auth_file": str(duplicate_path)},
                    {"id": "unique", "prefix": "unique", "auth_file": str(unique_path)},
                ],
            })
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}):
                catalog = build_catalog(config)
                accounts = public_accounts(config["accounts"])
            self.assertEqual(
                [item["slug"] for item in catalog["models"]],
                ["gpt-native", "unique/gpt-native"],
            )
            duplicate = next(item for item in accounts if item["id"] == "same")
            self.assertTrue(duplicate["duplicate"])
            self.assertEqual(duplicate["duplicate_of"], "当前 Codex 登录")

    def test_disabled_external_model_is_not_in_catalog(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize({
                "native_catalog_path": str(Path(directory) / "missing-native.json"),
            "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
            "models": [{"id": "demo/hidden", "provider": "demo", "enabled": False}],
            })
            self.assertEqual(build_catalog(config)["models"], [])


if __name__ == "__main__":
    unittest.main()
