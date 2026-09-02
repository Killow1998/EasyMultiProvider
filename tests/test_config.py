import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

import easy_multi_provider.config as config_module
from tests.support import ensure_test_master_key
from easy_multi_provider.capabilities import endpoint_fingerprint
from easy_multi_provider.config import ConfigError, api_key, load, merge_web_update, normalize, public_config, save
from easy_multi_provider.accounts import import_account, public_accounts


ensure_test_master_key()


class ConfigTests(unittest.TestCase):
    def test_general_web_save_preserves_separately_saved_runtime_selection(self):
        current = normalize({"codex_runtime_sources": ["codex_app"]})
        for stale_sources in (["auto"], ["cursor"], None):
            with self.subTest(stale_sources=stale_sources):
                incoming = public_config(current)
                if stale_sources is None:
                    incoming.pop("codex_runtime_sources")
                else:
                    incoming["codex_runtime_sources"] = stale_sources
                incoming["subscription_search"] = {"enabled": True, "account_id": ""}
                saved = merge_web_update(current, incoming)
                self.assertEqual(saved["codex_runtime_sources"], ["codex_app"])
                self.assertTrue(saved["subscription_search"]["enabled"])

    def test_legacy_search_account_selection_migrates_to_automatic(self):
        value = normalize(
            {
                "accounts": [{"id": "search", "prefix": "search"}],
                "subscription_search": {"enabled": True, "account_id": "search"},
            }
        )
        self.assertEqual(
            value["subscription_search"], {"enabled": True, "account_id": ""}
        )
        missing = normalize(
            {"subscription_search": {"enabled": True, "account_id": "removed"}}
        )
        self.assertEqual(missing["subscription_search"]["account_id"], "")

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

    def test_codex_runtime_sources_are_bounded_configuration(self):
        self.assertEqual(normalize({})["codex_runtime_sources"], ["auto"])
        self.assertEqual(
            normalize({"codex_runtime_sources": ["codex_app", "cursor"]})[
                "codex_runtime_sources"
            ],
            ["codex_app", "cursor"],
        )
        with self.assertRaises(ConfigError):
            normalize({"codex_runtime_sources": ["auto", "cursor"]})
        with self.assertRaises(ConfigError):
            normalize({"codex_runtime_sources": ["arbitrary-runtime"]})

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

    def test_route_keyed_catalog_presentations_normalize_without_changing_models(self):
        value = normalize({
            "providers": [self.provider],
            "models": [{**self.model, "family_id": "deepseek-chat"}],
            "catalog_presentations": {
                "gpt-native": {
                    "catalog_alias": "General",
                    "show_context": False,
                    "reasoning_summary": "hide",
                },
                "deepseek/deepseek-chat": {
                    "catalog_alias": "Worker",
                    "show_context": True,
                    "reasoning_summary": "show",
                },
            },
        })

        self.assertEqual(
            value["catalog_presentations"]["gpt-native"],
            {
                "catalog_alias": "General",
                "show_context": False,
                "reasoning_summary": "hide",
            },
        )
        self.assertEqual(value["models"][0]["id"], "deepseek/deepseek-chat")
        self.assertEqual(value["models"][0]["family_id"], "deepseek-chat")
        self.assertEqual(value["models"][0]["display_name"], "")

        with self.assertRaises(ConfigError):
            normalize({"catalog_presentations": {"bad route": {}}})
        with self.assertRaises(ConfigError):
            normalize({
                "catalog_presentations": {
                    "gpt-native": {"reasoning_summary": "raw-chain"}
                }
            })

    def test_family_presentations_and_native_visibility_are_bounded_config(self):
        value = normalize(
            {
                "native_hidden_models": ["gpt-5.5", "gpt-5.5"],
                "catalog_family_presentations": {
                    "gpt-5.6-sol": {
                        "catalog_alias": "将军",
                        "show_context": False,
                        "reasoning_summary": "show",
                    }
                },
            }
        )

        self.assertEqual(value["native_hidden_models"], ["gpt-5.5"])
        self.assertEqual(
            value["catalog_family_presentations"]["gpt-5.6-sol"],
            {
                "catalog_alias": "将军",
                "show_context": False,
                "reasoning_summary": "show",
            },
        )

    def test_reasoning_summary_capability_is_explicit_and_persisted(self):
        for value in (True, False, None):
            with self.subTest(value=value):
                config = normalize(
                    {
                        "providers": [self.provider],
                        "models": [
                            {
                                **self.model,
                                "supports_reasoning_summaries": value,
                            }
                        ],
                    }
                )
                self.assertIs(
                    config["models"][0]["supports_reasoning_summaries"], value
                )

        with self.assertRaises(ConfigError):
            normalize(
                {
                    "providers": [self.provider],
                    "models": [
                        {
                            **self.model,
                            "supports_reasoning_summaries": "yes",
                        }
                    ],
                }
            )

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
            if os.name != "nt":
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
            if os.name != "nt":
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

    def test_provider_base_url_rejects_query_and_fragment(self):
        for base_url in (
            "https://example.com/v1?tenant=a",
            "https://example.com/v1#deployment-a",
        ):
            with self.subTest(base_url=base_url), self.assertRaises(ConfigError):
                normalize({"providers": [{"id": "demo", "base_url": base_url}]})

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
            if os.name != "nt":
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


