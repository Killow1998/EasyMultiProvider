import json
import os
import tempfile
import unittest
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.capabilities import endpoint_fingerprint
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

    def test_provider_has_no_configurable_tool_call_mode(self):
        normalized = normalize({"providers": [self.provider]})["providers"][0]
        self.assertNotIn("tool_call_mode", normalized)

    def test_web_update_preserves_omitted_secret(self):
        current = normalize({"providers": [self.provider], "models": [self.model]})
        incoming = public_config(current)
        updated = merge_web_update(current, incoming)
        self.assertEqual(updated["providers"][0]["api_key"], "secret-value")

    def test_protocol_observation_and_capability_sources_round_trip_safely(self):
        value = normalize({
            "providers": [{
                **self.provider,
                "protocol": "auto",
                "resolved_protocol": "responses",
                "protocol_observation": {
                    "source": "observed",
                    "confidence": 1,
                    "observed_at": "2026-08-21T00:00:00+00:00",
                    "endpoint_fingerprint": endpoint_fingerprint(self.provider["base_url"]),
                    "deployment_identity": "production",
                    "upstream_model": "deepseek-chat",
                },
            }],
            "models": [{
                **self.model,
                "context_window": 128000,
                "output_limit": 4096,
                "capability_sources": {
                    "reasoning_levels": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-21T00:00:00+00:00",
                    },
                    "context_window": {
                        "source": "manual",
                        "confidence": 1,
                        "observed_at": "2026-08-21T00:01:00+00:00",
                    },
                },
            }],
        })
        safe = public_config(value)
        provider = safe["providers"][0]
        model = safe["models"][0]
        self.assertEqual(provider["protocol"], "auto")
        self.assertEqual(provider["resolved_protocol"], "responses")
        self.assertEqual(provider["protocol_observation"]["source"], "observed")
        self.assertEqual(model["capability_sources"]["reasoning_levels"]["source"], "advertised")
        self.assertEqual(model["capability_sources"]["context_window"]["source"], "manual")
        self.assertNotIn("secret-value", json.dumps(safe))

    def test_old_model_values_receive_inferred_provenance(self):
        value = normalize({"providers": [self.provider], "models": [{
            **self.model,
            "context_window": 128000,
        }]})
        sources = value["models"][0]["capability_sources"]
        self.assertEqual(sources["reasoning_levels"]["source"], "inferred")
        self.assertEqual(sources["context_window"]["source"], "inferred")

    def test_web_changed_capability_values_receive_manual_provenance(self):
        current = normalize({
            "providers": [self.provider],
            "models": [{
                **self.model,
                "context_window": 128000,
                "capability_sources": {
                    "context_window": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-21T00:00:00+00:00",
                    },
                },
            }],
        })
        incoming = public_config(current)
        incoming["models"][0]["context_window"] = 256000
        incoming["models"][0]["reasoning_levels"] = ["high"]
        updated = merge_web_update(current, incoming)
        sources = updated["models"][0]["capability_sources"]
        self.assertEqual(sources["context_window"]["source"], "manual")
        self.assertEqual(sources["reasoning_levels"]["source"], "manual")

    def test_context_calibration_round_trips_as_safe_numeric_identity_and_web_cannot_overwrite(self):
        fingerprint = endpoint_fingerprint(self.provider["base_url"])
        current = normalize({
            "providers": [self.provider],
            "models": [{
                **self.model,
                "upstream_id": "deepseek-chat",
                "context_calibrations": [{
                    "endpoint_fingerprint": fingerprint,
                    "upstream_model": "deepseek-chat",
                    "protocol": "chat_completions",
                    "deployment_identity": "default",
                    "largest_success_estimate": 1000,
                    "smallest_failure_estimate": 1200,
                    "largest_success_source": "observed",
                    "smallest_failure_source": "observed",
                }],
            }],
        })
        incoming = public_config(current)
        incoming["models"][0]["context_calibrations"] = [{
            "endpoint_fingerprint": fingerprint,
            "upstream_model": "attacker-model",
            "protocol": "responses",
            "deployment_identity": "default",
            "smallest_failure_estimate": 1,
        }]
        updated = merge_web_update(current, incoming)
        calibration = updated["models"][0]["context_calibrations"][0]
        self.assertEqual(calibration["upstream_model"], "deepseek-chat")
        self.assertEqual(calibration["largest_success_estimate"], 1000)
        serialized = json.dumps(public_config(updated))
        self.assertNotIn("secret-value", serialized)

    def test_protocol_observation_rejects_raw_endpoint(self):
        with self.assertRaises(ConfigError):
            normalize({
                "providers": [{
                    **self.provider,
                    "protocol_observation": {
                        "source": "observed",
                        "endpoint_fingerprint": "https://example.com/v1?key=secret",
                    },
                }],
            })

    def test_web_update_saves_account_model_visibility_and_preserves_credentials(self):
        current = normalize({
            "accounts": [{
                "id": "primary",
                "prefix": "primary",
                "auth_file": "/tmp/primary-auth.json.enc",
            }],
        })
        incoming = public_config(current)
        incoming["accounts"][0]["hidden_models"] = ["gpt-optional"]
        updated = merge_web_update(current, incoming)
        self.assertEqual(updated["accounts"][0]["hidden_models"], ["gpt-optional"])
        self.assertEqual(updated["accounts"][0]["auth_file"], "/tmp/primary-auth.json.enc")

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
                "id": "primary",
                "name": "Primary",
                "prefix": "primary",
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
            account = import_account(config, {"id": "primary", "name": "Primary", "prefix": "primary"}, auth)
            self.assertEqual(account["id"], "primary")
            encrypted_path = root / "primary" / "auth.json.enc"
            self.assertEqual(encrypted_path.stat().st_mode & 0o777, 0o600)
            self.assertNotIn("do-not-leak", encrypted_path.read_text(encoding="utf-8"))
            safe = public_accounts([account])
            encoded = json.dumps(safe)
            self.assertNotIn("do-not-leak", encoded)
            self.assertNotIn("refresh_token", encoded)
            self.assertNotIn("auth_file", safe[0])

    def test_account_hidden_models_are_normalized_and_public(self):
        account = normalize({
            "accounts": [{
                "id": "primary",
                "prefix": "primary",
                "hidden_models": ["gpt-old", "gpt-old", "codex-auto-review"],
            }],
        })["accounts"][0]
        self.assertEqual(account["hidden_models"], ["codex-auto-review", "gpt-old"])
        self.assertEqual(public_accounts([account])[0]["hidden_models"], account["hidden_models"])


if __name__ == "__main__":
    unittest.main()
