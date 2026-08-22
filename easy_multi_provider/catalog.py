"""Generate a Codex-compatible model catalog."""

from __future__ import annotations

import copy
import json
import os
import tempfile
from pathlib import Path
from typing import Any, Dict, List, Optional

from .accounts import duplicate_account_status
from .capabilities import codex_input_modalities
from .config import MAX_CONTEXT_WINDOW

EFFORT_DESCRIPTIONS = {
    "minimal": "Fast responses with minimal reasoning",
    "low": "Fast responses with lighter reasoning",
    "medium": "Balances speed and reasoning depth",
    "high": "Greater reasoning depth for complex tasks",
    "xhigh": "Extra high reasoning depth for hard tasks",
    "max": "Maximum reasoning depth",
}


def _usable_context_window(model: Dict[str, Any]) -> int:
    try:
        context_window = int(model.get("context_window", 0) or 0)
    except (TypeError, ValueError):
        return 0
    if context_window <= 0:
        return 0
    try:
        percentage = float(model.get("effective_context_window_percent", 100) or 100)
    except (TypeError, ValueError):
        percentage = 100
    if 0 < percentage <= 100:
        return max(1, round(context_window * percentage / 100))
    return context_window


def _compact_context_window(tokens: int) -> str:
    if tokens >= 1_000_000:
        value = ("%.2f" % (tokens / 1_000_000)).rstrip("0").rstrip(".")
        return value + "M"
    if tokens >= 1_000:
        return "%dK" % round(tokens / 1_000)
    return str(tokens)


def _display_name_with_context(model: Dict[str, Any]) -> str:
    name = str(model.get("display_name") or model.get("slug") or model.get("id") or "")
    context_window = _usable_context_window(model)
    if not context_window:
        return name
    return "%s [%s]" % (name, _compact_context_window(context_window))


def _description_with_context(model: Dict[str, Any]) -> str:
    description = str(model.get("description") or "")
    context_window = _usable_context_window(model)
    if not context_window:
        return description
    context = "Context %s" % _compact_context_window(context_window)
    return "%s · %s" % (description, context) if description else context


def native_path(config: Dict[str, Any]) -> Path:
    configured = config.get("native_catalog_path", "")
    if configured:
        return Path(os.path.expanduser(configured))
    return Path.home() / ".codex" / "models_cache.json"


def load_native_catalog(config: Dict[str, Any]) -> Dict[str, Any]:
    path = native_path(config)
    if not path.exists():
        return {"models": []}
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle)
    except (OSError, ValueError):
        return {"models": []}
    if not isinstance(value, dict) or not isinstance(value.get("models"), list):
        return {"models": []}
    return value


def _reasoning_levels(values: List[str]) -> List[Dict[str, str]]:
    return [
        {"effort": value, "description": EFFORT_DESCRIPTIONS.get(value, value)}
        for value in values
    ]


def _external_entry(model: Dict[str, Any], template: Dict[str, Any]) -> Dict[str, Any]:
    entry = copy.deepcopy(template)
    for field in (
        "context_window",
        "max_context_window",
        "effective_context_window_percent",
        "auto_compact_token_limit",
    ):
        entry.pop(field, None)
    levels = model.get("reasoning_levels") or ["medium"]
    entry.update(
        {
            "slug": model["id"],
            "display_name": model.get("display_name") or model["id"],
            "description": model.get("description") or "External provider model",
            "default_reasoning_level": levels[0],
            "supported_reasoning_levels": _reasoning_levels(levels),
            "visibility": model.get("visibility", "list"),
            "supported_in_api": True,
            "input_modalities": codex_input_modalities(model.get("input_modalities")),
            "supports_reasoning_summaries": False,
            "default_reasoning_summary": "none",
            "support_verbosity": False,
            "default_verbosity": None,
            "supports_search_tool": False,
            "supports_image_detail_original": model.get(
                "supports_image_detail_original", False
            )
            is True,
            "supports_parallel_tool_calls": False,
            "apply_patch_tool_type": None,
        }
    )
    context_window = min(MAX_CONTEXT_WINDOW, int(model.get("context_window", 0) or 0))
    if context_window:
        entry["context_window"] = context_window
        entry["max_context_window"] = context_window
        entry["auto_compact_token_limit"] = context_window * 4 // 5
    entry["display_name"] = _display_name_with_context(entry)
    entry["description"] = _description_with_context(entry)
    return entry


