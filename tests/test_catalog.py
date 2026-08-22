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

            self.assertEqual(models["gpt-native"]["display_name"], "Native [258K]")
            self.assertEqual(
                models["gpt-native"]["description"], "Native model · Context 258K"
            )
            self.assertEqual(
                models["primary/gpt-native"]["display_name"],
                "Primary · Native [258K]",
            )
            self.assertEqual(
                models["primary/gpt-native"]["description"],
                "ChatGPT subscription: Primary · Context 258K",
            )
            self.assertEqual(models["demo/large"]["display_name"], "Large [1.05M]")
            self.assertEqual(
                models["demo/large"]["description"],
                "External provider model · Context 1.05M",
            )
            self.assertEqual(models["demo/unknown"]["display_name"], "Unknown")
            self.assertEqual(
                models["demo/unknown"]["description"], "External provider model"
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
                        "hidden_models": ["gpt-hidden-by-current-login"],
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
                    "gpt-native",
                    "gpt-hidden-by-current-login",
                    "unique/gpt-native",
                    "unique/gpt-hidden-by-current-login",
                ],
            )
            by_slug = {item["slug"]: item for item in catalog["models"]}
            self.assertEqual(by_slug["gpt-native"].get("visibility", "list"), "list")
            self.assertEqual(
                by_slug["gpt-hidden-by-current-login"]["visibility"],
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


if __name__ == "__main__":
    unittest.main()
