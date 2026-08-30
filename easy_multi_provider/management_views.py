"""Credential-free Web management projections."""

from __future__ import annotations

import re
from typing import Any, Dict, Mapping

from . import __version__
from .capabilities import safe_capability_list
from .catalog import (
    build_catalog,
    has_explicit_family_identity,
    load_native_catalog,
    model_family_identity,
    subscription_model_options,
)
from .config import public_config


_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages", "unknown"}
)


def management_config(
    config: Dict[str, Any], native_account: Dict[str, Any] | None = None
) -> Dict[str, Any]:
    result = public_config(config)
    result["emp_version"] = __version__
    result["subscription_models"] = subscription_model_options(config)
    catalog_models = management_catalog_models(config)
    result["catalog_models"] = catalog_models
    result["catalog_families"] = management_catalog_families(
        config, catalog_models
    )
    if native_account is not None:
        result["native_account"] = dict(native_account)
    return result


def management_catalog_models(config: Dict[str, Any]) -> list[Dict[str, Any]]:
    """Return the small, credential-free model view used by display settings."""

    baseline_config = dict(config)
    baseline_config["catalog_presentations"] = {}
    baseline_config["catalog_family_presentations"] = {}
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
    external_models = {
        str(item.get("id") or ""): item
        for item in config.get("models", [])
        if isinstance(item, Mapping)
    }
    native_models = {
        str(item.get("slug") or ""): item
        for item in load_native_catalog(config).get("models", [])
        if isinstance(item, Mapping) and item.get("slug")
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
        source_model: Mapping[str, Any] = native_models.get(route, {})
        family_fallback = route
        family_verified = True
        if route in external_sources:
            source_type = "provider"
            source_id = external_sources[route]
            source_model = external_models.get(route, {})
            family_verified = has_explicit_family_identity(dict(source_model))
        else:
            for prefix, account_id in account_prefixes:
                if route.startswith(prefix + "/"):
                    source_type = "account"
                    source_id = account_id
                    family_fallback = route[len(prefix) + 1 :]
                    source_model = native_models.get(family_fallback, {})
                    break
        family_id = model_family_identity(dict(source_model), family_fallback)
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
                "family_id": family_id,
                "family_verified": family_verified,
                "supports_reasoning_summaries": (
                    item.get("supports_reasoning_summary_parameter") is True
                ),
            }
        )
    return rows


def management_catalog_families(
    config: Dict[str, Any], catalog_models: list[Dict[str, Any]] | None = None
) -> list[Dict[str, Any]]:
    """Group presentation controls by canonical model family, not account route."""

    groups: Dict[str, Dict[str, Any]] = {}
    order = {"native": 0, "account": 1, "provider": 2}
    for row in catalog_models if catalog_models is not None else management_catalog_models(config):
        family_id = str(row.get("family_id") or row["id"])
        group = groups.setdefault(
            family_id,
            {
                "id": family_id,
                "default_display_name": row["default_display_name"],
                "context_window": row["context_window"],
                "supports_reasoning_summaries": True,
                "routes": [],
            },
        )
        group["routes"].append(
            {
                "id": row["id"],
                "source_type": row["source_type"],
                "source_id": row["source_id"],
            }
        )
        if order.get(row["source_type"], 9) < min(
            (
                order.get(item["source_type"], 9)
                for item in group["routes"][:-1]
            ),
            default=9,
        ):
            group["default_display_name"] = row["default_display_name"]
            group["context_window"] = row["context_window"]
        group["supports_reasoning_summaries"] = bool(
            group["supports_reasoning_summaries"]
            and row["supports_reasoning_summaries"]
        )
    presentations = config.get("catalog_family_presentations", {})
    route_presentations = config.get("catalog_presentations", {})
    for group in groups.values():
        presentation = (
            presentations.get(group["id"], {})
            if isinstance(presentations, Mapping)
            else {}
        )
        if not presentation and isinstance(route_presentations, Mapping):
            routes = group.get("routes", [])
            preferred_route = routes[0].get("id") if routes else ""
            candidate = route_presentations.get(preferred_route, {})
            presentation = candidate if isinstance(candidate, Mapping) else {}
        group["presentation"] = {
            "catalog_alias": str(presentation.get("catalog_alias") or ""),
            "show_context": presentation.get("show_context", True) is not False,
            "reasoning_summary": str(
                presentation.get("reasoning_summary") or "auto"
            ),
        }
        alias = presentation.get("catalog_alias", "")
        group["display_name"] = alias or group["default_display_name"]
    return list(groups.values())


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