class TestNewCapabilityFieldsConfigRoundTrip(ConfigTests):
    """Regression tests for output_modalities, supported_protocols, max_input_tokens, reasoning_control."""

    def setUp(self):
        super().setUp()
        self.rich_model = {
            **self.model,
            "context_window": 128000,
            "max_input_tokens": 100000,
            "output_limit": 4096,
            "reasoning_control": "reasoning.effort enum: low, high",
            "input_modalities": ["text", "image"],
            "output_modalities": ["text", "audio"],
            "supported_protocols": ["responses", "chat_completions"],
            "capabilities": {
                "streaming": True,
                "structured_output": True,
                "web_search": False,
            },
        }

    def test_output_modalities_round_trip(self):
        value = normalize({"providers": [self.provider], "models": [self.rich_model]})
        model = value["models"][0]
        self.assertEqual(model["output_modalities"], ["text", "audio"])
        self.assertIn("output_modalities", model["capability_sources"])
        self.assertEqual(model["capability_sources"]["output_modalities"]["source"], "manual")

    def test_output_modalities_defaults_to_text(self):
        value = normalize({"providers": [self.provider], "models": [self.model]})
        self.assertEqual(value["models"][0]["output_modalities"], ["text"])

    def test_output_modalities_preserves_non_codex_types(self):
        value = normalize({"providers": [self.provider], "models": [{
            **self.model,
            "output_modalities": ["text", "audio", "video", "file", "pdf"],
        }]})
        self.assertEqual(
            value["models"][0]["output_modalities"],
            ["text", "audio", "video", "file", "pdf"],
        )

    def test_supported_protocols_round_trip(self):
        value = normalize({"providers": [self.provider], "models": [self.rich_model]})
        model = value["models"][0]
        self.assertEqual(model["supported_protocols"], ["responses", "chat_completions"])
        self.assertIn("supported_protocols", model["capability_sources"])

    def test_supported_protocols_empty_when_absent(self):
        value = normalize({"providers": [self.provider], "models": [self.model]})
        self.assertEqual(value["models"][0]["supported_protocols"], [])

    def test_supported_protocols_excludes_auto(self):
        value = normalize({"providers": [self.provider], "models": [{
            **self.model,
            "supported_protocols": ["auto", "responses"],
        }]})
        self.assertEqual(value["models"][0]["supported_protocols"], ["responses"])

    def test_max_input_tokens_round_trip(self):
        value = normalize({"providers": [self.provider], "models": [self.rich_model]})
        self.assertEqual(value["models"][0]["max_input_tokens"], 100000)
        self.assertIn("max_input_tokens", value["models"][0]["capability_sources"])

    def test_reasoning_control_round_trip(self):
        value = normalize({"providers": [self.provider], "models": [self.rich_model]})
        self.assertEqual(
            value["models"][0]["reasoning_control"],
            "reasoning.effort enum: low, high",
        )
        self.assertIn("reasoning_control", value["models"][0]["capability_sources"])

    def test_structured_output_and_web_search_in_capabilities(self):
        value = normalize({"providers": [self.provider], "models": [self.rich_model]})
        caps = value["models"][0]["capabilities"]
        self.assertEqual(caps["structured_output"], True)
        self.assertEqual(caps["web_search"], False)

    def test_web_update_gives_manual_provenance_to_changed_new_fields(self):
        current = normalize({"providers": [self.provider], "models": [self.rich_model]})
        incoming = public_config(current)
        incoming["models"][0]["output_modalities"] = ["text"]
        incoming["models"][0]["supported_protocols"] = ["chat_completions"]
        incoming["models"][0]["max_input_tokens"] = 50000
        incoming["models"][0]["reasoning_control"] = "new control"
        updated = merge_web_update(current, incoming)
        sources = updated["models"][0]["capability_sources"]
        self.assertEqual(sources["output_modalities"]["source"], "manual")
        self.assertEqual(sources["supported_protocols"]["source"], "manual")
        self.assertEqual(sources["max_input_tokens"]["source"], "manual")
        self.assertEqual(sources["reasoning_control"]["source"], "manual")

    def test_web_update_preserves_unchanged_new_fields(self):
        current = normalize({"providers": [self.provider], "models": [self.rich_model]})
        incoming = public_config(current)
        updated = merge_web_update(current, incoming)
        model = updated["models"][0]
        self.assertEqual(model["output_modalities"], ["text", "audio"])
        self.assertEqual(model["supported_protocols"], ["responses", "chat_completions"])
        self.assertEqual(model["max_input_tokens"], 100000)
        self.assertEqual(model["reasoning_control"], "reasoning.effort enum: low, high")

    def test_web_update_preserves_new_fields_when_omitted(self):
        current = normalize({"providers": [self.provider], "models": [self.rich_model]})
        incoming = public_config(current)
        del incoming["models"][0]["output_modalities"]
        del incoming["models"][0]["supported_protocols"]
        del incoming["models"][0]["max_input_tokens"]
        del incoming["models"][0]["reasoning_control"]
        updated = merge_web_update(current, incoming)
        model = updated["models"][0]
        self.assertEqual(model["output_modalities"], ["text", "audio"])
        self.assertEqual(model["supported_protocols"], ["responses", "chat_completions"])
        self.assertEqual(model["max_input_tokens"], 100000)

    def test_web_partial_model_edit_preserves_discovery_metadata_and_provenance(self):
        observed_at = "2026-08-22T00:00:00+00:00"
        current = normalize({
            "providers": [self.provider],
            "models": [{
                **self.rich_model,
                "deployment_identity": "production",
                "resolved_protocol": "responses",
                "protocol_observation": {
                    "source": "observed",
                    "confidence": 1,
                    "observed_at": observed_at,
                    "endpoint_fingerprint": endpoint_fingerprint(
                        self.provider["base_url"]
                    ),
                    "deployment_identity": "production",
                    "upstream_model": "deepseek-chat",
                },
                "capability_sources": {
                    "output_limit": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": observed_at,
                    },
                    "structured_output": {
                        "source": "observed",
                        "confidence": 1,
                        "observed_at": observed_at,
                    },
                },
            }],
        })
        model = current["models"][0]
        incoming = public_config(current)
        incoming["models"] = [{
            "id": model["id"],
            "provider": model["provider"],
            "upstream_id": model["upstream_id"],
            "display_name": "Edited name",
            "context_window": model["context_window"],
            "reasoning_levels": model["reasoning_levels"],
            "enabled": model["enabled"],
        }]

        updated = merge_web_update(current, incoming)
        updated_model = updated["models"][0]

        self.assertEqual(updated_model["output_limit"], 4096)
        self.assertEqual(updated_model["deployment_identity"], "production")
        self.assertEqual(updated_model["resolved_protocol"], "responses")
        self.assertEqual(
            updated_model["protocol_observation"], model["protocol_observation"]
        )
        self.assertEqual(
            updated_model["capability_sources"]["output_limit"]["source"],
            "advertised",
        )
        self.assertEqual(
            updated_model["capability_sources"]["structured_output"]["source"],
            "observed",
        )

    def test_save_load_round_trips_new_fields(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            value = normalize({"providers": [self.provider], "models": [self.rich_model]})
            save(value, path)
            loaded = load(path)
            model = loaded["models"][0]
            self.assertEqual(model["output_modalities"], ["text", "audio"])
            self.assertEqual(model["supported_protocols"], ["responses", "chat_completions"])
            self.assertEqual(model["max_input_tokens"], 100000)
            self.assertEqual(model["reasoning_control"], "reasoning.effort enum: low, high")
            self.assertEqual(model["capabilities"]["structured_output"], True)
            self.assertEqual(model["capabilities"]["web_search"], False)

    def test_negative_max_input_tokens_rejected(self):
        with self.assertRaises(ConfigError):
            normalize({
                "providers": [self.provider],
                "models": [{**self.model, "max_input_tokens": -1}],
            })


class TestNestedCapabilityDataFlow(ConfigTests):
    """Tests for nested capabilities under model['capabilities']."""

    def setUp(self):
        super().setUp()
        self.cap_model = {
            **self.model,
            "capabilities": {
                "streaming": True,
                "structured_output": True,
                "web_search": False,
            },
        }

    def test_nested_capabilities_get_capability_sources(self):
        value = normalize({"providers": [self.provider], "models": [self.cap_model]})
        sources = value["models"][0]["capability_sources"]
        self.assertIn("structured_output", sources)
        self.assertIn("web_search", sources)
        self.assertEqual(sources["structured_output"]["source"], "manual")
        self.assertEqual(sources["web_search"]["source"], "manual")

    def test_nested_capabilities_not_duplicated_at_top_level(self):
        value = normalize({"providers": [self.provider], "models": [self.cap_model]})
        model = value["models"][0]
        self.assertNotIn("structured_output", model)
        self.assertNotIn("web_search", model)
        self.assertIn("structured_output", model["capabilities"])
        self.assertIn("web_search", model["capabilities"])

    def test_merge_preserves_omitted_capabilities_object(self):
        current = normalize({"providers": [self.provider], "models": [self.cap_model]})
        incoming = public_config(current)
        del incoming["models"][0]["capabilities"]
        updated = merge_web_update(current, incoming)
        caps = updated["models"][0]["capabilities"]
        self.assertEqual(caps["streaming"], True)
        self.assertEqual(caps["structured_output"], True)
        self.assertEqual(caps["web_search"], False)

    def test_merge_preserves_individual_omitted_nested_capability(self):
        current = normalize({"providers": [self.provider], "models": [{
            **self.cap_model,
            "capabilities": {
                "streaming": True,
                "structured_output": True,
                "web_search": False,
            },
        }]})
        incoming = public_config(current)
        del incoming["models"][0]["capabilities"]["web_search"]
        updated = merge_web_update(current, incoming)
        caps = updated["models"][0]["capabilities"]
        self.assertIn("web_search", caps)
        self.assertEqual(caps["web_search"], False)

    def test_changed_nested_capability_gets_fresh_manual_provenance(self):
        current = normalize({"providers": [self.provider], "models": [self.cap_model]})
        incoming = public_config(current)
        incoming["models"][0]["capabilities"]["structured_output"] = False
        updated = merge_web_update(current, incoming)
        sources = updated["models"][0]["capability_sources"]
        self.assertEqual(sources["structured_output"]["source"], "manual")
        self.assertEqual(updated["models"][0]["capabilities"]["structured_output"], False)

    def test_unchanged_nested_capability_retains_prior_provenance(self):
        current = normalize({
            "providers": [self.provider],
            "models": [{
                **self.model,
                "capabilities": {"structured_output": True, "web_search": False},
                "capability_sources": {
                    "structured_output": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                    "web_search": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                },
            }],
        })
        incoming = public_config(current)
        incoming["models"][0]["capabilities"]["structured_output"] = False
        updated = merge_web_update(current, incoming)
        sources = updated["models"][0]["capability_sources"]
        self.assertEqual(sources["structured_output"]["source"], "manual")
        self.assertEqual(sources["web_search"]["source"], "advertised")

    def test_official_provenance_survives_normalize(self):
        value = normalize({
            "providers": [self.provider],
            "models": [{
                **self.model,
                "capabilities": {"structured_output": True},
                "capability_sources": {
                    "structured_output": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                },
            }],
        })
        sources = value["models"][0]["capability_sources"]
        self.assertEqual(sources["structured_output"]["source"], "official")
        self.assertEqual(sources["structured_output"]["confidence"], 0.95)
        self.assertEqual(
            sources["structured_output"]["observed_at"],
            "2026-08-22T00:00:00+00:00",
        )

    def test_output_modalities_absent_yields_unknown_capability_value(self):
        from easy_multi_provider.capabilities import capability_record

        value = normalize({"providers": [self.provider], "models": [self.model]})
        model = value["models"][0]
        self.assertEqual(model["output_modalities"], ["text"])
        self.assertEqual(
            model["capability_sources"]["output_modalities"]["source"],
            "unknown",
        )
        record = capability_record(
            value["providers"][0], model
        ).to_dict()
        cap = record["capabilities"]["output_modalities"]
        self.assertEqual(cap["value"], "unknown")
        self.assertEqual(cap["source"], "unknown")


class ConfigTransactionTests(unittest.TestCase):
    def test_failed_config_commit_restores_existing_provider_key(self):
        provider = {
            "id": "deepseek",
            "name": "DeepSeek",
            "base_url": "https://api.deepseek.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "OLD",
        }
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "config.json"
            original = normalize({
                "secret_store_path": str(Path(directory) / "secrets"),
                "providers": [provider],
            })
            save(original, path)
            updated = load(path)
            updated["providers"][0]["api_key"] = "NEW"
            with patch.object(
                config_module,
                "_replace_config",
                side_effect=OSError("config commit failed"),
            ):
                with self.assertRaises(OSError):
                    save(updated, path)

            self.assertEqual(api_key(load(path)["providers"][0]), "OLD")


if __name__ == "__main__":
    unittest.main()
