"""Generate a Codex-compatible model catalog."""

from __future__ import annotations

import copy
import hashlib
import json
import os
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

from .accounts import auth_headers, duplicate_account_status
from .capabilities import codex_input_modalities, normalize_reasoning_levels
from .config import MAX_CONTEXT_WINDOW
from .integration import atomic_write_text

RETIRED_SUBSCRIPTION_MODELS = frozenset({"gpt-5.5"})


def subscription_context_max(model: Dict[str, Any]) -> int:
    # A missing maximum permits the advertised default, not a guessed API limit.
    value = model.get("max_context_window")
    if value is None:
        value = model.get("context_window")
    return value if isinstance(value, int) and not isinstance(value, bool) and 0 < value <= MAX_CONTEXT_WINDOW else 0


def _apply_subscription_context(model: Dict[str, Any], windows: Dict[str, int]) -> None:
    # Codex defaults omitted/null percentages to 95; make that default explicit
    # so EMP's label and context guard calculate the same usable window.
    if model.get("effective_context_window_percent") is None:
        model["effective_context_window_percent"] = 95
    requested = windows.get(str(model.get("slug") or ""))
    maximum = subscription_context_max(model)
    if requested and maximum:
        model["context_window"] = min(requested, maximum)
        # Let Codex calculate compaction against the new window. Do not carry
        # an absolute threshold from the smaller default context.
        model.pop("auto_compact_token_limit", None)


def account_catalog_owner(account: Dict[str, Any]) -> str:
    headers = auth_headers(account)
    identity = headers.get("ChatGPT-Account-ID") or headers.get("chatgpt-account-id") or headers.get("Authorization", "")
    return hashlib.sha256(identity.encode("utf-8")).hexdigest()


def _account_catalog(config: Dict[str, Any], account: Dict[str, Any]) -> Dict[str, Any]:
    try:
        path = Path(account["auth_file"]).parent / "models_cache.json"
        if path.is_symlink() or path.stat().st_size > 4 * 1024 * 1024:
            return load_native_catalog(config)
        cached = json.loads(path.read_text(encoding="utf-8"))
        if isinstance(cached, dict) and isinstance(cached.get("models"), list) and cached.get("account_owner") == account_catalog_owner(account) and cached.get("base_url") == config.get("codex_base_url"):
            return cached
    except (KeyError, OSError, ValueError):
        pass
    return load_native_catalog(config)

EFFORT_DESCRIPTIONS = {
    "minimal": "Fast responses with minimal reasoning",
    "low": "Fast responses with lighter reasoning",
    "medium": "Balances speed and reasoning depth",
    "high": "Greater reasoning depth for complex tasks",
    "xhigh": "Extra high reasoning depth for hard tasks",
    "max": "Maximum reasoning depth",
    "ultra": "Maximum reasoning with automatic task delegation",
}
_CONTEXT_SUFFIX = re.compile(r"\s+\[\s*(?:\d+(?:\.\d+)?(?:K|M)?|\?)\]$")
_CONTEXT_PREFIX = re.compile(r"^\[\s*(?:\d+(?:\.\d+)?(?:K|M)?|\?)\]\s+")
_DESCRIPTION_CONTEXT_SUFFIX = re.compile(
    r"(?:\s+·\s+)?Context\s+(?:\d+(?:\.\d+)?(?:K|M)?|\?)$"
)


def catalog_etag(catalog: Dict[str, Any]) -> str:
    """Use the same catalog revision for discovery and Responses notifications."""
    payload = json.dumps(catalog, ensure_ascii=False, sort_keys=True, separators=(",", ":"))
    return '"emp-' + hashlib.sha256(payload.encode("utf-8")).hexdigest() + '"'


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


