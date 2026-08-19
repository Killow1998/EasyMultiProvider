import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider.accounts import import_account, load_auth
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.migration import MigrationError, export_bundle, import_bundle, read_bundle


class MigrationTests(unittest.TestCase):
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


if __name__ == "__main__":
    unittest.main()
