"""Configuration loading, validation, redaction, and persistence."""

from __future__ import annotations

import copy
import json
import os
import re
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import quote, urlparse

from .accounts import normalize_account, public_accounts
from .vault import VaultError, read_encrypted_text, write_encrypted_text


DEFAULT_CONFIG: Dict[str, Any] = {
    "host": "127.0.0.1",
    "port": 4200,
    "native_catalog_path": str(Path.home() / ".codex" / "models_cache.json"),
    "account_store_path": "state/accounts",
    "secret_store_path": "state/secrets",
    "codex_base_url": "https://chatgpt.com/backend-api/codex",
    "accounts": [],
    "providers": [],
    "models": [],
}

_ID = re.compile(r"^[A-Za-z0-9._/-]+$")
_PROTOCOLS = {"responses", "chat_completions", "anthropic_messages"}
_AUTH_MODES = {"api_key", "anthropic_api_key", "forward"}


class ConfigError(ValueError):
    """Raised when Web-supplied configuration is invalid."""


def config_path() -> Path:
    return Path(os.environ.get("EASY_MULTI_PROVIDER_CONFIG", "config.json"))


def _copy_default() -> Dict[str, Any]:
    return copy.deepcopy(DEFAULT_CONFIG)


def _string(value: Any, field: str = "value", required: bool = False) -> str:
    if value is None:
        value = ""
    if not isinstance(value, str):
        raise ConfigError("%s must be a string" % field)
    value = value.strip()
    if required and not value:
        raise ConfigError("%s is required" % field)
    return value


def _validate_id(value: Any, field: str) -> str:
    value = _string(value, field, required=True)
    if not _ID.match(value):
        raise ConfigError("%s contains unsupported characters" % field)
    return value


def _validate_url(value: Any, field: str) -> str:
    value = _string(value, field, required=True).rstrip("/")
    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ConfigError("%s must be an http(s) URL" % field)
    return value