def _display_name_with_context(
    model: Dict[str, Any], presentation: Optional[Dict[str, Any]] = None
) -> str:
    presentation = presentation or {}
    alias = presentation.get("catalog_alias", "")
    if isinstance(alias, str) and alias:
        # A user alias is exact presentation data. Do not reinterpret a
        # context-looking prefix or suffix that was entered deliberately.
        source_label = (
            model.get("_emp_source_label")
            if presentation.get("_family_scoped") is True
            else ""
        )
        name = "%s · %s" % (source_label, alias) if source_label else alias
    else:
        name = str(
            model.get("display_name")
            or model.get("slug")
            or model.get("id")
            or ""
        )
        name = _CONTEXT_PREFIX.sub("", name)
        name = _CONTEXT_SUFFIX.sub("", name)
    if presentation.get("show_context", True) is False:
        return name
    context_window = _usable_context_window(model)
    context = _compact_context_window(context_window) if context_window else "?"
    return "[%5s]  %s" % (context, name)


def _route_presentation(config: Dict[str, Any], route: str) -> Dict[str, Any]:
    presentations = config.get("catalog_presentations")
    if not isinstance(presentations, dict):
        return {}
    value = presentations.get(route)
    return value if isinstance(value, dict) else {}


def _family_presentation(config: Dict[str, Any], family: str) -> Dict[str, Any]:
    presentations = config.get("catalog_family_presentations")
    if not isinstance(presentations, dict):
        return {}
    value = presentations.get(family)
    if not isinstance(value, dict):
        return {}
    result = dict(value)
    result["_family_scoped"] = True
    return result


def presentation_for_route(
    config: Dict[str, Any],
    route: str,
    model: Optional[Dict[str, Any]] = None,
    *,
    family_verified: bool = False,
) -> Dict[str, Any]:
    model = model or {}
    # build_catalog resolves the family before rewriting native models into
    # account/provider routes.  Keep that canonical identity here: current
    # Codex model caches commonly omit family_id/upstream_id, so recomputing
    # from an account route would incorrectly produce e.g.
    # ``killow/gpt-5.6-sol`` instead of ``gpt-5.6-sol``.
    family = str(model.get("_emp_family") or model_family_identity(model, route))
    if family_verified:
        presentation = _family_presentation(config, family)
        if presentation:
            return presentation
    return _route_presentation(config, route)


def _apply_presentation(config: Dict[str, Any], entry: Dict[str, Any]) -> None:
    route = str(entry.get("slug") or "")
    presentation = presentation_for_route(
        config,
        route,
        entry,
        family_verified=entry.get("_emp_family_verified") is True,
    )
    entry["display_name"] = _display_name_with_context(
        entry, presentation
    )
    entry["description"] = _description_with_context(entry, presentation)
    policy = presentation.get("reasoning_summary", "auto")
    supports_summary = entry.get("supports_reasoning_summary_parameter")
    if not isinstance(supports_summary, bool):
        supports_summary = entry.get("supports_reasoning_summaries") is True
    if policy == "hide" or (policy == "show" and not supports_summary):
        entry["default_reasoning_summary"] = "none"
    elif policy == "show" and supports_summary:
        entry["default_reasoning_summary"] = "auto"


def model_family_identity(model: Dict[str, Any], fallback: str) -> str:
    explicit = model.get("family_id")
    if isinstance(explicit, str) and explicit.strip():
        return explicit.strip()
    upstream = model.get("upstream_id")
    if isinstance(upstream, str) and upstream.strip():
        return upstream.strip()
    # A matching route suffix is not proof that independent providers expose
    # the same family. Only explicit capability metadata may join sources.
    return fallback


def has_explicit_family_identity(model: Dict[str, Any]) -> bool:
    return any(
        isinstance(model.get(field), str) and bool(model.get(field).strip())
        for field in ("family_id", "upstream_id")
    )


def _release_value(model: Dict[str, Any]) -> int:
    try:
        value = int(model.get("created_at", 0) or 0)
    except (TypeError, ValueError):
        return 0
    return max(0, value)


