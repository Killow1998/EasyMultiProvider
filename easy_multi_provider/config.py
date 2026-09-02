"""Configuration loading, validation, redaction, and persistence."""

from __future__ import annotations

import copy
import ipaddress
import json
import os
import re
import tempfile
from pathlib import Path
from typing import Any, Dict, List, Optional
from urllib.parse import quote, urlparse

from .accounts import (
    AccountError,
    canonicalize_account_paths,
    normalize_account,
    normalize_hidden_models,
    public_accounts,
)
from .capabilities import (
    input_modalities_known,
    make_provenance,
    normalize_input_modalities,
    normalize_output_modalities,
    normalize_reasoning_levels,
    normalize_supported_protocols,
    observed_at_now,
    output_modalities_known,
    supported_protocols_known,
)
from .vault import (
    FileTransaction,
    VaultError,
    file_transaction,
    read_encrypted_text,
    write_encrypted_text,
)


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
    "native_hidden_models": [],
    "catalog_presentations": {},
    "catalog_family_presentations": {},
    "subscription_search": {
        "enabled": False,
        "account_id": "",
    },
    "codex_runtime_sources": ["auto"],
}

_ID = re.compile(r"^[A-Za-z0-9._/:-]+$")
_PROVIDER_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
MAX_CONTEXT_WINDOW = 100_000_000
MAX_OUTPUT_LIMIT = MAX_CONTEXT_WINDOW
_PROTOCOLS = {"auto", "responses", "chat_completions", "anthropic_messages"}
_AUTH_MODES = {"api_key", "anthropic_api_key", "forward"}
_CAPABILITY_SOURCE_FIELDS = {
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "reasoning_levels",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "websocket",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
}
_EXPLICIT_CAPABILITY_FIELDS = {
    "supports_reasoning",
    "supports_reasoning_summaries",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "reasoning_control",
    "max_input_tokens",
    "structured_output",
    "web_search",
    "supports_image_detail_original",
}
_BOOLEAN_CAPABILITIES = {
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "supports_reasoning",
    "supports_reasoning_summaries",
    "websocket",
}
_SAFE_CAPABILITY_ID = re.compile(r"^[A-Za-z0-9._/:-]{1,256}$")
_ENDPOINT_FINGERPRINT = re.compile(r"^sha256:[0-9a-f]{64}$")
_CONCRETE_PROTOCOLS = {"responses", "chat_completions", "anthropic_messages"}
_REASONING_SUMMARY_POLICIES = {"auto", "show", "hide"}
_MAX_CATALOG_ALIAS_BYTES = 512
_CODEX_RUNTIME_SOURCES = {
    "auto",
    "configured",
    "codex_app",
    "managed",
    "vscode",
    "vscode_insiders",
    "cursor",
    "path_cli",
}


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


def _validate_provider_id(value: Any) -> str:
    value = _string(value, "provider.id", required=True)
    if not _PROVIDER_ID.fullmatch(value):
        raise ConfigError("provider.id must be a safe single path segment")
    return value


def _normalize_catalog_presentations(raw: Any) -> Dict[str, Dict[str, Any]]:
    if raw is None:
        return {}
    if not isinstance(raw, dict):
        raise ConfigError("catalog_presentations must be an object")
    result: Dict[str, Dict[str, Any]] = {}
    for raw_route, raw_value in raw.items():
        route = _validate_id(raw_route, "catalog_presentations route")
        if not isinstance(raw_value, dict):
            raise ConfigError("catalog presentation must be an object")
        alias = raw_value.get("catalog_alias", "")
        if not isinstance(alias, str):
            raise ConfigError("catalog_alias must be a string")
        if len(alias.encode("utf-8")) > _MAX_CATALOG_ALIAS_BYTES:
            raise ConfigError("catalog_alias is too long")
        if any(ord(character) < 32 or ord(character) == 127 for character in alias):
            raise ConfigError("catalog_alias contains unsupported characters")
        show_context = raw_value.get("show_context", True)
        if not isinstance(show_context, bool):
            raise ConfigError("show_context must be boolean")
        reasoning_summary = raw_value.get("reasoning_summary", "auto")
        if not isinstance(reasoning_summary, str):
            raise ConfigError("reasoning_summary must be a string")
        reasoning_summary = reasoning_summary.strip().lower()
        if reasoning_summary not in _REASONING_SUMMARY_POLICIES:
            raise ConfigError("reasoning_summary must be auto, show, or hide")
        result[route] = {
            "catalog_alias": alias,
            "show_context": show_context,
            "reasoning_summary": reasoning_summary,
        }
    return result


