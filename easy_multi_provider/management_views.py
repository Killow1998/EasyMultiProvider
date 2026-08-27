"""Credential-free Web management projections."""

from __future__ import annotations

import re
from typing import Any, Dict, Mapping

from . import __version__
from .capabilities import safe_capability_list
from .catalog import build_catalog, subscription_model_options
from .config import public_config


_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages", "unknown"}
)


def management_config(config: Dict[str, Any]) -> Dict[str, Any]:
    result = public_config(config)
    result["emp_version"] = __version__
    result["subscription_models"] = subscription_model_options(config)
    result["catalog_models"] = management_catalog_models(config)
    return result


def management_catalog_models(config: Dict[str, Any]) -> list[Dict[str, Any]]:
    """Return the small, credential-free model view used by display settings."""

    baseline_config = dict(config)
    baseline_config["catalog_presentations"] = {}
    baseline = {
        str(item.get("slug") or ""): item
        for item in build_catalog(baseline_config).get("models", [])
        if isinstance(item, Mapping)
    }
    external_sources = {
        str(item.get("id") or ""): str(item.get("provider") or "")
        for item in config.get("models", [])
        if isinstance(item, Mapping)
    }
    account_prefixes = [
        (str(item.get("prefix") or ""), str(item.get("id") or ""))
        for item in config.get("accounts", [])
        if isinstance(item, Mapping) and item.get("prefix") and item.get("id")
    ]
    rows = []
    for item in build_catalog(config).get("models", []):
        if not isinstance(item, Mapping):
            continue
        if item.get("visibility", "list") != "list":
            continue
        if item.get("supported_in_api", True) is False:
            continue
        route = str(item.get("slug") or "")
        if not route:
            continue
        source_type = "native"
        source_id = ""
        if route in external_sources:
            source_type = "provider"
            source_id = external_sources[route]
        else:
            for prefix, account_id in account_prefixes:
                if route.startswith(prefix + "/"):
                    source_type = "account"
                    source_id = account_id
                    break
        try:
            context_window = int(item.get("context_window", 0) or 0)
        except (TypeError, ValueError):
            context_window = 0
        try:
            percentage = float(
                item.get("effective_context_window_percent", 100) or 100
            )
        except (TypeError, ValueError):
            percentage = 100
        if context_window > 0 and 0 < percentage <= 100:
            context_window = max(1, round(context_window * percentage / 100))
        default_name = str(baseline.get(route, {}).get("display_name") or route)
        default_name = re.sub(
            r"^\[\s*(?:\d+(?:\.\d+)?(?:K|M)?|\?)\]\s+", "", default_name
        )
        rows.append(
            {
                "id": route,
                "display_name": str(item.get("display_name") or route),
                "default_display_name": default_name,
                "context_window": max(0, context_window),
                "source_type": source_type,
                "source_id": source_id,
                "supports_reasoning_summaries": (
                    item.get("supports_reasoning_summary_parameter") is True
                ),
            }
        )
    return rows


def management_capabilities(state: Any) -> Dict[str, Any]:
    config = state.snapshot()
    providers = {
        item.get("id"): item
        for item in config.get("providers", [])
        if isinstance(item, Mapping)
    }
    models = {
        item.get("id"): item
        for item in config.get("models", [])
        if isinstance(item, Mapping)
    }
    records = safe_capability_list(config)
    for record in records:
        provider = providers.get(record.get("provider_id"))
        model = models.get(record.get("model_id"))
        if provider is None or model is None:
            continue
        effective = (
            record.get("capabilities", {})
            .get("effective_protocol", {})
            .get("value", "unknown")
        )
        protocol = effective if effective in _PROTOCOLS else "unknown"
        record["context"] = state.context_guard.status(provider, model, protocol)
    return {"capabilities": records}
