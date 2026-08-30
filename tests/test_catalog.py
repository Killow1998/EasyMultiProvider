import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key
from easy_multi_provider.accounts import public_accounts
from easy_multi_provider.catalog import (
    build_catalog,
    generated_catalog_path,
    subscription_model_options,
    write_catalog,
)
from easy_multi_provider.config import normalize
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class CatalogTests(unittest.TestCase):
    def test_family_presentation_renames_native_and_prefixed_sources_once(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-5.6-sol",
                                "family_id": "gpt-5.6-sol",
                                "display_name": "GPT-5.6-Sol",
                                "context_window": 258000,
                                "visibility": "list",
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "accounts": [
                        {
                            "id": "ship",
                            "name": "🚢",
                            "prefix": "ship",
                            "auth_file": str(Path(directory) / "ship.enc"),
                        }
                    ],
                    "providers": [
                        {
                            "id": "or",
                            "name": "OpenRouter",
                            "base_url": "https://openrouter.ai/api/v1",
                        }
                    ],
                    "models": [
                        {
                            "id": "or/gpt-5.6-sol",
                            "provider": "or",
                            "upstream_id": "gpt-5.6-sol",
                            "context_window": 258000,
                        }
                    ],
                    "catalog_family_presentations": {
                        "gpt-5.6-sol": {
                            "catalog_alias": "将军",
                            "show_context": False,
                        }
                    },
                }
            )

            by_slug = {
                model["slug"]: model for model in build_catalog(config)["models"]
            }

        self.assertEqual(by_slug["gpt-5.6-sol"]["display_name"], "将军")
        self.assertEqual(by_slug["ship/gpt-5.6-sol"]["display_name"], "🚢 · 将军")
        self.assertEqual(
            by_slug["or/gpt-5.6-sol"]["display_name"], "OpenRouter · 将军"
        )

    def test_native_visibility_is_owned_without_importing_a_duplicate_account(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {"slug": "shown", "visibility": "list"},
                            {"slug": "hidden-by-user", "visibility": "list"},
                            {"slug": "codex-auto-review", "visibility": "hide"},
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "native_hidden_models": [
                        "hidden-by-user",
                        "codex-auto-review",
                    ],
                }
            )

            slugs = {model["slug"] for model in build_catalog(config)["models"]}

        self.assertIn("shown", slugs)
        self.assertNotIn("hidden-by-user", slugs)
        self.assertIn("codex-auto-review", slugs)
    def test_reasoning_summary_policy_never_fabricates_support(self):
        config = normalize(
            {
                "providers": [
                    {
                        "id": "api",
                        "base_url": "https://example.com/v1",
                        "protocol": "responses",
                    },
                    {
                        "id": "chat",
                        "base_url": "https://chat.example.com/v1",
                        "protocol": "chat_completions",
                    },
                ],
                "models": [
                    {
                        "id": "api/auto",
                        "provider": "api",
                        "upstream_id": "auto",
                        "supports_reasoning_summaries": True,
                    },
                    {
                        "id": "api/show",
                        "provider": "api",
                        "upstream_id": "show",
                        "supports_reasoning_summaries": True,
                    },
                    {
                        "id": "api/hide",
                        "provider": "api",
                        "upstream_id": "hide",
                        "supports_reasoning_summaries": True,
                    },
                    {
                        "id": "api/unsupported",
                        "provider": "api",
                        "upstream_id": "unsupported",
                        "supports_reasoning_summaries": False,
                    },
                    {
                        "id": "chat/claimed",
                        "provider": "chat",
                        "upstream_id": "claimed",
                        "supports_reasoning_summaries": True,
                    },
                ],
                "catalog_presentations": {
                    "api/show": {"reasoning_summary": "show"},
                    "api/hide": {"reasoning_summary": "hide"},
                    "api/unsupported": {"reasoning_summary": "show"},
                },
            }
        )

        by_slug = {
            model["slug"]: model for model in build_catalog(config)["models"]
        }

        self.assertTrue(by_slug["api/auto"]["supports_reasoning_summary_parameter"])
        self.assertEqual(by_slug["api/auto"]["default_reasoning_summary"], "auto")
        self.assertEqual(by_slug["api/show"]["default_reasoning_summary"], "auto")
        self.assertEqual(by_slug["api/hide"]["default_reasoning_summary"], "none")
        self.assertFalse(
            by_slug["api/unsupported"]["supports_reasoning_summary_parameter"]
        )
        self.assertEqual(
            by_slug["api/unsupported"]["default_reasoning_summary"], "none"
        )
        self.assertFalse(
            by_slug["chat/claimed"]["supports_reasoning_summary_parameter"]
        )
        self.assertEqual(
            by_slug["chat/claimed"]["default_reasoning_summary"], "none"
        )

    def test_route_presentations_apply_alias_and_context_without_changing_identity(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [{
                "slug": "gpt-native",
                "display_name": "Native Model",
                "context_window": 258000,
                "base_instructions": "Be useful",
                "model_messages": {},
            }]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "accounts": [{
                    "id": "account-a",
                    "prefix": "account-a",
                    "auth_file": "/tmp/account-a.enc",
                }],
                "providers": [{"id": "api", "base_url": "https://example.com/v1"}],
                "models": [{
                    "id": "api/gpt-native",
                    "provider": "api",
                    "upstream_id": "gpt-native",
                    "display_name": "Discovered Name",
                    "context_window": 258000,
                }],
                "catalog_presentations": {
                    "gpt-native": {
                        "catalog_alias": "General",
                        "show_context": False,
                    },
                    "account-a/gpt-native": {
                        "catalog_alias": "Reserve",
                        "show_context": True,
                    },
                    "api/gpt-native": {
                        "catalog_alias": "Worker",
                        "show_context": False,
                    },
                },
            })

            models = build_catalog(config)["models"]

        by_slug = {item["slug"]: item for item in models}
        self.assertEqual(by_slug["gpt-native"]["display_name"], "General")
        self.assertEqual(
            by_slug["gpt-native"]["description"], "General"
        )
        self.assertEqual(
            by_slug["account-a/gpt-native"]["display_name"], "[ 258K]  Reserve"
        )
        self.assertEqual(
            by_slug["account-a/gpt-native"]["description"],
            "Reserve · ChatGPT subscription: account-a · Context 258K",
        )
        self.assertEqual(by_slug["api/gpt-native"]["display_name"], "Worker")
        self.assertEqual(
            by_slug["api/gpt-native"]["description"],
            "Worker · Discovered Name",
        )
        self.assertEqual(by_slug["api/gpt-native"]["slug"], "api/gpt-native")
        self.assertEqual(config["models"][0]["upstream_id"], "gpt-native")

    def test_route_presentation_is_stable_when_applied_more_than_once(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-native",
                                "display_name": "Native",
                                "description": "Native model",
                                "context_window": 258000,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "catalog_presentations": {
                        "gpt-native": {
                            "catalog_alias": "General",
                            "show_context": True,
                        }
                    },
                }
            )

            first = build_catalog(config)["models"][0]
            second = build_catalog(config)["models"][0]

        self.assertEqual(first["slug"], "gpt-native")
        self.assertEqual(first["description"], "General · Native model · Context 258K")
        self.assertEqual(second["description"], first["description"])
        self.assertEqual(first["description"].count("General"), 1)
        self.assertEqual(first["description"].count("Context 258K"), 1)

    def test_user_alias_is_preserved_exactly_when_context_is_hidden(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-native",
                                "display_name": "Native Model [258K]",
                                "context_window": 258000,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "catalog_presentations": {
                        "gpt-native": {
                            "catalog_alias": "[258K] General",
                            "show_context": False,
                        }
                    },
                }
            )

            model = build_catalog(config)["models"][0]

        self.assertEqual(model["display_name"], "[258K] General")

    def test_catalog_display_names_include_only_known_usable_context(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-native",
                                "display_name": "Native",
                                "description": "Native model",
                                "context_window": 272000,
                                "effective_context_window_percent": 95,
                                "base_instructions": "Be useful",
                                "model_messages": {},
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "accounts": [
                        {
                            "id": "primary",
                            "name": "Primary",
                            "prefix": "primary",
                            "auth_file": "/tmp/primary-auth.json",
                        }
                    ],
                    "providers": [
                        {"id": "demo", "base_url": "https://example.com/v1"}
                    ],
                    "models": [
                        {
                            "id": "demo/large",
                            "display_name": "Large",
                            "provider": "demo",
                            "context_window": 1_048_576,
                        },
                        {
                            "id": "demo/unknown",
                            "display_name": "Unknown",
                            "provider": "demo",
                            "context_window": 0,
                        },
                    ],
                }
            )

            models = {item["slug"]: item for item in build_catalog(config)["models"]}

            self.assertEqual(models["gpt-native"]["display_name"], "[ 258K]  Native")
            self.assertEqual(
                models["gpt-native"]["description"], "Native model · Context 258K"
            )
            self.assertEqual(
                models["primary/gpt-native"]["display_name"],
                "[ 258K]  Primary · Native",
            )
            self.assertEqual(
                models["primary/gpt-native"]["description"],
                "ChatGPT subscription: Primary · Context 258K",
            )
            self.assertEqual(
                models["demo/large"]["display_name"], "[1.05M]  demo/large"
            )
            self.assertEqual(
                models["demo/large"]["description"], "Large · Context 1.05M"
            )
            self.assertEqual(
                models["demo/unknown"]["display_name"], "[    ?]  demo/unknown"
            )
            self.assertEqual(
                models["demo/unknown"]["description"], "Unknown"
            )
            self.assertNotIn("context_window", models["demo/unknown"])

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

    def test_external_model_is_delegatable_without_inheriting_agent_backend(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [{
                "slug": "gpt-native",
                "display_name": "Native",
                "base_instructions": "Be useful",
                "model_messages": {},
                "multi_agent_version": "disabled",
            }]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
                "models": [{"id": "demo/worker", "provider": "demo"}],
            })

            model = build_catalog(config)["models"][-1]

            self.assertEqual(model["slug"], "demo/worker")
            self.assertTrue(model["supported_in_api"])
            self.assertIsNone(model["multi_agent_version"])
            self.assertEqual(model["supported_reasoning_levels"], [])

    def test_external_model_preserves_confirmed_parallel_tool_capability(self):
        config = normalize(
            {
                "native_catalog_path": "/nonexistent",
                "providers": [
                    {"id": "demo", "base_url": "https://example.com/v1"}
                ],
                "models": [
                    {
                        "id": "demo/model",
                        "provider": "demo",
                        "capabilities": {"parallel_tools": True},
                    }
                ],
            }
        )

        model = build_catalog(config)["models"][0]

        self.assertTrue(model["supports_parallel_tool_calls"])

    def test_generated_catalog_path_is_stable_below_codex_home(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory) / "codex"
            self.assertEqual(
                generated_catalog_path(codex_home),
                (codex_home / "easy-multi-provider" / "catalog.json").resolve(),
            )

    def test_generated_catalog_path_uses_code_home_without_cwd(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory) / "codex"
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}):
                self.assertEqual(
                    generated_catalog_path(),
                    (codex_home / "easy-multi-provider" / "catalog.json").resolve(),
                )

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
                    {"id": "primary", "name": "Primary", "prefix": "primary", "auth_file": "/tmp/primary-auth.json"},
                    {"id": "plus", "name": "Plus", "prefix": "secondary", "auth_file": "/tmp/plus-auth.json"},
                ],
            })
            slugs = [item["slug"] for item in build_catalog(config)["models"]]
            self.assertEqual(slugs, ["gpt-native", "primary/gpt-native", "secondary/gpt-native"])

    def test_subscription_aliases_respect_native_and_account_visibility(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [
                {
                    "slug": "gpt-current",
                    "display_name": "Current",
                    "visibility": "list",
                    "supported_in_api": True,
                },
                {
                    "slug": "gpt-optional",
                    "display_name": "Optional",
                    "visibility": "list",
                    "supported_in_api": True,
                },
                {
                    "slug": "codex-auto-review",
                    "display_name": "Codex Auto Review",
                    "visibility": "hide",
                    "supported_in_api": True,
                },
            ]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "accounts": [{
                    "id": "primary",
                    "prefix": "primary",
                    "auth_file": "/tmp/primary-auth.json",
                    "hidden_models": ["gpt-optional"],
                }],
            })

            catalog = build_catalog(config)
            native_models = {
                item["slug"]: item
                for item in catalog["models"]
                if not item["slug"].startswith("primary/")
            }
            self.assertEqual(native_models["codex-auto-review"]["visibility"], "hide")
            aliases = [item["slug"] for item in catalog["models"] if item["slug"].startswith("primary/")]
            self.assertEqual(aliases, ["primary/gpt-current"])
            self.assertEqual(
                [item["id"] for item in subscription_model_options(config)],
                ["gpt-current", "gpt-optional"],
            )

    def test_generated_catalog_omits_native_models_not_supported_in_api(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [
                {
                    "slug": "gpt-supported",
                    "display_name": "Supported",
                    "visibility": "list",
                    "supported_in_api": True,
                },
                {
                    "slug": "gpt-picker-only",
                    "display_name": "Picker only",
                    "visibility": "list",
                    "supported_in_api": False,
                },
            ]}), encoding="utf-8")
            config = normalize({"native_catalog_path": str(native_path)})

            slugs = [item["slug"] for item in build_catalog(config)["models"]]

            self.assertEqual(slugs, ["gpt-supported"])

    def test_duplicate_subscription_is_marked_and_filtered_from_catalog(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native_path = root / "native.json"
            native_path.write_text(json.dumps({"models": [
                {
                    "slug": "gpt-native",
                    "display_name": "Native",
                    "base_instructions": "Be useful",
                    "model_messages": {},
                },
                {
                    "slug": "gpt-hidden-by-current-login",
                    "display_name": "Hidden by current login",
                    "base_instructions": "Be useful",
                    "model_messages": {},
                },
                {
                    "slug": "codex-auto-review",
                    "display_name": "Codex Auto Review",
                    "visibility": "hide",
                    "supported_in_api": True,
                    "base_instructions": "Review code",
                    "model_messages": {},
                },
            ]}), encoding="utf-8")
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
                    {
                        "id": "same",
                        "prefix": "same",
                        "auth_file": str(duplicate_path),
                        "hidden_models": [
                            "gpt-hidden-by-current-login",
                            "codex-auto-review",
                        ],
                    },
                    {"id": "unique", "prefix": "unique", "auth_file": str(unique_path)},
                ],
            })
            with patch.dict(os.environ, {"CODEX_HOME": str(codex_home)}):
                catalog = build_catalog(config)
                accounts = public_accounts(config["accounts"])
            self.assertEqual(
                [item["slug"] for item in catalog["models"]],
                [
                    "codex-auto-review",
                    "unique/gpt-hidden-by-current-login",
                    "gpt-native",
                    "unique/gpt-native",
                ],
            )
            by_slug = {item["slug"]: item for item in catalog["models"]}
            self.assertEqual(by_slug["gpt-native"].get("visibility", "list"), "list")
            self.assertEqual(
                by_slug["codex-auto-review"]["visibility"],
                "hide",
            )
            self.assertEqual(
                by_slug["unique/gpt-hidden-by-current-login"].get("visibility", "list"),
                "list",
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

    def test_external_models_group_by_provider_order_and_newest_first(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text('{"models": []}', encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "providers": [
                    {"id": "second", "base_url": "https://second.example/v1"},
                    {"id": "first", "base_url": "https://first.example/v1"},
                ],
                "models": [
                    {"id": "first/old", "provider": "first", "created_at": 10},
                    {"id": "second/old", "provider": "second", "created_at": 10},
                    {"id": "first/new", "provider": "first", "created_at": 20},
                    {"id": "second/new", "provider": "second", "created_at": 20},
                ],
            })

            slugs = [model["slug"] for model in build_catalog(config)["models"]]

        self.assertEqual(
            slugs,
            ["second/new", "first/new", "second/old", "first/old"],
        )

    def test_unknown_cross_provider_suffixes_are_not_one_family(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text('{"models": []}', encoding="utf-8")
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "providers": [
                        {"id": "first", "base_url": "https://first.example/v1"},
                        {"id": "second", "base_url": "https://second.example/v1"},
                    ],
                    "models": [
                        {
                            "id": "first/shared",
                            "provider": "first",
                            "created_at": 10,
                        },
                        {
                            "id": "second/shared",
                            "provider": "second",
                            "created_at": 20,
                        },
                    ],
                }
            )

            slugs = [model["slug"] for model in build_catalog(config)["models"]]

        self.assertEqual(slugs, ["second/shared", "first/shared"])

    def test_family_first_order_uses_exact_identity_and_source_priority(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(json.dumps({"models": [
                {
                    "slug": "model-new",
                    "display_name": "New",
                    "created_at": 20,
                },
                {
                    "slug": "model-old",
                    "display_name": "Old",
                    "created_at": 10,
                },
            ]}), encoding="utf-8")
            config = normalize({
                "native_catalog_path": str(native_path),
                "accounts": [{
                    "id": "account-a",
                    "prefix": "account-a",
                    "auth_file": "/tmp/account-a.enc",
                }],
                "providers": [
                    {"id": "provider-a", "base_url": "https://a.example/v1"},
                    {"id": "provider-b", "base_url": "https://b.example/v1"},
                ],
                "models": [
                    {
                        "id": "provider-b/model-new",
                        "provider": "provider-b",
                        "upstream_id": "model-new",
                        "created_at": 20,
                    },
                    {
                        "id": "provider-a/model-new",
                        "provider": "provider-a",
                        "upstream_id": "model-new",
                        "created_at": 20,
                    },
                    {
                        "id": "provider-a/model-new-preview",
                        "provider": "provider-a",
                        "upstream_id": "model-new-preview",
                        "created_at": 30,
                    },
                ],
            })

            slugs = [item["slug"] for item in build_catalog(config)["models"]]

        self.assertEqual(
            slugs,
            [
                "provider-a/model-new-preview",
                "model-new",
                "account-a/model-new",
                "provider-a/model-new",
                "provider-b/model-new",
                "model-old",
                "account-a/model-old",
            ],
        )


if __name__ == "__main__":
    unittest.main()