def _normalize_provider(raw: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(raw, dict):
        raise ConfigError("each provider must be an object")
    provider = {
        "id": _validate_id(raw.get("id"), "provider.id"),
        "name": _string(raw.get("name")) or _string(raw.get("id"), "provider.id"),
        "base_url": _validate_url(raw.get("base_url"), "provider.base_url"),
        "protocol": _string(raw.get("protocol")) or "chat_completions",
        "auth_mode": _string(raw.get("auth_mode")) or "api_key",
        "api_key": _string(raw.get("api_key")),
        "api_key_file": _string(raw.get("api_key_file")),
        "anthropic_version": _string(raw.get("anthropic_version")) or "2023-06-01",
        "enabled": bool(raw.get("enabled", True)),
    }
    if provider["protocol"] not in _PROTOCOLS:
        raise ConfigError("provider.protocol must be responses, chat_completions, or anthropic_messages")
    if provider["auth_mode"] not in _AUTH_MODES:
        raise ConfigError("provider.auth_mode must be api_key, anthropic_api_key, or forward")
    if provider["auth_mode"] == "forward" and provider["protocol"] != "responses":
        raise ConfigError("forward providers must use the Responses protocol")
    return provider


def _normalize_model(raw: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(raw, dict):
        raise ConfigError("each model must be an object")
    levels = raw.get("reasoning_levels", ["medium"])
    if not isinstance(levels, list) or not levels:
        raise ConfigError("model.reasoning_levels must be a non-empty list")
    levels = [_string(level, "model.reasoning_levels") for level in levels]
    model = {
        "id": _validate_id(raw.get("id"), "model.id"),
        "provider": _validate_id(raw.get("provider"), "model.provider"),
        "upstream_id": _string(raw.get("upstream_id")),
        "display_name": _string(raw.get("display_name")),
        "description": _string(raw.get("description")),
        "reasoning_levels": levels,
        "context_window": int(raw.get("context_window", 0) or 0),
        "enabled": bool(raw.get("enabled", True)),
    }
    if model["context_window"] < 0:
        raise ConfigError("model.context_window cannot be negative")
    return model


def normalize(raw: Optional[Dict[str, Any]]) -> Dict[str, Any]:
    if raw is None:
        raw = _copy_default()
    if not isinstance(raw, dict):
        raise ConfigError("configuration must be an object")

    result = _copy_default()
    result["host"] = _string(raw.get("host", result["host"])) or "127.0.0.1"
    if result["host"] != "127.0.0.1":
        raise ConfigError("host must be 127.0.0.1 for local-only management")
    result["port"] = int(raw.get("port", result["port"]))
    result["native_catalog_path"] = _string(
        raw.get("native_catalog_path", result["native_catalog_path"])
    )
    result["account_store_path"] = _string(
        raw.get("account_store_path", result["account_store_path"])
    ) or "state/accounts"
    result["secret_store_path"] = _string(
        raw.get("secret_store_path", result["secret_store_path"])
    ) or "state/secrets"
    result["codex_base_url"] = _validate_url(
        raw.get("codex_base_url", result["codex_base_url"]), "codex_base_url"
    )
    if not 1 <= result["port"] <= 65535:
        raise ConfigError("port must be between 1 and 65535")

    accounts = [normalize_account(item) for item in raw.get("accounts", [])]
    account_ids = {item["id"] for item in accounts}
    account_prefixes = {item["prefix"] for item in accounts}
    if len(account_ids) != len(accounts):
        raise ConfigError("account ids must be unique")
    if len(account_prefixes) != len(accounts):
        raise ConfigError("account prefixes must be unique")
    providers = [_normalize_provider(item) for item in raw.get("providers", [])]
    models = [_normalize_model(item) for item in raw.get("models", [])]
    provider_ids = {item["id"] for item in providers}
    if len(provider_ids) != len(providers):
        raise ConfigError("provider ids must be unique")
    overlap = account_prefixes & provider_ids
    if overlap:
        raise ConfigError("account prefixes conflict with provider ids: %s" % ", ".join(sorted(overlap)))
    model_ids = {item["id"] for item in models}
    if len(model_ids) != len(models):
        raise ConfigError("model ids must be unique")
    missing = sorted({item["provider"] for item in models} - provider_ids)
    if missing:
        raise ConfigError("models reference unknown providers: %s" % ", ".join(missing))
    result["providers"] = providers
    result["models"] = models
    result["accounts"] = accounts
    return result


def load(path: Optional[Path] = None) -> Dict[str, Any]:
    path = Path(path or config_path())
    if not path.exists():
        return normalize(None)
    try:
        with path.open("r", encoding="utf-8") as handle:
            return normalize(json.load(handle))
    except json.JSONDecodeError as exc:
        raise ConfigError("invalid JSON in %s: %s" % (path, exc))


def save(config: Dict[str, Any], path: Optional[Path] = None) -> Path:
    path = Path(path or config_path())
    normalized = normalize(config)
    secret_root = Path(normalized["secret_store_path"]).expanduser()
    if not secret_root.is_absolute():
        secret_root = path.parent / secret_root
    for provider in normalized["providers"]:
        key = provider.get("api_key", "")
        if key and key != "••••••••":
            secret_root.mkdir(parents=True, exist_ok=True)
            os.chmod(str(secret_root), 0o700)
            secret_path = secret_root / (quote(provider["id"], safe="") + ".key.enc")
            try:
                write_encrypted_text(secret_path, key)
            except VaultError as exc:
                raise ConfigError(str(exc)) from exc
            provider["api_key_file"] = str(secret_path)
            provider["api_key"] = ""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".easy-multi-provider-", dir=str(path.parent))
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(normalized, handle, indent=2, ensure_ascii=False)
            handle.write("\n")
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return path


def merge_web_update(current: Dict[str, Any], incoming: Dict[str, Any]) -> Dict[str, Any]:
    """Apply a Web update while preserving secrets omitted by the UI."""
    if not isinstance(incoming, dict):
        raise ConfigError("request body must be an object")
    merged = copy.deepcopy(incoming)
    old_by_id = {item["id"]: item for item in current.get("providers", [])}
    for provider in merged.get("providers", []):
        old = old_by_id.get(provider.get("id"), {})
        if "api_key" not in provider or provider.get("api_key") == "••••••••":
            provider["api_key"] = old.get("api_key", "")
        if "api_key_file" not in provider and old.get("api_key_file"):
            provider["api_key_file"] = old["api_key_file"]
    old_accounts = {item["id"]: item for item in current.get("accounts", [])}
    for account in merged.get("accounts", []):
        old = old_accounts.get(account.get("id"), {})
        if not account.get("auth_file") and old.get("auth_file"):
            account["auth_file"] = old["auth_file"]
    return normalize(merged)


def public_config(config: Dict[str, Any]) -> Dict[str, Any]:
    """Return configuration safe to send to the browser."""
    result = copy.deepcopy(config)
    result["accounts"] = public_accounts(result.get("accounts", []))
    for provider in result["providers"]:
        key = provider.pop("api_key", "")
        secret_file = provider.pop("api_key_file", "")
        provider["api_key_set"] = bool(key or (secret_file and Path(secret_file).is_file()))
        provider["api_key"] = "••••••••" if key else ""
    return result


def api_key(provider: Dict[str, Any]) -> str:
    value = provider.get("api_key", "")
    if value:
        return value
    secret_file = provider.get("api_key_file", "")
    if secret_file:
        try:
            return read_encrypted_text(Path(secret_file)).strip()
        except (OSError, VaultError):
            return ""
    return ""
