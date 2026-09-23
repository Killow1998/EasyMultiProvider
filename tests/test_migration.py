import json
import itertools
import os
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider.accounts import import_account, load_auth
from easy_multi_provider.config import api_key, load, normalize, save
import easy_multi_provider.migration as migration_module
from easy_multi_provider.migration import MigrationError, export_bundle, export_bundle_with_summary, import_bundle, read_bundle


class MigrationTests(unittest.TestCase):
    def test_external_vision_capability_evidence_round_trips(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(
            os.environ,
            {"EASY_MULTI_PROVIDER_MASTER_KEY": Fernet.generate_key().decode("ascii")},
        ):
            root = Path(directory)
            source_path = root / "source" / "config.json"
            target_path = root / "target" / "config.json"
            source = normalize(
                {
                    "providers": [
                        {"id": "external", "base_url": "https://example.test/v1"}
                    ],
                    "models": [
                        {
                            "id": "external/vision",
                            "provider": "external",
                            "input_modalities": ["text", "image"],
                            "output_modalities": ["text"],
                            "supported_protocols": ["responses"],
                            "supports_image_detail_original": True,
                            "capabilities": {"structured_tools": True},
                            "capability_sources": {
                                "input_modalities": {"source": "manual"},
                                "output_modalities": {"source": "advertised"},
                                "supported_protocols": {"source": "observed"},
                                "supports_image_detail_original": {"source": "manual"},
                                "structured_tools": {"source": "advertised"},
                            },
                        }
                    ],
                }
            )
            save(source, source_path)
            bundle = export_bundle(
                load(source_path), source_path, "migration-pass", ["external"]
            )
            save(normalize({}), target_path)
            imported, _ = import_bundle(
                load(target_path), bundle, "migration-pass", target_path
            )
            model = imported["models"][0]
            self.assertEqual(model["input_modalities"], ["text", "image"])
            self.assertEqual(
                model["capability_sources"]["input_modalities"]["source"],
                "manual",
            )
            self.assertEqual(
                model["capability_sources"]["supported_protocols"]["source"],
                "observed",
            )
            self.assertTrue(model["supports_image_detail_original"])
            self.assertTrue(model["capabilities"]["structured_tools"])

    def test_native_imports_preserve_independent_accounts_and_update_reimports(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            "EASY_MULTI_PROVIDER_MASTER_KEY": Fernet.generate_key().decode("ascii"),
        }):
            root = Path(directory)
            target_path = root / "target" / "config.json"
            save(normalize({}), target_path)
            bundles = []
            for identity in ("account-A", "account-B"):
                native_path = root / (identity + ".json")
                native_path.write_text(json.dumps({"tokens": {"account_id": identity,
                    "access_token": identity + "-access"}}), encoding="utf-8")
                bundles.append(export_bundle(normalize({}), root / "source.json", "migration-pass", ["native"], native_path))
            first, _ = import_bundle(load(target_path), bundles[0], "migration-pass", target_path)
            original_file = Path(first["accounts"][0]["auth_file"])
            original_bytes = original_file.read_bytes()
            imported, summary = import_bundle(load(target_path), bundles[1], "migration-pass", target_path)
            self.assertEqual({a["id"] for a in imported["accounts"]}, {"native-login", "native-login-2"})
            self.assertEqual({load_auth(a)["tokens"]["account_id"] for a in imported["accounts"]}, {"account-A", "account-B"})
            self.assertEqual(summary["renamed_accounts"], 1)
            self.assertEqual(original_file.read_bytes(), original_bytes)
            imported, _ = import_bundle(load(target_path), bundles[1], "migration-pass", target_path)
            self.assertEqual(len(imported["accounts"]), 2)

    def test_account_conflicts_remap_presentations_and_preserve_unknown_identity(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            "EASY_MULTI_PROVIDER_MASTER_KEY": Fernet.generate_key().decode("ascii"),
        }):
            root = Path(directory)
            source_path = root / "source" / "config.json"
            target_path = root / "target" / "config.json"
            for identity in ("different", "unknown", "unreadable", "same", "prefix"):
                with self.subTest(identity=identity):
                    source = normalize({"catalog_presentations": {"route/model": {"catalog_alias": "Imported"}}})
                    save(source, source_path)
                    source = load(source_path)
                    source["accounts"] = [import_account(source, {"id": "shared", "prefix": "route"},
                        {"tokens": {"account_id": "account-B", "access_token": "new-access"}}, source_path)]
                    save(source, source_path)
                    bundle = export_bundle(load(source_path), source_path, "migration-pass", ["subscriptions"])
                    target = normalize({"catalog_presentations": {"local/model": {"catalog_alias": "Retained"}}})
                    save(target, target_path)
                    target = load(target_path)
                    auth = {"tokens": {"access_token": "old-access"}}
                    if identity != "unknown":
                        auth["tokens"]["account_id"] = "account-B" if identity == "same" else "account-A"
                    target["accounts"] = [import_account(target, {"id": "other" if identity == "prefix" else "shared",
                        "prefix": "route" if identity == "prefix" else "local"}, auth, target_path)]
                    save(target, target_path)
                    if identity == "unreadable":
                        Path(target["accounts"][0]["auth_file"]).write_bytes(b"corrupt fixture")
                    original_bytes = Path(target["accounts"][0]["auth_file"]).read_bytes()
                    imported, _ = import_bundle(load(target_path), bundle, "migration-pass", target_path)
                    self.assertEqual(len(imported["accounts"]), 1 if identity == "same" else 2)
                    new_account = next(a for a in imported["accounts"] if load_auth(a)["tokens"]["access_token"] == "new-access") if identity != "unreadable" else imported["accounts"][-1]
                    self.assertEqual(imported["catalog_presentations"][new_account["prefix"] + "/model"]["catalog_alias"], "Imported")
                    if identity != "same":
                        self.assertEqual(imported["catalog_presentations"]["local/model"]["catalog_alias"], "Retained")
                        self.assertEqual(Path(target["accounts"][0]["auth_file"]).read_bytes(), original_bytes)

    def test_export_summary_matches_credentials_and_reports_missing_native_login(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "auth.json"
            config = normalize({})
            bundle, summary = export_bundle_with_summary(config, root / "source.json", "migration-pass", ["native"], path)
            self.assertTrue(summary["native_login_missing"])
            self.assertFalse(summary["native_login_included"])
            self.assertEqual(summary["accounts"], len(read_bundle(bundle, "migration-pass")["accounts"]))
            path.write_text(json.dumps({"tokens": {"access_token": "fixture-native"}}))
            bundle, summary = export_bundle_with_summary(config, root / "source.json", "migration-pass", ["native"], path)
            self.assertTrue(summary["native_login_included"])
            self.assertFalse(summary["native_login_missing"])
            self.assertEqual(summary["accounts"], len(read_bundle(bundle, "migration-pass")["accounts"]))
            self.assertEqual(summary["groups"], ["native"])
            self.assertNotIn("fixture-native", json.dumps(summary))

    def test_native_login_exports_as_portable_account_without_replacing_current_login(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            "EASY_MULTI_PROVIDER_MASTER_KEY": Fernet.generate_key().decode("ascii"),
        }):
            root = Path(directory)
            native_path = root / "auth.json"
            auth = {"tokens": {"access_token": "native-access", "refresh_token": "native-refresh"}}
            native_path.write_text(json.dumps(auth), encoding="utf-8")
            config = normalize({"providers": [{"id": "native-login", "base_url": "https://example.test/v1", "auth_mode": "forward", "protocol": "responses"}],
                                "native_hidden_models": ["gpt-hidden"]})
            bundle = export_bundle(config, root / "source.json", "migration-pass", ["native"], native_path)
            payload = read_bundle(bundle, "migration-pass")
            self.assertEqual(payload["accounts"][0]["auth"], auth)
            self.assertEqual(payload["accounts"][0]["metadata"]["id"], "native-login-2")
            self.assertEqual(config["accounts"], [])
            self.assertNotIn(b"native-access", bundle)
            self.assertNotIn(b"native-refresh", bundle)
            target_path = root / "target" / "config.json"
            save(normalize({}), target_path)
            imported, summary = import_bundle(load(target_path), bundle, "migration-pass", target_path)
            self.assertEqual(summary["accounts"], 1)
            self.assertEqual(load_auth(imported["accounts"][0]), auth)
            self.assertEqual(imported["accounts"][0]["hidden_models"], ["gpt-hidden"])
            self.assertEqual(json.loads(native_path.read_text()), auth)
            native_path.write_text("invalid-json", encoding="utf-8")
            with self.assertRaises(MigrationError):
                export_bundle(config, root / "source.json", "migration-pass", ["native"], native_path)
            # An omitted Native category does not touch even a broken login file.
            omitted = read_bundle(export_bundle(config, root / "source.json", "migration-pass", ["external"], native_path), "migration-pass")
            self.assertEqual(omitted["accounts"], [])
            native_path.unlink()
            absent = read_bundle(export_bundle(config, root / "source.json", "migration-pass", ["native"], native_path), "migration-pass")
            self.assertEqual(absent["accounts"], [])

    def test_selected_categories_round_trip_without_unselected_credentials(self):
        with tempfile.TemporaryDirectory() as directory, patch.dict(os.environ, {
            "EASY_MULTI_PROVIDER_MASTER_KEY": Fernet.generate_key().decode("ascii"),
        }):
            root = Path(directory)
            source_path = root / "source" / "config.json"
            source = normalize({
                "account_store_path": str(root / "source" / "accounts"),
                "secret_store_path": str(root / "source" / "secrets"),
                "native_catalog_path": str(root / "missing-catalog.json"),
                "providers": [
                    {"id": "external", "base_url": "https://example.test/v1", "api_key": "external-secret"},
                    {"id": "bridge", "base_url": "https://native.test/v1", "auth_mode": "forward", "protocol": "responses"},
                ],
                "models": [
                    {"id": "external/flash", "provider": "external", "family_id": "gemini"},
                    {"id": "bridge/local", "provider": "bridge", "upstream_id": "gpt-x"},
                ],
                "native_hidden_models": ["gpt-hidden"],
                "catalog_presentations": {
                    "gpt-x": {"catalog_alias": "Native model"},
                    "other/gpt-x": {"catalog_alias": "Other model"},
                    "external/flash": {"catalog_alias": "External model"},
                    "bridge/local": {"catalog_alias": "Native bridge"},
                },
                "catalog_family_presentations": {
                    "gpt-x": {"show_context": False}, "gemini": {"show_context": True},
                },
            })
            save(source, source_path)
            source = load(source_path)
            source["accounts"] = [import_account(source, {"id": "other", "prefix": "other"},
                {"tokens": {"access_token": "subscription-secret"}}, source_path)]
            save(source, source_path)
            source = load(source_path)
            groups = ["native", "subscriptions", "external"]
            for size in range(1, 4):
                for selected in itertools.combinations(groups, size):
                    with self.subTest(selected=selected):
                        bundle = export_bundle(source, source_path, "migration-pass", list(selected))
                        payload = read_bundle(bundle, "migration-pass")
                        native = "native" in selected
                        subscriptions = "subscriptions" in selected
                        external = "external" in selected
                        self.assertEqual(len(payload["accounts"]), int(subscriptions))
                        self.assertEqual(set(payload["provider_keys"]), {"external"} if external else set())
                        self.assertEqual({p["id"] for p in payload["config"]["providers"]},
                                         ({"bridge"} if native else set()) | ({"external"} if external else set()))
                        expected_routes = ({"gpt-x", "bridge/local"} if native else set())
                        expected_routes |= {"other/gpt-x"} if subscriptions else set()
                        expected_routes |= {"external/flash"} if external else set()
                        self.assertEqual(set(payload["config"]["catalog_presentations"]), expected_routes)
                        self.assertEqual(payload["config"]["native_hidden_models"], ["gpt-hidden"] if native else [])
                        expected_families = ({"gpt-x"} if native or subscriptions else set()) | ({"gemini"} if external else set())
                        self.assertEqual(set(payload["config"]["catalog_family_presentations"]), expected_families)
                        self.assertNotIn(b"external-secret", bundle)
                        self.assertNotIn(b"subscription-secret", bundle)
                        target_path = root / ("target-" + "-".join(selected)) / "config.json"
                        current = normalize({
                            "providers": [{"id": "retained", "base_url": "https://retained.test/v1", "api_key": "retained-secret"}],
                            "models": [{"id": "retained/model", "provider": "retained"}],
                            "native_hidden_models": ["retained-hidden"],
                        })
                        save(current, target_path)
                        imported, summary = import_bundle(load(target_path), bundle, "migration-pass", target_path)
                        self.assertEqual(summary["accounts"], int(subscriptions))
                        retained = next(p for p in imported["providers"] if p["id"] == "retained")
                        self.assertEqual(api_key(retained), "retained-secret")
                        self.assertIn("retained/model", {m["id"] for m in imported["models"]})
                        self.assertIn("retained-hidden", imported["native_hidden_models"])
                        if subscriptions:
                            self.assertEqual(load_auth(imported["accounts"][0])["tokens"]["access_token"], "subscription-secret")

    def test_omitted_categories_never_read_credentials_and_invalid_selection_fails(self):
        config = normalize({
            "accounts": [{"id": "other", "prefix": "other", "auth_file": "unavailable-auth.json"}],
            "providers": [{"id": "external", "base_url": "https://example.test/v1", "api_key_file": "unavailable-key"}],
        })
        with patch.object(migration_module, "load_auth", side_effect=AssertionError("unexpected account read")), \
                patch.object(migration_module, "api_key", side_effect=AssertionError("unexpected key read")):
            payload = read_bundle(export_bundle(config, Path("config.json"), "migration-pass", ["native"]), "migration-pass")
            self.assertEqual(payload["accounts"], [])
            self.assertEqual(payload["provider_keys"], {})
        for selected in ([], "native", ["unknown"], [1], {}, ["native", "unknown"]):
            with self.subTest(selected=selected), self.assertRaises(MigrationError):
                export_bundle(config, Path("config.json"), "migration-pass", selected)

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
                    {"auth_mode": "chatgpt", "tokens": {"account_id": "same-account", "access_token": "NEW"}},
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
                    {"auth_mode": "chatgpt", "tokens": {"account_id": "same-account", "access_token": "OLD"}},
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
