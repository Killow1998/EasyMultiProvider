"""Generate a Codex-compatible model catalog."""

from __future__ import annotations

import copy
import json
import os
import tempfile
from pathlib import Path
from typing import Any, Dict, List

from .accounts import duplicate_account_status
from .config import MAX_CONTEXT_WINDOW

EFFORT_DESCRIPTIONS = {
    "minimal": "Fast responses with minimal reasoning",
    "low": "Fast responses with lighter reasoning",
    "medium": "Balances speed and reasoning depth",
    "high": "Greater reasoning depth for complex tasks",
    "xhigh": "Extra high reasoning depth for hard tasks",
    "max": "Maximum reasoning depth",
}


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
    levels = model.get("reasoning_levels") or ["medium"]
    entry.update(
        {
            "slug": model["id"],
            "display_name": model.get("display_name") or model["id"],
            "description": model.get("description") or "External provider model",
            "default_reasoning_level": levels[0],
            "supported_reasoning_levels": _reasoning_levels(levels),
            "visibility": "list",
            "supported_in_api": True,
            "input_modalities": ["text"],
            "supports_reasoning_summaries": False,
            "default_reasoning_summary": "none",
            "support_verbosity": False,
            "default_verbosity": None,
            "supports_search_tool": False,
            "supports_image_detail_original": False,
            "supports_parallel_tool_calls": False,
            "apply_patch_tool_type": None,
        }
    )
    context_window = min(MAX_CONTEXT_WINDOW, int(model.get("context_window", 0) or 0))
    if context_window:
        entry["context_window"] = context_window
        entry["max_context_window"] = context_window
        entry["auto_compact_token_limit"] = context_window * 4 // 5
    return entry


def _account_entry(account: Dict[str, Any], native: Dict[str, Any]) -> Dict[str, Any]:
    entry = copy.deepcopy(native)
    slug = str(native.get("slug", ""))
    entry["slug"] = account["prefix"] + "/" + slug
    entry["display_name"] = "%s · %s" % (
        account.get("name") or account["prefix"],
        native.get("display_name") or slug,
    )
    entry["description"] = "ChatGPT subscription: %s" % (account.get("name") or account["prefix"])
    entry["visibility"] = "list"
    entry["supported_in_api"] = True
    return entry


def build_catalog(config: Dict[str, Any]) -> Dict[str, Any]:
    native = load_native_catalog(config)
    native_models = [item for item in native["models"] if isinstance(item, dict)]
    existing = {str(item.get("slug")) for item in native_models}
    template = native_models[0] if native_models else {
        "base_instructions": "You are a helpful coding assistant.",
        "model_messages": {},
        "shell_type": "shell_command",
    }
    external = []
    account_aliases = []
    duplicate_accounts = duplicate_account_status(config.get("accounts", []))
    for account in config.get("accounts", []):
        if (
            not account.get("enabled", True)
            or not account.get("auth_file")
            or account.get("id") in duplicate_accounts
        ):
            continue
        for model in native_models:
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


def generated_catalog_path() -> Path:
    return Path("generated") / "codex-models.json"


def codex_profile_path() -> Path:
    configured_home = os.environ.get("CODEX_HOME", "").strip()
    home = Path(configured_home).expanduser() if configured_home else Path.home() / ".codex"
    return home.resolve() / "emp.config.toml"


def integration_info(config: Dict[str, Any], catalog_path: Path) -> Dict[str, Any]:
    host = config.get("host", "127.0.0.1")
    if host in ("0.0.0.0", "::"):
        host = "127.0.0.1"
    base_url = "http://%s:%d/v1" % (host, config["port"])

    def toml_string(value: str) -> str:
        return '"%s"' % value.replace("\\", "\\\\").replace('"', '\\"')

    snippet = "\n".join(
        [
            'model_provider = "easy-multi-provider"',
            "model_catalog_json = %s" % toml_string(str(catalog_path.resolve())),
            "",
            "[model_providers.easy-multi-provider]",
            'name = "EasyMultiProvider"',
            "base_url = %s" % toml_string(base_url),
            'wire_api = "responses"',
            "requires_openai_auth = true",
            "supports_websockets = false",
        ]
    )
    return {
        "base_url": base_url,
        "catalog_path": str(catalog_path.resolve()),
        "profile_path": str(codex_profile_path()),
        "command": "codex --profile emp",
        "snippet": snippet,
    }


def write_codex_profile(config: Dict[str, Any], catalog_path: Path) -> Path:
    info = integration_info(config, catalog_path)
    path = codex_profile_path()
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix=".emp-profile-", dir=str(path.parent))
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write("# Generated by EasyMultiProvider.\n")
            handle.write(info["snippet"])
            handle.write("\n")
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)
    return path