def _description_with_context(
    model: Dict[str, Any], presentation: Optional[Dict[str, Any]] = None
) -> str:
    presentation = presentation or {}
    description = _DESCRIPTION_CONTEXT_SUFFIX.sub(
        "", str(model.get("description") or "").strip()
    ).strip()
    alias = presentation.get("catalog_alias", "")
    if isinstance(alias, str) and alias:
        if description != alias and not description.startswith(alias + " · "):
            description = "%s · %s" % (alias, description) if description else alias
    if presentation.get("show_context", True) is False:
        return description
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
    if str(value.get("etag", "")).strip('"').startswith("emp-"):
        try:
            value = json.loads((path.parent / "easy-multi-provider" / "native-catalog.json").read_text(encoding="utf-8"))
        except (OSError, ValueError):
            return {"models": []}
        if not isinstance(value, dict) or not isinstance(value.get("models"), list):
            return {"models": []}
    return value


def preserve_native_catalog(config: Dict[str, Any]) -> None:
    """Keep native metadata separate from Codex's cache of EMP's merged catalog."""
    value = load_native_catalog(config)
    if value.get("models"):
        path = native_path(config).parent / "easy-multi-provider" / "native-catalog.json"
        atomic_write_text(path, json.dumps(value, ensure_ascii=False))


def _reasoning_levels(values: List[str]) -> List[Dict[str, str]]:
    return [
        {"effort": value, "description": EFFORT_DESCRIPTIONS.get(value, value)}
        for value in values
    ]


def _boolean_capability(
    model: Dict[str, Any], provider: Dict[str, Any], field: str
) -> bool:
    for source in (model, provider):
        if isinstance(source.get(field), bool):
            return source[field]
        nested = source.get("capabilities")
        if isinstance(nested, dict) and isinstance(nested.get(field), bool):
            return nested[field]
    return False


def _supports_reasoning_summary_route(
    model: Dict[str, Any], provider: Dict[str, Any]
) -> bool:
    if model.get("supports_reasoning_summaries") is not True:
        return False
    protocol = provider.get("protocol")
    if protocol == "responses":
        return True
    if protocol != "auto":
        return False
    resolved = model.get("resolved_protocol") or provider.get("resolved_protocol")
    return resolved == "responses"