def _account_entry(account: Dict[str, Any], native: Dict[str, Any]) -> Dict[str, Any]:
    entry = copy.deepcopy(native)
    slug = str(native.get("slug", ""))
    entry["slug"] = account["prefix"] + "/" + slug
    entry["display_name"] = "%s · %s" % (
        account.get("name") or account["prefix"],
        _display_name_with_context(native),
    )
    entry["description"] = "ChatGPT subscription: %s" % (account.get("name") or account["prefix"])
    entry["description"] = _description_with_context(entry)
    entry["visibility"] = native.get("visibility", "list")
    entry["supported_in_api"] = native.get("supported_in_api", True)
    return entry


def _subscription_native_models(config: Dict[str, Any]) -> List[Dict[str, Any]]:
    native = load_native_catalog(config)
    return [
        item
        for item in native["models"]
        if isinstance(item, dict)
        and str(item.get("slug", "")).strip()
        and item.get("visibility", "list") == "list"
        and item.get("supported_in_api", True) is not False
    ]


def subscription_model_options(config: Dict[str, Any]) -> List[Dict[str, str]]:
    """Return native Coding Agent models that subscription aliases may expose."""
    return [
        {
            "id": str(model["slug"]),
            "display_name": str(model.get("display_name") or model["slug"]),
            "description": str(model.get("description") or ""),
        }
        for model in _subscription_native_models(config)
    ]


def build_catalog(config: Dict[str, Any]) -> Dict[str, Any]:
    native = load_native_catalog(config)
    native_models = [copy.deepcopy(item) for item in native["models"] if isinstance(item, dict)]
    for model in native_models:
        model["display_name"] = _display_name_with_context(model)
        model["description"] = _description_with_context(model)
    existing = {str(item.get("slug")) for item in native_models}
    template = native_models[0] if native_models else {
        "base_instructions": "You are a helpful coding assistant.",
        "model_messages": {},
        "shell_type": "shell_command",
    }
    external = []
    account_aliases = []
    duplicate_accounts = duplicate_account_status(config.get("accounts", []))
    native_hidden_models = {
        model_id
        for account in config.get("accounts", [])
        if duplicate_accounts.get(account.get("id")) == "当前 Codex 登录"
        for model_id in account.get("hidden_models", [])
    }
    for model in native_models:
        if model.get("slug") in native_hidden_models:
            model["visibility"] = "hide"
    subscription_models = _subscription_native_models(config)
    for account in config.get("accounts", []):
        if (
            not account.get("enabled", True)
            or not account.get("auth_file")
            or account.get("id") in duplicate_accounts
        ):
            continue
        hidden_models = set(account.get("hidden_models", []))
        for model in subscription_models:
            if model.get("slug") in hidden_models:
                continue
            alias = _account_entry(account, model)
            if alias["slug"] not in existing:
                account_aliases.append(alias)
                existing.add(alias["slug"])
    for model in config.get("models", []):
        if not model.get("enabled", True) or model["id"] in existing:
            continue
        external.append(_external_entry(model, template))
    return {"models": native_models + account_aliases + external}


def write_catalog(config: Dict[str, Any], path: Path) -> Path:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".codex-models-", dir=str(path.parent))
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            json.dump(build_catalog(config), handle, indent=2, ensure_ascii=False)
            handle.write("\n")
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return path


def generated_catalog_path(codex_home: Optional[Path] = None) -> Path:
    """Return EMP's stable catalog location below the active Codex home."""

    if codex_home is None:
        configured_home = os.environ.get("CODEX_HOME", "").strip()
        home = Path(configured_home).expanduser() if configured_home else Path.home() / ".codex"
    else:
        home = Path(codex_home).expanduser()
    return home.resolve() / "easy-multi-provider" / "catalog.json"
