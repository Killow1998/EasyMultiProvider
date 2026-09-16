"""Encrypted, portable ``.emp`` configuration migration bundles."""

from __future__ import annotations

import base64
import copy
import json
import os
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from cryptography.fernet import Fernet, InvalidToken
from cryptography.hazmat.primitives.kdf.scrypt import Scrypt

from .accounts import (
    account_auth_path,
    load_auth,
    normalize_account,
    same_account_auth,
    validate_auth_json,
)
from .config import api_key, load, normalize, save
from .catalog import load_native_catalog, model_family_identity
from .vault import file_transaction, write_encrypted_json


MAGIC = b"EMP-MIGRATION\x01\n"
SCHEMA = "easy-multi-provider-migration"
VERSION = 1
EXPORT_GROUPS = frozenset({"native", "subscriptions", "external"})
MAX_BUNDLE_BYTES = 32 * 1024 * 1024
MIN_PASSWORD_BYTES = 8
MAX_PASSWORD_BYTES = 4096
MAX_MIGRATION_REQUEST_BYTES = 4 * ((MAX_BUNDLE_BYTES + 2) // 3) + 64 * 1024
_SALT_BYTES = 16
_SCRYPT_N = 2**14
_SCRYPT_R = 8
_SCRYPT_P = 1


class MigrationError(ValueError):
    """Raised when an encrypted migration bundle is invalid or incomplete."""


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")


def _password_bytes(password: Any) -> bytes:
    if not isinstance(password, str):
        raise MigrationError("migration password is required")
    value = password.strip()
    encoded = value.encode("utf-8")
    if len(encoded) < MIN_PASSWORD_BYTES:
        raise MigrationError("migration password must contain at least 8 bytes")
    if len(encoded) > MAX_PASSWORD_BYTES:
        raise MigrationError("migration password is too long")
    return encoded


def _fernet(password: Any, salt: bytes) -> Fernet:
    key = Scrypt(
        salt=salt,
        length=32,
        n=_SCRYPT_N,
        r=_SCRYPT_R,
        p=_SCRYPT_P,
    ).derive(_password_bytes(password))
    return Fernet(base64.urlsafe_b64encode(key))


def _b64(value: bytes) -> str:
    return base64.urlsafe_b64encode(value).decode("ascii")


def _unb64(value: Any, field: str) -> bytes:
    if not isinstance(value, str) or not value:
        raise MigrationError("migration bundle field is invalid: %s" % field)
    try:
        return base64.b64decode(value.encode("ascii"), altchars=b"-_", validate=True)
    except (UnicodeEncodeError, ValueError) as exc:
        raise MigrationError("migration bundle field is invalid: %s" % field) from exc


def _portable_config(config: Dict[str, Any]) -> Dict[str, Any]:
    result = copy.deepcopy(config)
    result["host"] = "127.0.0.1"
    result["native_catalog_path"] = "~/.codex/models_cache.json"
    result["account_store_path"] = "state/accounts"
    result["secret_store_path"] = "state/secrets"
    result["accounts"] = []
    for account in config.get("accounts", []):
        metadata = normalize_account(account)
        metadata.pop("auth_file", None)
        result["accounts"].append(metadata)
    result["providers"] = []
    for provider in config.get("providers", []):
        portable = copy.deepcopy(provider)
        portable.pop("api_key_file", None)
        portable["api_key"] = ""
        result["providers"].append(portable)
    return result


def select_export_config(config: Dict[str, Any], groups: Any = None) -> Dict[str, Any]:
    """Keep selected categories and their route/display dependencies only."""
    if groups is None:
        selected = EXPORT_GROUPS
    elif (isinstance(groups, list) and groups
          and all(isinstance(group, str) and group in EXPORT_GROUPS for group in groups)):
        selected = set(groups)
    else:
        raise MigrationError("select at least one valid export category")
    result = copy.deepcopy(config)
    if selected == EXPORT_GROUPS:
        return result
    providers = {item["id"]: item for item in config.get("providers", [])}
    provider_groups = {key: "native" if value.get("auth_mode") == "forward" else "external"
                       for key, value in providers.items()}
    account_prefixes = {item["prefix"] for item in config.get("accounts", [])}
    models = {item["id"]: item for item in config.get("models", [])}
    native_models = (load_native_catalog(config)["models"]
                     if selected & {"native", "subscriptions"} else [])
    native_slugs = {item.get("slug") for item in native_models if isinstance(item, dict)}

    def route_group(route):
        if route in models:
            return provider_groups.get(models[route]["provider"])
        prefix = route.split("/", 1)[0]
        if prefix in account_prefixes:
            return "subscriptions"
        if prefix in provider_groups:
            return provider_groups[prefix]
        return "native" if "/" not in route or route in native_slugs else None

    result["accounts"] = result.get("accounts", []) if "subscriptions" in selected else []
    result["providers"] = [item for item in result.get("providers", []) if provider_groups[item["id"]] in selected]
    result["models"] = [item for item in result.get("models", []) if route_group(item["id"]) in selected]
    result["catalog_presentations"] = {route: value for route, value in result.get("catalog_presentations", {}).items()
                                       if route_group(route) in selected}
    families = {model_family_identity(item, item["id"]) for item in result["models"]}
    if selected & {"native", "subscriptions"}:
        families.update(model_family_identity(item, item.get("slug", ""))
                        for item in native_models if isinstance(item, dict))
        # Preserve subscription family settings even when its local model cache
        # is absent; known external-only families stay out of this category.
        external_families = {model_family_identity(item, item["id"]) for item in models.values()
                             if provider_groups.get(item["provider"]) == "external"}
        families.update(set(result.get("catalog_family_presentations", {})) - external_families)
    result["catalog_family_presentations"] = {family: value for family, value in result.get("catalog_family_presentations", {}).items()
                                              if family in families}
    if "native" not in selected:
        result["native_hidden_models"] = []
        result["native_model_context_windows"] = {}
    if not selected & {"native", "subscriptions"}:
        result["subscription_search"] = {"enabled": False, "account_id": ""}
    return result


def export_bundle(config: Dict[str, Any], config_path: Path, password: Any, groups: Any = None,
                  native_auth_path: Optional[Path] = None) -> bytes:
    return export_bundle_with_summary(config, config_path, password, groups, native_auth_path)[0]


def export_bundle_with_summary(config: Dict[str, Any], config_path: Path, password: Any,
                               groups: Any = None, native_auth_path: Optional[Path] = None
                               ) -> Tuple[bytes, Dict[str, Any]]:
    """Return an encrypted bundle without writing any plaintext credential."""
    _password_bytes(password)
    config = select_export_config(config, groups)
    accounts = []
    native_requested = groups is None or "native" in groups
    native_included = False
    for raw in config.get("accounts", []):
        account = normalize_account(raw)
        if not account["auth_file"]:
            raise MigrationError("account credentials are unavailable: %s" % account["id"])
        try:
            auth = load_auth(account)
        except Exception as exc:
            raise MigrationError("account credentials are unavailable: %s" % account["id"]) from exc
        metadata = dict(account)
        metadata.pop("auth_file", None)
        accounts.append({"metadata": metadata, "auth": auth})

    if native_auth_path is not None and (groups is None or "native" in groups):
        try:
            auth = json.loads(Path(native_auth_path).read_text(encoding="utf-8-sig"))
            auth = validate_auth_json(auth)
        except FileNotFoundError:
            auth = None
        except Exception as exc:
            raise MigrationError("Native login credentials are unavailable or invalid") from exc
        if auth is not None:
            native_included = True
            # Use the existing portable account format. Importing never replaces
            # the destination machine's current Codex login.
            reserved = {item["id"] for item in config.get("providers", [])}
            reserved.update(item["id"] for item in config.get("accounts", []))
            reserved.update(item["prefix"] for item in config.get("accounts", []))
            account_id = "native-login"
            suffix = 2
            while account_id in reserved:
                account_id = "native-login-%d" % suffix
                suffix += 1
            account = normalize_account({"id": account_id, "prefix": account_id,
                "name": "Native login", "hidden_models": config.get("native_hidden_models", []),
                "model_context_windows": config.get("native_model_context_windows", {})})
            config["accounts"].append(account)
            metadata = dict(account)
            metadata.pop("auth_file", None)
            accounts.append({"metadata": metadata, "auth": auth})

    provider_keys = {}
    for provider in config.get("providers", []):
        value = api_key(provider)
        if provider.get("api_key_file") and not value:
            raise MigrationError("Provider credentials are unavailable: %s" % provider["id"])
        if value:
            provider_keys[provider["id"]] = value

    payload = {
        "schema": SCHEMA,
        "version": VERSION,
        "config": _portable_config(config),
        "accounts": accounts,
        "provider_keys": provider_keys,
    }
    plaintext = _json_bytes(payload)
    salt = os.urandom(_SALT_BYTES)
    encrypted = _fernet(password, salt).encrypt(plaintext)
    envelope = {
        "schema": SCHEMA,
        "version": VERSION,
        "kdf": "scrypt",
        "scrypt": {"n": _SCRYPT_N, "r": _SCRYPT_R, "p": _SCRYPT_P},
        "salt": _b64(salt),
        "payload": _b64(encrypted),
    }
    result = MAGIC + _json_bytes(envelope) + b"\n"
    if len(result) > MAX_BUNDLE_BYTES:
        raise MigrationError("migration bundle is too large")
    return result, {
        "accounts": len(accounts), "providers": len(config.get("providers", [])),
        "models": len(config.get("models", [])),
        "groups": sorted(EXPORT_GROUPS if groups is None else set(groups)),
        "native_login_included": native_included,
        "native_login_missing": native_requested and not native_included,
    }


def _validate_payload(payload: Any) -> Dict[str, Any]:
    if not isinstance(payload, dict) or payload.get("schema") != SCHEMA or payload.get("version") != VERSION:
        raise MigrationError("unsupported migration bundle")
    raw_config = payload.get("config")
    if not isinstance(raw_config, dict):
        raise MigrationError("migration configuration is invalid")
    try:
        source_config = normalize(raw_config)
    except Exception as exc:
        raise MigrationError("migration configuration is invalid") from exc

    records = payload.get("accounts")
    if not isinstance(records, list):
        raise MigrationError("migration accounts are invalid")
    account_ids = set()
    for record in records:
        if not isinstance(record, dict):
            raise MigrationError("migration account record is invalid")
        try:
            metadata = normalize_account(record.get("metadata"))
            validate_auth_json(record.get("auth"))
        except Exception as exc:
            raise MigrationError("migration account record is invalid") from exc
        if metadata["id"] in account_ids:
            raise MigrationError("migration account IDs must be unique")
        account_ids.add(metadata["id"])

    provider_keys = payload.get("provider_keys", {})
    if not isinstance(provider_keys, dict):
        raise MigrationError("migration Provider keys are invalid")
    provider_ids = {item["id"] for item in source_config.get("providers", [])}
    if set(provider_keys) - provider_ids:
        raise MigrationError("migration contains a key for an unknown Provider")
    if any(not isinstance(value, str) for value in provider_keys.values()):
        raise MigrationError("migration Provider keys are invalid")

    return {
        "config": source_config,
        "accounts": records,
        "provider_keys": provider_keys,
    }


def read_bundle(bundle: bytes, password: Any) -> Dict[str, Any]:
    """Decrypt and validate a bundle before any local file is changed."""
    if not isinstance(bundle, bytes) or len(bundle) > MAX_BUNDLE_BYTES:
        raise MigrationError("migration bundle is too large")
    if not bundle.startswith(MAGIC):
        raise MigrationError("file is not a supported .emp migration bundle")
    try:
        envelope = json.loads(bundle[len(MAGIC):].decode("utf-8"))
        if not isinstance(envelope, dict):
            raise MigrationError("migration envelope is invalid")
        if envelope.get("schema") != SCHEMA or envelope.get("version") != VERSION:
            raise MigrationError("unsupported migration bundle")
        if envelope.get("kdf") != "scrypt":
            raise MigrationError("unsupported migration encryption")
        salt = _unb64(envelope.get("salt"), "salt")
        if len(salt) != _SALT_BYTES:
            raise MigrationError("migration salt is invalid")
        encrypted = _unb64(envelope.get("payload"), "payload")
        plaintext = _fernet(password, salt).decrypt(encrypted)
        payload = json.loads(plaintext.decode("utf-8"))
    except MigrationError:
        raise
    except (UnicodeDecodeError, ValueError, InvalidToken) as exc:
        raise MigrationError("migration password is incorrect or file is invalid") from exc
    return _validate_payload(payload)


def import_bundle(
    current: Dict[str, Any], bundle: bytes, password: Any, config_path: Path
) -> Tuple[Dict[str, Any], Dict[str, int]]:
    """Merge a bundle into local config and re-encrypt credentials locally."""
    payload = read_bundle(bundle, password)
    source = payload["config"]
    target = copy.deepcopy(current)

    providers = {item["id"]: item for item in target.get("providers", [])}
    for raw in source.get("providers", []):
        provider = copy.deepcopy(raw)
        provider["api_key"] = payload["provider_keys"].get(provider["id"], "")
        provider["api_key_file"] = ""
        providers[provider["id"]] = provider
    target["providers"] = list(providers.values())

    models = {item["id"]: item for item in target.get("models", [])}
    for model in source.get("models", []):
        models[model["id"]] = copy.deepcopy(model)
    target["models"] = list(models.values())

    family_presentations = copy.deepcopy(
        target.get("catalog_family_presentations", {})
    )
    family_presentations.update(
        copy.deepcopy(source.get("catalog_family_presentations", {}))
    )
    target["catalog_family_presentations"] = family_presentations
    target["native_hidden_models"] = sorted(
        set(target.get("native_hidden_models", ()))
        | set(source.get("native_hidden_models", ()))
    )

    accounts = {item["id"]: item for item in target.get("accounts", [])}
    imported_auth = {}
    prefix_map = {}
    renamed_accounts = 0
    reserved = {item["id"] for item in target["providers"]}
    for account in list(accounts.values()) + [record["metadata"] for record in payload["accounts"]]:
        reserved.update((account["id"], account["prefix"]))

    def unique_segment(value):
        suffix = 2
        while True:
            ending = "-%d" % suffix
            candidate = value[:64 - len(ending)] + ending
            if candidate not in reserved:
                reserved.add(candidate)
                return candidate
            suffix += 1

    for record in payload["accounts"]:
        metadata = normalize_account(record["metadata"])
        auth = validate_auth_json(record["auth"])
        source_prefix = metadata["prefix"]
        existing = accounts.get(metadata["id"])
        same_account = False
        if existing is not None:
            # A previous import may already have renamed this source ID.
            # Reimporting that account should update it, not create another copy.
            candidates = [existing] + [item for item in accounts.values() if item is not existing]
            for candidate in candidates:
                try:
                    same_account = same_account_auth(load_auth(candidate), auth)
                except ValueError:
                    continue
                if same_account:
                    existing = candidate
                    metadata["id"] = existing["id"]
                    break
            if same_account:
                metadata["prefix"] = existing["prefix"]
            else:
                metadata["id"] = unique_segment(metadata["id"])
                metadata["prefix"] = metadata["id"]
                renamed_accounts += 1
        if not same_account:
            occupied_prefixes = {item["prefix"] for item in accounts.values()}
            occupied_prefixes.update(item["id"] for item in target["providers"])
            if metadata["prefix"] in occupied_prefixes:
                metadata["prefix"] = unique_segment(metadata["prefix"])
                if existing is None:
                    renamed_accounts += 1
        reserved.update((metadata["id"], metadata["prefix"]))
        prefix_map[source_prefix] = metadata["prefix"]
        imported_auth[metadata["id"]] = auth
        metadata["auth_file"] = ""
        accounts[metadata["id"]] = metadata
    target["accounts"] = list(accounts.values())
    presentations = copy.deepcopy(target.get("catalog_presentations", {}))
    for route, value in source.get("catalog_presentations", {}).items():
        prefix, separator, slug = route.partition("/")
        destination = prefix_map.get(prefix, prefix) + separator + slug if separator else route
        presentations[destination] = copy.deepcopy(value)
    target["catalog_presentations"] = presentations
    target = normalize(target)
    for account in target["accounts"]:
        if account["id"] in imported_auth:
            account["auth_file"] = str(account_auth_path(target, account["id"], config_path))
    target = normalize(target)

    with file_transaction() as transaction:
        for account_id, auth in imported_auth.items():
            path = account_auth_path(target, account_id, config_path)
            transaction.remember(path)
            write_encrypted_json(path, auth)
        save(target, config_path, _transaction=transaction)
        result = load(config_path)

    summary = {
        "accounts": len(imported_auth),
        "providers": len(source.get("providers", [])),
        "models": len(source.get("models", [])),
    }
    if renamed_accounts:
        summary["renamed_accounts"] = renamed_accounts
    return result, summary
