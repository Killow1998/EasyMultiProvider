import json
import os
import tempfile
import unittest
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.config import ConfigError, api_key, load, merge_web_update, normalize, public_config, save
from easy_multi_provider.accounts import import_account, public_accounts


ensure_test_master_key()


class ConfigTests(unittest.TestCase):
    def setUp(self):
        self.provider = {
            "id": "deepseek",
            "name": "DeepSeek",
            "base_url": "https://api.deepseek.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "secret-value",
        }
        self.model = {
            "id": "deepseek/deepseek-chat",
            "provider": "deepseek",
            "reasoning_levels": ["medium"],
        }

    def test_normalize_and_public_redaction(self):
        value = normalize({"providers": [self.provider], "models": [self.model]})
        safe = public_config(value)
        self.assertEqual(safe["providers"][0]["api_key"], "••••••••")
        self.assertNotIn("secret-value", json.dumps(safe))

    def test_provider_tool_call_mode_defaults_and_validates(self):
        self.assertEqual(normalize({"providers": [self.provider]})["providers"][0]["tool_call_mode"], "native")
        disabled = dict(self.provider, tool_call_mode="disabled")
        self.assertEqual(normalize({"providers": [disabled]})["providers"][0]["tool_call_mode"], "disabled")
        with self.assertRaises(ConfigError):
            normalize({"providers": [dict(self.provider, tool_call_mode="text")]})

    def test_web_update_preserves_omitted_secret(self):
        current = normalize({"providers": [self.provider], "models": [self.model]})
        incoming = public_config(current)
        updated = merge_web_update(current, incoming)
        self.assertEqual(updated["providers"][0]["api_key"], "secret-value")

    def test_web_update_preserves_externalized_api_key(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            current = normalize({"providers": [self.provider], "models": [self.model]})
            save(current, path)
            current = load(path)
            updated = merge_web_update(current, public_config(current))
            save(updated, path)
            self.assertEqual(api_key(load(path)["providers"][0]), "secret-value")

    def test_web_update_cannot_supply_private_credential_paths(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            current = normalize({"providers": [self.provider], "models": [self.model]})
            save(current, path)
            current = load(path)
            incoming = public_config(current)
            incoming["providers"][0]["api_key_file"] = str(Path(directory).parent / "stolen.key")
            with self.assertRaises(ConfigError):
                merge_web_update(current, incoming, path)

    def test_loaded_account_path_must_match_derived_vault_path(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            raw = normalize({"account_store_path": str(Path(directory) / "accounts")})
            raw["accounts"] = [{
                "id": "ship",
                "name": "Ship",
                "prefix": "ship",
                "auth_file": str(Path(directory).parent / "auth.json.enc"),
            }]
            path.write_text(json.dumps(raw), encoding="utf-8")
            with self.assertRaises(ConfigError):
                load(path)

    def test_round_trip_uses_private_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            value = normalize({"providers": [self.provider], "models": [self.model]})
            save(value, path)
            loaded = load(path)
            self.assertEqual(api_key(loaded["providers"][0]), "secret-value")
            self.assertEqual(loaded["providers"][0]["api_key"], "")
            self.assertEqual(path.stat().st_mode & 0o777, 0o600)

    def test_api_key_is_stored_outside_config_file(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            value = normalize({"providers": [self.provider], "models": [self.model]})
            save(value, path)
            stored = load(path)
            self.assertEqual(api_key(stored["providers"][0]), "secret-value")
            self.assertNotIn("secret-value", path.read_text(encoding="utf-8"))
            secret_file = Path(stored["providers"][0]["api_key_file"])
            self.assertEqual(secret_file.stat().st_mode & 0o777, 0o600)

    def test_relative_config_path_does_not_delete_new_provider_key(self):
        original = Path.cwd()
        with tempfile.TemporaryDirectory() as directory:
            try:
                os.chdir(directory)
                path = Path("config.json")
                provider = dict(self.provider, api_key="", api_key_file="state/secrets/deepseek.key.enc")
                save(normalize({"providers": [provider]}), path)
                current = load(path)
                incoming = public_config(current)
                incoming["providers"][0]["api_key"] = "new-secret-value"
                save(merge_web_update(current, incoming, path), path)
                self.assertEqual(api_key(load(path)["providers"][0]), "new-secret-value")
            finally:
                os.chdir(original)

    def test_removed_provider_key_file_is_cleaned_after_save(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            save(normalize({"providers": [self.provider], "models": [self.model]}), path)
            secret_file = Path(load(path)["providers"][0]["api_key_file"])
            self.assertTrue(secret_file.exists())
            save(normalize({}), path)
            self.assertFalse(secret_file.exists())

    def test_unknown_model_provider_rejected(self):
        with self.assertRaises(ConfigError):
            normalize({"models": [self.model]})

    def test_provider_id_cannot_escape_secret_store(self):
        with self.assertRaises(ConfigError):
            normalize({"providers": [{"id": "..", "base_url": "https://example.com/v1"}]})
        with self.assertRaises(ConfigError):
            normalize({"providers": [{"id": "nested/provider", "base_url": "https://example.com/v1"}]})

    def test_non_loopback_host_is_rejected(self):
        with self.assertRaises(ConfigError):
            normalize({"host": "0.0.0.0"})

    def test_remote_cleartext_upstream_is_rejected(self):
        with self.assertRaises(ConfigError):
            normalize({"providers": [{"id": "demo", "base_url": "http://example.com/v1"}]})

    def test_model_context_window_has_a_safe_upper_bound(self):
        with self.assertRaises(ConfigError):
            normalize({
                "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
                "models": [{
                    "id": "demo/model",
                    "provider": "demo",
                    "context_window": 100_000_001,
                }],
            })
        self.assertEqual(
            normalize({"providers": [{"id": "demo", "base_url": "http://127.0.0.1:9999/v1"}]})[
                "providers"
            ][0]["base_url"],
            "http://127.0.0.1:9999/v1",
        )

    def test_anthropic_messages_provider_is_valid(self):
        value = normalize({
            "providers": [{
                "id": "anthropic",
                "base_url": "https://api.anthropic.com/v1",
                "protocol": "anthropic_messages",
                "auth_mode": "anthropic_api_key",
                "api_key": "secret-value",
            }]
        })
        self.assertEqual(value["providers"][0]["protocol"], "anthropic_messages")

    def test_auto_provider_protocol_is_valid(self):
        value = normalize({
            "providers": [{
                "id": "custom",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
            }]
        })
        self.assertEqual(value["providers"][0]["protocol"], "auto")

    def test_model_created_at_is_preserved(self):
        value = normalize({
            "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
            "models": [{"id": "demo/new", "provider": "demo", "created_at": 123}],
        })
        self.assertEqual(value["models"][0]["created_at"], 123)

    def test_account_import_is_separate_and_redacted(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory) / "state" / "accounts"
            config = normalize({"account_store_path": str(root)})
            auth = {
                "auth_mode": "chatgpt",
                "tokens": {
                    "access_token": "do-not-leak",
                    "refresh_token": "also-do-not-leak",
                    "account_id": "account-private",
                },
            }
            account = import_account(config, {"id": "ship", "name": "Ship", "prefix": "ship"}, auth)
            self.assertEqual(account["id"], "ship")
            encrypted_path = root / "ship" / "auth.json.enc"
            self.assertEqual(encrypted_path.stat().st_mode & 0o777, 0o600)
            self.assertNotIn("do-not-leak", encrypted_path.read_text(encoding="utf-8"))
            safe = public_accounts([account])
            encoded = json.dumps(safe)
            self.assertNotIn("do-not-leak", encoded)
            self.assertNotIn("refresh_token", encoded)
            self.assertNotIn("auth_file", safe[0])


if __name__ == "__main__":
    unittest.main()
