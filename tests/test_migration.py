import json
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider.accounts import import_account, load_auth
from easy_multi_provider.config import api_key, load, normalize, save
import easy_multi_provider.migration as migration_module
from easy_multi_provider.migration import MigrationError, export_bundle, import_bundle, read_bundle


class MigrationTests(unittest.TestCase):
    def test_current_import_accepts_a_v0_9_0_style_bundle(self):
        """Version-1 bundles remain readable when newer config fields are absent."""

        password = "legacy-migration-pass"
        salt = b"v0.9.0-fixture!!"
        payload = {
            "schema": migration_module.SCHEMA,
            "version": 1,
            "config": {
                "host": "127.0.0.1",
                "port": 4200,
                "native_catalog_path": "~/.codex/models_cache.json",
                "account_store_path": "state/accounts",
                "secret_store_path": "state/secrets",
                "codex_base_url": "http://127.0.0.1:4200/v1",
                "accounts": [],
                "providers": [
                    {
                        "id": "legacy",
                        "name": "Legacy",
                        "base_url": "https://example.com/v1",
                        "api_key": "",
                    }
                ],
                "models": [{"id": "legacy/model", "provider": "legacy"}],
                "catalog_presentations": {
                    "legacy/model": {"catalog_alias": "Legacy model"}
                },
                "subscription_search": {"enabled": False, "account_id": ""},
            },
            "accounts": [],
            "provider_keys": {"legacy": "legacy-provider-secret"},
        }
        encrypted = migration_module._fernet(password, salt).encrypt(
            migration_module._json_bytes(payload)
        )
        envelope = {
            "schema": migration_module.SCHEMA,
            "version": 1,
            "kdf": "scrypt",
            "scrypt": {"n": 2**14, "r": 8, "p": 1},
            "salt": migration_module._b64(salt),
            "payload": migration_module._b64(encrypted),
        }
        bundle = (
            migration_module.MAGIC
            + json.dumps(envelope, separators=(",", ":")).encode("utf-8")
            + b"\n"
        )

        with tempfile.TemporaryDirectory() as directory:
            target_path = Path(directory) / "config.json"
            key = Fernet.generate_key().decode("ascii")
            with patch.dict(
                os.environ,
                {"EASY_MULTI_PROVIDER_MASTER_KEY": key},
                clear=False,
            ):
                save(normalize({}), target_path)
                imported, summary = import_bundle(
                    load(target_path), bundle, password, target_path
                )
                self.assertEqual(
                    summary, {"accounts": 0, "providers": 1, "models": 1}
                )
                self.assertEqual(
                    api_key(imported["providers"][0]), "legacy-provider-secret"
                )
                self.assertEqual(imported["models"][0]["id"], "legacy/model")
                self.assertEqual(
                    imported["catalog_presentations"]["legacy/model"][
                        "catalog_alias"
                    ],
                    "Legacy model",
                )
                self.assertEqual(imported["catalog_family_presentations"], {})
                self.assertEqual(imported["native_hidden_models"], [])

    def test_bundle_is_encrypted_and_round_trips_to_a_new_machine(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source = root / "source"
            source_config_path = source / "config.json"
            source_config = normalize(
                {
                    "account_store_path": str(source / "state" / "accounts"),
                    "secret_store_path": str(source / "state" / "secrets"),
                    "providers": [
                        {
                            "id": "demo",
                            "name": "Demo",
                            "base_url": "https://example.com/v1",
                            "api_key": "provider-secret",
                        }
                    ],
                    "models": [{"id": "demo/model", "provider": "demo"}],
                    "catalog_presentations": {
                        "demo/model": {
                            "catalog_alias": "Portable Worker",
                            "show_context": False,
                            "reasoning_summary": "hide",
                        }
                    },
                    "catalog_family_presentations": {
                        "shared-family": {
                            "catalog_alias": "General",
                            "show_context": True,
                            "reasoning_summary": "auto",
                        }
                    },
                    "native_hidden_models": ["gpt-hidden"],
                }
            )
            source_key = Fernet.generate_key().decode("ascii")
            with patch.dict(os.environ, {"EASY_MULTI_PROVIDER_MASTER_KEY": source_key}, clear=False):
                save(source_config, source_config_path)
                source_config = load(source_config_path)
                account = import_account(
                    source_config,
                    {"id": "account-a", "name": "Account A", "prefix": "account-a"},
                    {
                        "auth_mode": "chatgpt",
                        "tokens": {
                            "access_token": "account-access-secret",
                            "refresh_token": "account-refresh-secret",
                        },
                    },
                    source_config_path,
                )
                source_config["accounts"] = [account]
                save(source_config, source_config_path)
                source_config = load(source_config_path)
                bundle = export_bundle(source_config, source_config_path, "migration-pass-1")

            self.assertTrue(bundle.startswith(b"EMP-MIGRATION\x01\n"))
            self.assertNotIn(b"provider-secret", bundle)
            self.assertNotIn(b"account-access-secret", bundle)
            with self.assertRaises(MigrationError):
                read_bundle(bundle, "wrong-pass")
            self.assertEqual(len(read_bundle(bundle, "migration-pass-1")["accounts"]), 1)

            target = root / "target"
            target_config_path = target / "config.json"
            target_key = Fernet.generate_key().decode("ascii")
            self.assertNotEqual(source_key, target_key)
            with patch.dict(os.environ, {"EASY_MULTI_PROVIDER_MASTER_KEY": target_key}, clear=False):
                save(
                    normalize(
                        {
                            "port": 4299,
                            "codex_base_url": "http://127.0.0.1:4299/v1",
                        }
                    ),
                    target_config_path,
                )
                imported, summary = import_bundle(
                    load(target_config_path), bundle, "migration-pass-1", target_config_path
                )
                self.assertEqual(summary, {"accounts": 1, "providers": 1, "models": 1})
                self.assertEqual(imported["port"], 4299)
                self.assertEqual(imported["codex_base_url"], "http://127.0.0.1:4299/v1")
                self.assertEqual(api_key(imported["providers"][0]), "provider-secret")
                loaded = load(target_config_path)
                self.assertEqual(
                    loaded["catalog_presentations"]["demo/model"],
                    {
                        "catalog_alias": "Portable Worker",
                        "show_context": False,
                        "reasoning_summary": "hide",
                    },
                )
                self.assertEqual(
                    loaded["catalog_family_presentations"]["shared-family"][
                        "catalog_alias"
                    ],
                    "General",
                )
                self.assertEqual(loaded["native_hidden_models"], ["gpt-hidden"])
                self.assertEqual(api_key(loaded["providers"][0]), "provider-secret")
                self.assertEqual(load_auth(loaded["accounts"][0])["tokens"]["access_token"], "account-access-secret")
                auth_path = Path(loaded["accounts"][0]["auth_file"])
                self.assertNotIn(b"account-access-secret", auth_path.read_bytes())

    def test_import_merges_without_deleting_local_entries(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            source_path = root / "source.json"
            target_path = root / "target.json"
            key = Fernet.generate_key().decode("ascii")
            with patch.dict(os.environ, {"EASY_MULTI_PROVIDER_MASTER_KEY": key}, clear=False):
                source = normalize({"providers": [{"id": "source", "base_url": "https://example.com/v1"}]})
                target = normalize({"providers": [{"id": "local", "base_url": "https://example.com/v1"}]})
                save(source, source_path)
                save(target, target_path)
                bundle = export_bundle(load(source_path), source_path, "migration-pass-2")
                imported, _ = import_bundle(load(target_path), bundle, "migration-pass-2", target_path)
                self.assertEqual({p["id"] for p in imported["providers"]}, {"local", "source"})

    def test_failed_import_restores_existing_account_credential(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            key = Fernet.generate_key().decode("ascii")
            with patch.dict(
                os.environ,
                {"EASY_MULTI_PROVIDER_MASTER_KEY": key},
                clear=False,
            ):
                source_path = root / "source" / "config.json"
                source = normalize({
                    "account_store_path": str(root / "source" / "accounts")
                })
                save(source, source_path)
                source = load(source_path)
                source["accounts"] = [import_account(
                    source,
                    {"id": "same", "prefix": "same"},
                    {"auth_mode": "chatgpt", "tokens": {"access_token": "NEW"}},
                    source_path,
                )]
                save(source, source_path)
                bundle = export_bundle(load(source_path), source_path, "migration-pass-3")

                target_path = root / "target" / "config.json"
                target = normalize({
                    "account_store_path": str(root / "target" / "accounts")
                })
                save(target, target_path)
                target = load(target_path)
                target["accounts"] = [import_account(
                    target,
                    {"id": "same", "prefix": "same"},
                    {"auth_mode": "chatgpt", "tokens": {"access_token": "OLD"}},
                    target_path,
                )]
                save(target, target_path)

                with patch.object(
                    migration_module,
                    "save",
                    side_effect=RuntimeError("config commit failed"),
                ):
                    with self.assertRaises(RuntimeError):
                        import_bundle(
                            load(target_path),
                            bundle,
                            "migration-pass-3",
                            target_path,
                        )

                persisted = load(target_path)
                self.assertEqual(
                    load_auth(persisted["accounts"][0])["tokens"]["access_token"],
                    "OLD",
                )


if __name__ == "__main__":
    unittest.main()