def _normalize_subscription_search(
    raw: Any, _account_ids: set[str]
) -> Dict[str, Any]:
    if raw is None:
        raw = {}
    if not isinstance(raw, dict):
        raise ConfigError("subscription_search must be an object")
    enabled = raw.get("enabled", False)
    if not isinstance(enabled, bool):
        raise ConfigError("subscription_search.enabled must be boolean")
    # Account selection is automatic. Keep the field in the public shape so
    # older clients can still round-trip the object, but discard legacy pinned
    # account IDs during normalization.
    return {"enabled": enabled, "account_id": ""}


def _validate_url(value: Any, field: str) -> str:
    value = _string(value, field, required=True).rstrip("/")
    parsed = urlparse(value)
    if parsed.scheme not in ("http", "https") or not parsed.netloc:
        raise ConfigError("%s must be an http(s) URL" % field)
    if parsed.username or parsed.password:
        raise ConfigError("%s must not contain URL credentials" % field)
    if parsed.query or parsed.fragment:
        raise ConfigError("%s must not contain a query or fragment" % field)
    try:
        hostname = (parsed.hostname or "").lower()
        loopback = hostname == "localhost" or ipaddress.ip_address(hostname).is_loopback
    except ValueError:
        loopback = False
    if parsed.scheme == "http" and not loopback:
        raise ConfigError("%s must use HTTPS unless it targets loopback" % field)
    return value


def _normalize_codex_runtime_sources(value: Any) -> List[str]:
    if value is None:
        return ["auto"]
    if not isinstance(value, list) or not value:
        raise ConfigError("codex_runtime_sources must be a non-empty list")
    if len(value) > len(_CODEX_RUNTIME_SOURCES):
        raise ConfigError("codex_runtime_sources has too many entries")
    sources = []
    for index, raw_source in enumerate(value):
        source = _string(raw_source, "codex_runtime_sources[%d]" % index)
        if source not in _CODEX_RUNTIME_SOURCES:
            raise ConfigError("codex_runtime_sources contains an unsupported source")
        if source not in sources:
            sources.append(source)
    if "auto" in sources and len(sources) != 1:
        raise ConfigError("codex_runtime_sources auto cannot be combined")
    return sources


def _normalize_capabilities(raw: Any, field: str) -> Dict[str, bool]:
    """Keep only explicitly configured boolean capability facts."""

    if raw is None:
        return {}
    if not isinstance(raw, dict):
        raise ConfigError("%s must be an object" % field)
    result: Dict[str, bool] = {}
    for name in _BOOLEAN_CAPABILITIES:
        if name not in raw:
            continue
        value = raw[name]
        if not isinstance(value, bool):
            raise ConfigError("%s.%s must be boolean" % (field, name))
        result[name] = value
    return result


def _capability_known(field: str, value: Any) -> bool:
    if field in _BOOLEAN_CAPABILITIES:
        return isinstance(value, bool)
    if field == "reasoning_levels":
        return isinstance(value, list) and bool(value)
    if field == "reasoning_control":
        return isinstance(value, str) and bool(value.strip())
    if field == "input_modalities":
        return input_modalities_known(value)
    if field == "output_modalities":
        return output_modalities_known(value)
    if field == "supported_protocols":
        return supported_protocols_known(value)
    if field == "supports_image_detail_original":
        return isinstance(value, bool)
    return isinstance(value, int) and not isinstance(value, bool) and value > 0


def _effective_capability_value(values: Dict[str, Any], field: str) -> Any:
    """Return the value for a capability field from top-level or nested capabilities."""
    if field in values:
        return values[field]
    caps = values.get("capabilities")
    if isinstance(caps, dict) and field in caps:
        return caps[field]
    return None


def _default_capability_source(
    field: str, values: Dict[str, Any], explicit_fields: set
) -> str:
    if field in _EXPLICIT_CAPABILITY_FIELDS:
        return "manual" if field in explicit_fields else "unknown"
    effective = _effective_capability_value(values, field)
    return "inferred" if _capability_known(field, effective) else "unknown"


