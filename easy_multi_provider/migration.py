"""Encrypted, portable ``.emp`` configuration migration bundles."""

from __future__ import annotations

import base64
import copy
import json
import os
from pathlib import Path
from typing import Any, Dict, Tuple

from cryptography.fernet import Fernet, InvalidToken
from cryptography.hazmat.primitives.kdf.scrypt import Scrypt

from .accounts import (
    account_auth_path,
    load_auth,
    normalize_account,
    validate_auth_json,
)
from .config import api_key, load, normalize, save
from .vault import write_encrypted_json


MAGIC = b"EMP-MIGRATION\x01\n"
SCHEMA = "easy-multi-provider-migration"
VERSION = 1
MAX_BUNDLE_BYTES = 32 * 1024 * 1024
MIN_PASSWORD_BYTES = 8
MAX_PASSWORD_BYTES = 4096
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


def export_bundle(config: Dict[str, Any], config_path: Path, password: Any) -> bytes:
    """Return an encrypted bundle without writing any plaintext credential."""
    _password_bytes(password)
    accounts = []
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
    return result


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

    accounts = {item["id"]: item for item in target.get("accounts", [])}
    imported_auth = {}
    for record in payload["accounts"]:
        metadata = normalize_account(record["metadata"])
        imported_auth[metadata["id"]] = validate_auth_json(record["auth"])
        metadata["auth_file"] = ""
        accounts[metadata["id"]] = metadata
    target["accounts"] = list(accounts.values())
    target = normalize(target)
    for account in target["accounts"]:
        if account["id"] in imported_auth:
            account["auth_file"] = str(account_auth_path(target, account["id"], config_path))
    target = normalize(target)

    created_auth_paths = []
    try:
        for account_id, auth in imported_auth.items():
            path = account_auth_path(target, account_id, config_path)
            if not path.exists():
                created_auth_paths.append(path)
            write_encrypted_json(path, auth)
        save(target, config_path)
    except Exception:
        for path in created_auth_paths:
            try:
                path.unlink()
            except OSError:
                pass
        raise

    result = load(config_path)
    return result, {
        "accounts": len(imported_auth),
        "providers": len(source.get("providers", [])),
        "models": len(source.get("models", [])),
    }