def _external_entry(
    model: Dict[str, Any], template: Dict[str, Any], provider: Dict[str, Any]
) -> Dict[str, Any]:
    # Reuse coding instructions, not arbitrary upstream capabilities or account
    # entitlements. Newly introduced native fields must be reviewed explicitly.
    entry = {
        key: copy.deepcopy(template[key])
        for key in (
            "base_instructions", "shell_type", "truncation_policy",
            "include_skills_usage_instructions", "include_plugin_usage_instructions",
            "include_apps_usage_instructions", "node_repl_auto_review_required",
            "node_repl_disabled",
        )
        if key in template
    }
    messages = template.get("model_messages")
    if isinstance(messages, dict):
        entry["model_messages"] = {
            key: copy.deepcopy(messages[key])
            for key in (
                "instructions_template", "instructions_variables", "tools",
                "approvals", "collaboration_modes", "auto_review", "permissions",
                "multi_agent",
            )
            if key in messages
        }
    entry.update({
        "service_tiers": [],
        "additional_speed_tiers": [],
        "default_service_tier": None,
        "availability_nux": None,
        "upgrade": None,
        "model_specialty": None,
        "experimental_supported_tools": [],
        "multi_agent_reasoning_effort": None,
        "use_responses_lite": False,
        "supports_experimental_context": False,
        "priority": 0,
    })
    levels = normalize_reasoning_levels(model.get("reasoning_levels"))
    friendly_name = str(model.get("display_name") or "").strip()
    description = str(model.get("description") or "").strip()
    supports_reasoning_summaries = _supports_reasoning_summary_route(
        model, provider
    )
    if not description and friendly_name and friendly_name != model["id"]:
        description = friendly_name
    entry.update(
        {
            "slug": model["id"],
            "display_name": model["id"],
            "description": description or "External provider model",
            "visibility": model.get("visibility", "list"),
            "supported_in_api": True,
            "input_modalities": codex_input_modalities(model.get("input_modalities")),
            "supports_reasoning_summaries": supports_reasoning_summaries,
            "supports_reasoning_summary_parameter": supports_reasoning_summaries,
            "default_reasoning_summary": (
                "auto" if supports_reasoning_summaries else "none"
            ),
            "support_verbosity": False,
            "default_verbosity": None,
            "supports_search_tool": True,
            "supports_image_detail_original": model.get(
                "supports_image_detail_original", False
            )
            is True,
            "supports_parallel_tool_calls": _boolean_capability(
                model, provider, "parallel_tools"
            ),
            "apply_patch_tool_type": None,
            # A routed model may be selected explicitly as a native Codex
            # child model.  Do not inherit the native template's value here:
            # multi_agent_version describes which collaboration backend the
            # model can orchestrate itself, not whether it can be delegated to.
            # None remains eligible as a child in Codex while avoiding a false
            # claim that every external model can spawn further agents.
            "multi_agent_version": None,
            # Codex 0.149 requires the field even when an external provider
            # does not advertise concrete effort levels. An empty list keeps
            # the capability honest without fabricating a default effort.
            "supported_reasoning_levels": [],
        }
    )
    if levels:
        entry["default_reasoning_level"] = levels[(len(levels) - 1) // 2]
        entry["supported_reasoning_levels"] = _reasoning_levels(levels)
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
    _apply_subscription_context(entry, account.get("model_context_windows", {}))
    # Missing means unknown, not an empty list of confirmed entitlements.
    entry.pop("available_access_programs", None)
    slug = str(native.get("slug", ""))
    entry["slug"] = account["prefix"] + "/" + slug
    native_name = str(native.get("display_name") or slug)
    native_name = _CONTEXT_PREFIX.sub("", native_name)
    native_name = _CONTEXT_SUFFIX.sub("", native_name)
    entry["display_name"] = "%s · %s" % (
        account.get("name") or account["prefix"],
        native_name,
    )
    entry["display_name"] = _display_name_with_context(entry)
    entry["description"] = "ChatGPT subscription: %s" % (account.get("name") or account["prefix"])
    entry["description"] = _description_with_context(entry)
    entry["_emp_source_label"] = account.get("name") or account["prefix"]
    entry["visibility"] = native.get("visibility", "list")
    entry["supported_in_api"] = native.get("supported_in_api", True)
    return entry


def _subscription_native_models(config: Dict[str, Any], account: Optional[Dict[str, Any]] = None) -> List[Dict[str, Any]]:
    native = _account_catalog(config, account) if account is not None else load_native_catalog(config)
    return [
        item
        for item in native["models"]
        if isinstance(item, dict)
        and str(item.get("slug", "")).strip()
        and item.get("slug") not in RETIRED_SUBSCRIPTION_MODELS
        and item.get("visibility", "list") == "list"
        and item.get("supported_in_api", True) is not False
    ]


def subscription_model_options(config: Dict[str, Any], account: Optional[Dict[str, Any]] = None) -> List[Dict[str, Any]]:
    """Return native Coding Agent models that subscription aliases may expose."""
    return [
        {
            "id": str(model["slug"]),
            "display_name": str(model.get("display_name") or model["slug"]),
            "description": str(model.get("description") or ""),
            "context_window": _usable_context_window({**model, "effective_context_window_percent": model.get("effective_context_window_percent") or 95}),
            "default_context_window": model.get("context_window", 0),
            "max_context_window": subscription_context_max(model),
            "effective_context_window_percent": model.get("effective_context_window_percent") or 95,
            "supports_reasoning_summaries": (
                model.get("supports_reasoning_summary_parameter") is True
                or model.get("supports_reasoning_summaries") is True
            ),
        }
        for model in _subscription_native_models(config, account)
    ]


def validate_subscription_contexts(config: Dict[str, Any], previous: Optional[Dict[str, Any]] = None) -> None:
    sources = [(None, config.get("native_model_context_windows", {}), (previous or {}).get("native_model_context_windows", {}))]
    old_accounts = {a["id"]: a for a in (previous or {}).get("accounts", [])}
    sources.extend((a, a.get("model_context_windows", {}), old_accounts.get(a["id"], {}).get("model_context_windows", {})) for a in config.get("accounts", []))
    for account, windows, old in sources:
        limits = {m["id"]: m["max_context_window"] for m in subscription_model_options(config, account)}
        for slug, tokens in windows.items():
            if old.get(slug) == tokens:
                continue
            maximum = limits.get(slug, 0)
            if not maximum or tokens > maximum:
                raise ValueError("Context for %s exceeds the subscription catalog limit (%d tokens); refresh models first" % (slug, maximum))


def subscription_route_model(config: Dict[str, Any], slug: str, account: Optional[Dict[str, Any]] = None) -> Optional[Dict[str, Any]]:
    catalog = _account_catalog(config, account) if account is not None else load_native_catalog(config)
    for item in catalog.get("models", []):
        if isinstance(item, dict) and item.get("slug") == slug and item.get("supported_in_api", True) is not False:
            model = copy.deepcopy(item)
            windows = account.get("model_context_windows", {}) if account is not None else config.get("native_model_context_windows", {})
            _apply_subscription_context(model, windows)
            return model
    return None


def build_catalog(config: Dict[str, Any]) -> Dict[str, Any]:
    native = load_native_catalog(config)
    duplicate_accounts = duplicate_account_status(config.get("accounts", []))
    native_hidden_models = set(config.get("native_hidden_models", []))
    native_hidden_models.update(
        model_id
        for account in config.get("accounts", [])
        if duplicate_accounts.get(account.get("id")) == "当前 Codex 登录"
        for model_id in account.get("hidden_models", [])
    )
    native_models = [
        copy.deepcopy(item)
        for item in native["models"]
        if isinstance(item, dict)
        and item.get("slug") not in RETIRED_SUBSCRIPTION_MODELS
        and item.get("supported_in_api", True) is not False
        and not (
            item.get("slug") in native_hidden_models
            and item.get("visibility", "list") != "hide"
        )
    ]
    for model_index, model in enumerate(native_models):
        _apply_subscription_context(model, config.get("native_model_context_windows", {}))
        slug = str(model.get("slug") or "")
        model["_emp_family"] = model_family_identity(model, slug)
        model["_emp_family_verified"] = True
        model["_emp_release"] = _release_value(model)
        model["_emp_source_rank"] = 0
        model["_emp_source_order"] = model_index
    existing = {str(item.get("slug")) for item in native_models}
    template = native_models[0] if native_models else {
        "base_instructions": "You are a helpful coding assistant.",
        "model_messages": {},
        "shell_type": "shell_command",
    }
    external_by_provider = {}
    account_aliases = []
    for account_index, account in enumerate(config.get("accounts", [])):
        if (
            not account.get("enabled", True)
            or not account.get("auth_file")
            or account.get("credential_status") == "invalid"
            or account.get("id") in duplicate_accounts
        ):
            continue
        hidden_models = set(account.get("hidden_models", []))
        for model in _subscription_native_models(config, account):
            if model.get("slug") in hidden_models:
                continue
            alias = _account_entry(account, model)
            if alias["slug"] not in existing:
                native_slug = str(model.get("slug") or "")
                alias["_emp_family"] = model_family_identity(model, native_slug)
                alias["_emp_family_verified"] = True
                alias["_emp_release"] = _release_value(model)
                alias["_emp_source_rank"] = 1
                alias["_emp_source_order"] = account_index
                account_aliases.append(alias)
                existing.add(alias["slug"])
    providers_by_id = {
        str(provider.get("id")): provider
        for provider in config.get("providers", [])
        if isinstance(provider, dict) and provider.get("id")
    }
    provider_order = [
        str(provider.get("id"))
        for provider in config.get("providers", [])
        if isinstance(provider, dict) and provider.get("id")
    ]
    for model_index, model in enumerate(config.get("models", [])):
        if not model.get("enabled", True) or model["id"] in existing:
            continue
        entry = _external_entry(
            model,
            template,
            providers_by_id.get(str(model.get("provider") or ""), {}),
        )
        entry["_emp_family"] = model_family_identity(model, model["id"])
        entry["_emp_family_verified"] = has_explicit_family_identity(model)
        provider = providers_by_id.get(str(model.get("provider") or ""), {})
        entry["_emp_source_label"] = str(
            provider.get("name") or provider.get("id") or model.get("provider") or ""
        )
        entry["_emp_release"] = _release_value(model)
        entry["_emp_source_rank"] = 2
        entry["_emp_source_order"] = (
            provider_order.index(str(model.get("provider") or ""))
            if str(model.get("provider") or "") in provider_order
            else len(provider_order)
        )
        entry["_emp_order"] = model_index
        external_by_provider.setdefault(str(model.get("provider") or ""), []).append(entry)
    external = []
    ordered_provider_ids = provider_order + sorted(
        provider_id
        for provider_id in external_by_provider
        if provider_id not in provider_order
    )
    for provider_id in ordered_provider_ids:
        entries = external_by_provider.get(provider_id, [])
        entries.sort(
            key=lambda item: (
                -(float(item.get("_emp_release") or 0)),
                int(item.get("_emp_order") or 0),
            )
        )
        for entry in entries:
            external.append(entry)
    result = native_models + account_aliases + external
    family_release: Dict[str, int] = {}
    family_order: Dict[str, int] = {}
    family_verified: Dict[str, bool] = {}
    for entry in result:
        family = str(entry.get("_emp_family") or entry.get("slug") or "")
        family_order.setdefault(family, len(family_order))
        family_verified[family] = bool(
            family_verified.get(family, False)
            or entry.get("_emp_family_verified") is True
        )
        family_release[family] = max(
            family_release.get(family, 0), int(entry.get("_emp_release") or 0)
        )
    result.sort(
        key=lambda entry: (
            -family_release.get(
                str(entry.get("_emp_family") or entry.get("slug") or ""), 0
            ),
            (
                0,
                str(entry.get("_emp_family") or entry.get("slug") or ""),
            )
            if family_verified.get(
                str(entry.get("_emp_family") or entry.get("slug") or ""),
                False,
            )
            else (
                1,
                family_order.get(
                    str(entry.get("_emp_family") or entry.get("slug") or ""),
                    len(family_order),
                ),
            ),
            int(entry.get("_emp_source_rank") or 0),
            int(entry.get("_emp_source_order") or 0),
            int(entry.get("_emp_order") or 0),
            str(entry.get("slug") or ""),
        )
    )
    for entry in result:
        _apply_presentation(config, entry)
        for field in (
            "_emp_family",
            "_emp_family_verified",
            "_emp_release",
            "_emp_source_rank",
            "_emp_source_order",
            "_emp_order",
            "_emp_source_label",
        ):
            entry.pop(field, None)
    return {"models": result}


def write_catalog(config: Dict[str, Any], path: Path) -> Path:
    return atomic_write_text(path, json.dumps(build_catalog(config), indent=2, ensure_ascii=False) + "\n")


def generated_catalog_path(codex_home: Optional[Path] = None) -> Path:
    """Return EMP's stable catalog location below the active Codex home."""

    if codex_home is None:
        configured_home = os.environ.get("CODEX_HOME", "").strip()
        home = Path(configured_home).expanduser() if configured_home else Path.home() / ".codex"
    else:
        home = Path(codex_home).expanduser()
    return home.resolve() / "easy-multi-provider" / "catalog.json"