def _normalize_capability_sources(
    raw: Any, values: Dict[str, Any], explicit_fields: Optional[set] = None
) -> Dict[str, Dict[str, Any]]:
    if raw is None:
        raw = {}
    if not isinstance(raw, dict):
        raise ConfigError("model.capability_sources must be an object")
    explicit_fields = explicit_fields or set()
    result: Dict[str, Dict[str, Any]] = {}
    for field in _CAPABILITY_SOURCE_FIELDS:
        if field not in raw:
            effective = _effective_capability_value(values, field)
            if _capability_known(field, effective):
                result[field] = make_provenance(
                    _default_capability_source(field, values, explicit_fields)
                )
            continue
        value = raw[field]
        if not isinstance(value, dict):
            raise ConfigError("model.capability_sources.%s must be an object" % field)
        default_source = _default_capability_source(field, values, explicit_fields)
        source = value.get("source", default_source)
        try:
            provenance = make_provenance(
                source,
                value.get("confidence"),
                value.get("observed_at"),
            )
        except (TypeError, ValueError) as exc:
            raise ConfigError("invalid provenance for model.%s" % field) from exc
        result[field] = provenance
    return result


def _safe_capability_identity(value: Any, field: str) -> str:
    value = _string(value, field)
    if value and not _SAFE_CAPABILITY_ID.fullmatch(value):
        raise ConfigError("%s contains unsupported characters" % field)
    return value


def _normalize_protocol_observation(raw: Any) -> Dict[str, Any]:
    if raw is None:
        raw = {}
    if not isinstance(raw, dict):
        raise ConfigError("protocol_observation must be an object")
    source = raw.get("source", "unknown")
    try:
        result = make_provenance(
            source,
            raw.get("confidence"),
            raw.get("observed_at"),
        )
    except (TypeError, ValueError) as exc:
        raise ConfigError("invalid protocol_observation") from exc
    fingerprint = _string(raw.get("endpoint_fingerprint"), "protocol_observation.endpoint_fingerprint")
    if fingerprint and not _ENDPOINT_FINGERPRINT.fullmatch(fingerprint):
        raise ConfigError("protocol_observation.endpoint_fingerprint is invalid")
    result["endpoint_fingerprint"] = fingerprint
    result["deployment_identity"] = _safe_capability_identity(
        raw.get("deployment_identity"), "protocol_observation.deployment_identity"
    )
    result["upstream_model"] = _safe_capability_identity(
        raw.get("upstream_model"), "protocol_observation.upstream_model"
    )
    return result


def _normalize_context_calibrations(raw: Any) -> list:
    """Keep only bounded numeric calibration facts with safe identity keys."""

    if raw is None:
        return []
    if not isinstance(raw, list):
        raise ConfigError("model.context_calibrations must be a list")
    result = []
    for item in raw[:8]:
        if not isinstance(item, dict):
            raise ConfigError("model.context_calibrations entries must be objects")
        fingerprint = _string(
            item.get("endpoint_fingerprint"),
            "model.context_calibrations.endpoint_fingerprint",
        )
        if not _ENDPOINT_FINGERPRINT.fullmatch(fingerprint):
            raise ConfigError("model.context_calibrations endpoint fingerprint is invalid")
        protocol = _string(item.get("protocol"), "model.context_calibrations.protocol")
        if protocol not in _CONCRETE_PROTOCOLS:
            raise ConfigError("model.context_calibrations protocol is invalid")
        upstream = _safe_capability_identity(
            item.get("upstream_model"),
            "model.context_calibrations.upstream_model",
        )
        deployment = _safe_capability_identity(
            item.get("deployment_identity"),
            "model.context_calibrations.deployment_identity",
        )
        if not upstream or not deployment:
            raise ConfigError("model.context_calibrations identity is required")
        clean = {
            "endpoint_fingerprint": fingerprint,
            "upstream_model": upstream,
            "protocol": protocol,
            "deployment_identity": deployment,
        }
        for name in ("largest_success_estimate", "smallest_failure_estimate"):
            value = item.get(name)
            if value in (None, ""):
                clean[name] = None
                continue
            try:
                value = int(value)
            except (TypeError, ValueError):
                raise ConfigError("model.context_calibrations.%s must be numeric" % name)
            if isinstance(value, bool) or value <= 0 or value > MAX_CONTEXT_WINDOW:
                raise ConfigError("model.context_calibrations.%s is out of range" % name)
            clean[name] = value
        for name, default in (
            ("largest_success_source", "unknown"),
            ("smallest_failure_source", "unknown"),
        ):
            source = _string(item.get(name, default), "model.context_calibrations.%s" % name)
            if source not in ("observed", "unknown"):
                raise ConfigError("model.context_calibrations.%s is invalid" % name)
            clean[name] = source
        for name in ("largest_success_confidence", "smallest_failure_confidence"):
            value = item.get(name, 1.0 if "largest" in name and clean["largest_success_estimate"] else 1.0 if "smallest" in name and clean["smallest_failure_estimate"] else 0.0)
            try:
                value = float(value)
            except (TypeError, ValueError):
                raise ConfigError("model.context_calibrations.%s must be numeric" % name)
            if not 0.0 <= value <= 1.0:
                raise ConfigError("model.context_calibrations.%s is out of range" % name)
            clean[name] = value
        for name in ("largest_success_observed_at", "smallest_failure_observed_at"):
            value = item.get(name)
            if value is not None:
                try:
                    make_provenance("observed", observed_at=value)
                except (TypeError, ValueError) as exc:
                    raise ConfigError("model.context_calibrations.%s is invalid" % name) from exc
            clean[name] = value
        result.append(clean)
    return result


def _resolved_protocol(raw: Any) -> str:
    value = _string(raw, "resolved_protocol")
    if value and value not in _PROTOCOLS - {"auto"}:
        raise ConfigError("resolved_protocol must be a concrete protocol")
    return value


def _normalize_provider(raw: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(raw, dict):
        raise ConfigError("each provider must be an object")
    provider = {
        "id": _validate_provider_id(raw.get("id")),
        "name": _string(raw.get("name")) or _string(raw.get("id"), "provider.id"),
        "base_url": _validate_url(raw.get("base_url"), "provider.base_url"),
        "protocol": _string(raw.get("protocol")) or "chat_completions",
        "auth_mode": _string(raw.get("auth_mode")) or "api_key",
        "api_key": _string(raw.get("api_key")),
        "api_key_file": _string(raw.get("api_key_file")),
        "anthropic_version": _string(raw.get("anthropic_version")) or "2023-06-01",
        "enabled": bool(raw.get("enabled", True)),
        "deployment_identity": _safe_capability_identity(
            raw.get("deployment_identity"), "provider.deployment_identity"
        ),
        "resolved_protocol": _resolved_protocol(raw.get("resolved_protocol")),
        "protocol_observation": _normalize_protocol_observation(
            raw.get("protocol_observation")
        ),
        "capabilities": _normalize_capabilities(
            raw.get("capabilities"), "provider.capabilities"
        ),
    }
    if provider["protocol"] not in _PROTOCOLS:
        raise ConfigError("provider.protocol must be auto, responses, chat_completions, or anthropic_messages")
    if provider["auth_mode"] not in _AUTH_MODES:
        raise ConfigError("provider.auth_mode must be api_key, anthropic_api_key, or forward")
    if provider["auth_mode"] == "forward" and provider["protocol"] != "responses":
        raise ConfigError("forward providers must use the Responses protocol")
    return provider


def _normalize_model(raw: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(raw, dict):
        raise ConfigError("each model must be an object")
    levels = raw.get("reasoning_levels", [])
    if not isinstance(levels, list):
        raise ConfigError("model.reasoning_levels must be a list")
    levels = [_string(level, "model.reasoning_levels") for level in levels]
    if any(not level for level in levels):
        raise ConfigError("model.reasoning_levels entries must not be empty")
    levels = normalize_reasoning_levels(levels)
    raw_reasoning_support = raw.get("supports_reasoning")
    if raw_reasoning_support is None:
        supports_reasoning = True if levels else None
    elif isinstance(raw_reasoning_support, bool):
        supports_reasoning = raw_reasoning_support
    else:
        raise ConfigError("model.supports_reasoning must be boolean or null")
    raw_summary_support = raw.get("supports_reasoning_summaries")
    if raw_summary_support is None:
        supports_reasoning_summaries = None
    elif isinstance(raw_summary_support, bool):
        supports_reasoning_summaries = raw_summary_support
    else:
        raise ConfigError(
            "model.supports_reasoning_summaries must be boolean or null"
        )
    context_window = int(raw.get("context_window", 0) or 0)
    if context_window < 0:
        raise ConfigError("model.context_window cannot be negative")
    if context_window > MAX_CONTEXT_WINDOW:
        raise ConfigError("model.context_window is too large")
    output_limit = int(
        raw.get("output_limit", raw.get("output_token_limit", 0)) or 0
    )
    if output_limit < 0:
        raise ConfigError("model.output_limit cannot be negative")
    if output_limit > MAX_OUTPUT_LIMIT:
        raise ConfigError("model.output_limit is too large")
    created_at = int(raw.get("created_at", 0) or 0)
    if created_at < 0:
        raise ConfigError("model.created_at cannot be negative")
    visibility = _string(raw.get("visibility"), "model.visibility") or "list"
    if visibility not in {"list", "hide"}:
        raise ConfigError("model.visibility must be list or hide")
    supports_image_detail_original = raw.get("supports_image_detail_original", False)
    if not isinstance(supports_image_detail_original, bool):
        supports_image_detail_original = False
    max_input_tokens = int(raw.get("max_input_tokens", 0) or 0)
    if max_input_tokens < 0:
        raise ConfigError("model.max_input_tokens cannot be negative")
    if max_input_tokens > MAX_CONTEXT_WINDOW:
        raise ConfigError("model.max_input_tokens is too large")
    reasoning_control = _string(raw.get("reasoning_control"))
    output_modalities = normalize_output_modalities(raw.get("output_modalities"))
    supported_protocols = normalize_supported_protocols(raw.get("supported_protocols"))
    model = {
        "id": _validate_id(raw.get("id"), "model.id"),
        "provider": _validate_id(raw.get("provider"), "model.provider"),
        "upstream_id": _string(raw.get("upstream_id")),
        "family_id": _safe_capability_identity(
            raw.get("family_id"), "model.family_id"
        ),
        "display_name": _string(raw.get("display_name")),
        "description": _string(raw.get("description")),
        "supports_reasoning": supports_reasoning,
        "supports_reasoning_summaries": supports_reasoning_summaries,
        "reasoning_levels": levels,
        "reasoning_control": reasoning_control,
        "context_window": context_window,
        "max_input_tokens": max_input_tokens,
        "output_limit": output_limit,
        "created_at": created_at,
        "enabled": bool(raw.get("enabled", True)),
        "visibility": visibility,
        "input_modalities": normalize_input_modalities(raw.get("input_modalities")),
        "output_modalities": output_modalities,
        "supported_protocols": supported_protocols,
        "supports_image_detail_original": supports_image_detail_original,
        "deployment_identity": _safe_capability_identity(
            raw.get("deployment_identity"), "model.deployment_identity"
        ),
        "resolved_protocol": _resolved_protocol(raw.get("resolved_protocol")),
        "protocol_observation": _normalize_protocol_observation(
            raw.get("protocol_observation")
        ),
        "context_calibrations": _normalize_context_calibrations(
            raw.get("context_calibrations")
        ),
        "capabilities": _normalize_capabilities(
            raw.get("capabilities"), "model.capabilities"
        ),
    }
    raw_caps = raw.get("capabilities")
    explicit_fields = set(raw)
    if isinstance(raw_caps, dict):
        explicit_fields |= set(raw_caps)
    model["capability_sources"] = _normalize_capability_sources(
        raw.get("capability_sources"), model, explicit_fields
    )
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
    result["native_hidden_models"] = normalize_hidden_models(
        raw.get("native_hidden_models"), "native_hidden_models"
    )
    result["catalog_presentations"] = _normalize_catalog_presentations(
        raw.get("catalog_presentations")
    )
    result["catalog_family_presentations"] = _normalize_catalog_presentations(
        raw.get("catalog_family_presentations")
    )
    result["subscription_search"] = _normalize_subscription_search(
        raw.get("subscription_search"), account_ids
    )
    result["codex_runtime_sources"] = _normalize_codex_runtime_sources(
        raw.get("codex_runtime_sources", ["auto"])
    )
    return result


def _canonicalize_private_paths(config: Dict[str, Any], path: Path) -> Dict[str, Any]:
    try:
        canonicalize_account_paths(config, path)
    except AccountError as exc:
        raise ConfigError(str(exc)) from exc

    secret_root = Path(config["secret_store_path"]).expanduser()
    if not secret_root.is_absolute():
        secret_root = path.parent / secret_root
    secret_root = secret_root.resolve()
    for provider in config.get("providers", []):
        raw_path = provider.get("api_key_file", "")
        if not raw_path:
            continue
        expected = secret_root / (quote(provider["id"], safe="") + ".key.enc")
        actual = Path(raw_path).expanduser()
        if not actual.is_absolute():
            actual = path.parent / actual
        if actual.resolve() != expected or actual.is_symlink():
            raise ConfigError("provider.api_key_file must be managed inside the secret store")
        provider["api_key_file"] = str(expected)
    return config


def load(path: Optional[Path] = None) -> Dict[str, Any]:
    path = Path(path or config_path())
    if not path.exists():
        return normalize(None)
    try:
        with path.open("r", encoding="utf-8") as handle:
            return _canonicalize_private_paths(normalize(json.load(handle)), path)
    except json.JSONDecodeError as exc:
        raise ConfigError("invalid JSON in %s: %s" % (path, exc))


def _replace_config(source: str, destination: Path) -> None:
    os.replace(source, str(destination))


def save(
    config: Dict[str, Any],
    path: Optional[Path] = None,
    _transaction: Optional[FileTransaction] = None,
) -> Path:
    if _transaction is None:
        with file_transaction() as transaction:
            return save(config, path, _transaction=transaction)
    path = Path(path or config_path())
    previous_secret_files = set()
    if path.exists():
        try:
            previous = load(path)
            previous_secret_files = {
                str(item.get("api_key_file"))
                for item in previous.get("providers", [])
                if item.get("api_key_file")
            }
        except (OSError, ConfigError):
            pass
    normalized = _canonicalize_private_paths(normalize(config), path)
    secret_root = Path(normalized["secret_store_path"]).expanduser()
    if not secret_root.is_absolute():
        secret_root = path.parent / secret_root
    secret_root = secret_root.resolve()
    for provider in normalized["providers"]:
        key = provider.get("api_key", "")
        if key and key != "••••••••":
            secret_root.mkdir(parents=True, exist_ok=True)
            os.chmod(str(secret_root), 0o700)
            secret_path = secret_root / (quote(provider["id"], safe="") + ".key.enc")
            try:
                _transaction.remember(secret_path)
                write_encrypted_text(secret_path, key)
            except VaultError as exc:
                raise ConfigError(str(exc)) from exc
            provider["api_key_file"] = str(secret_path)
            provider["api_key"] = ""
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".easy-multi-provider-", dir=str(path.parent))
    committed = False
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(normalized, handle, indent=2, ensure_ascii=False)
            handle.write("\n")
        _transaction.remember(path)
        _replace_config(temporary, path)
        committed = True
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    if committed:
        current_secret_files = {
            str(item.get("api_key_file"))
            for item in normalized.get("providers", [])
            if item.get("api_key_file")
        }
        for obsolete in previous_secret_files - current_secret_files:
            try:
                obsolete_path = Path(obsolete)
                _transaction.remember(obsolete_path)
                obsolete_path.unlink()
            except FileNotFoundError:
                pass
            except OSError:
                pass
    return path


def merge_web_update(
    current: Dict[str, Any], incoming: Dict[str, Any], path: Optional[Path] = None
) -> Dict[str, Any]:
    """Apply a Web update while preserving secrets omitted by the UI."""
    if not isinstance(incoming, dict):
        raise ConfigError("request body must be an object")
    merged = copy.deepcopy(incoming)
    # Runtime selection has its own endpoint. A stale general settings form
    # must not overwrite a selection already saved by this or another tab.
    merged["codex_runtime_sources"] = copy.deepcopy(
        current.get("codex_runtime_sources", ["auto"])
    )
    for field in ("account_store_path", "secret_store_path", "native_catalog_path"):
        if field in current:
            merged[field] = current[field]
    old_by_id = {item["id"]: item for item in current.get("providers", [])}
    for provider in merged.get("providers", []):
        old = old_by_id.get(provider.get("id"), {})
        if "api_key" not in provider or provider.get("api_key") == "••••••••":
            provider["api_key"] = old.get("api_key", "")
        if provider.get("api_key_file") and provider.get("api_key_file") != old.get("api_key_file"):
            raise ConfigError("provider.api_key_file is managed by EMP")
        if old.get("api_key_file"):
            provider["api_key_file"] = old["api_key_file"]
        if old and any(
            provider.get(field) != old.get(field)
            for field in ("base_url", "protocol", "deployment_identity")
        ):
            provider["resolved_protocol"] = ""
            provider["protocol_observation"] = {}
    old_accounts = {item["id"]: item for item in current.get("accounts", [])}
    for account in merged.get("accounts", []):
        old = old_accounts.get(account.get("id"), {})
        if account.get("auth_file") and account.get("auth_file") != old.get("auth_file"):
            raise ConfigError("account.auth_file is managed by EMP")
        if old.get("auth_file"):
            account["auth_file"] = old["auth_file"]
    old_models = {item["id"]: item for item in current.get("models", [])}
    for model in merged.get("models", []):
        if not isinstance(model, dict):
            continue
        old = old_models.get(model.get("id"))
        model["context_calibrations"] = copy.deepcopy(
            old.get("context_calibrations", []) if old else []
        )
        for field in (
            "visibility",
            "supports_reasoning",
            "supports_reasoning_summaries",
            "input_modalities",
            "output_modalities",
            "supported_protocols",
            "reasoning_control",
            "max_input_tokens",
            "output_limit",
            "supports_image_detail_original",
            "deployment_identity",
            "resolved_protocol",
            "protocol_observation",
        ):
            if old and field not in model:
                model[field] = copy.deepcopy(old.get(field))
        old_caps = old.get("capabilities") if old else {}
        incoming_caps = model.get("capabilities")
        if old_caps and not isinstance(incoming_caps, dict):
            model["capabilities"] = copy.deepcopy(old_caps)
        elif old_caps and isinstance(incoming_caps, dict):
            for cap_field in _BOOLEAN_CAPABILITIES:
                if cap_field not in incoming_caps and cap_field in old_caps:
                    incoming_caps[cap_field] = copy.deepcopy(old_caps[cap_field])
        sources = copy.deepcopy(old.get("capability_sources", {}) if old else {})
        incoming_sources = model.get("capability_sources")
        if isinstance(incoming_sources, dict):
            sources.update(copy.deepcopy(incoming_sources))
        _TOP_LEVEL_PROVENANCE_FIELDS = (
            "supports_reasoning",
            "supports_reasoning_summaries",
            "reasoning_levels",
            "reasoning_control",
            "context_window",
            "max_input_tokens",
            "output_limit",
            "input_modalities",
            "output_modalities",
            "supported_protocols",
            "supports_image_detail_original",
        )
        for field in _TOP_LEVEL_PROVENANCE_FIELDS:
            if field not in model:
                continue
            previous = old.get(field) if old else None
            if old is None or model.get(field) != previous:
                sources[field] = make_provenance("manual", observed_at=observed_at_now())
        current_caps = model.get("capabilities")
        if isinstance(current_caps, dict):
            for cap_field in _BOOLEAN_CAPABILITIES:
                if cap_field not in current_caps:
                    continue
                previous_cap = old_caps.get(cap_field) if old_caps else None
                if old is None or current_caps.get(cap_field) != previous_cap:
                    sources[cap_field] = make_provenance("manual", observed_at=observed_at_now())
        if sources:
            model["capability_sources"] = sources
        if old and any(
            model.get(field) != old.get(field)
            for field in ("provider", "upstream_id", "deployment_identity")
        ):
            model["resolved_protocol"] = ""
            model["protocol_observation"] = {}
    normalized = normalize(merged)
    return _canonicalize_private_paths(normalized, Path(path)) if path else normalized


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
            if Path(secret_file).is_symlink():
                return ""
            return read_encrypted_text(Path(secret_file)).strip()
        except (OSError, VaultError):
            return ""
    return ""
