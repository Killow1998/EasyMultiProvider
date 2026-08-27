"""Resolve one immutable request route from EMP configuration."""

from __future__ import annotations

import copy
from dataclasses import dataclass
from types import MappingProxyType
from typing import Any, Dict, Mapping, Optional, Tuple

from .capabilities import deployment_identity, endpoint_fingerprint, make_provenance
from .catalog import load_native_catalog
from .dialects import classify_dialect
from .router_errors import RouterError


EXPLICIT_MODEL = "explicit_model"
SUBSCRIPTION_ACCOUNT = "subscription_account"
FORWARD_PROVIDER = "forward_provider"
IMPLICIT_NATIVE = "implicit_native"


def _freeze(value: Any) -> Any:
    if isinstance(value, Mapping):
        return MappingProxyType({str(key): _freeze(item) for key, item in value.items()})
    if isinstance(value, list):
        return tuple(_freeze(item) for item in value)
    if isinstance(value, tuple):
        return tuple(_freeze(item) for item in value)
    if isinstance(value, set):
        return frozenset(_freeze(item) for item in value)
    return copy.deepcopy(value)


def _thaw(value: Any) -> Any:
    if isinstance(value, Mapping):
        return {key: _thaw(item) for key, item in value.items()}
    if isinstance(value, tuple):
        return [_thaw(item) for item in value]
    if isinstance(value, frozenset):
        return {_thaw(item) for item in value}
    return copy.deepcopy(value)


@dataclass(frozen=True)
class ResolvedRoute:
    """Configuration facts for one requested model and one concrete protocol.

    Provider and model values are recursively frozen snapshots. Request-local
    projection and transport state must use ``provider_copy``/``model_copy``;
    it must never be attached to this value.
    """

    requested_model: str
    upstream_model: str
    source: str
    provider: Mapping[str, Any]
    model: Mapping[str, Any]
    protocol: str
    dialect: str
    provider_id: str
    endpoint_fingerprint: str
    deployment_identity: str

    @classmethod
    def from_parts(
        cls,
        requested_model: str,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        source: str,
    ) -> "ResolvedRoute":
        provider_snapshot = _freeze(provider)
        model_snapshot = _freeze(model)
        upstream_model = resolved_upstream_model(
            provider_snapshot, model_snapshot, requested_model
        )
        return cls(
            requested_model=requested_model,
            upstream_model=upstream_model,
            source=source,
            provider=provider_snapshot,
            model=model_snapshot,
            protocol=str(provider_snapshot.get("protocol") or ""),
            dialect=classify_dialect(provider_snapshot),
            provider_id=str(provider_snapshot.get("id") or ""),
            endpoint_fingerprint=endpoint_fingerprint(
                provider_snapshot.get("base_url")
            ),
            deployment_identity=deployment_identity(
                provider_snapshot, model_snapshot
            ),
        )

    def provider_copy(self) -> Dict[str, Any]:
        return _thaw(self.provider)

    def model_copy(self) -> Dict[str, Any]:
        return _thaw(self.model)

    def with_protocol(self, protocol: str) -> "ResolvedRoute":
        if protocol == self.protocol:
            return self
        provider = self.provider_copy()
        provider["protocol"] = protocol
        return ResolvedRoute.from_parts(
            self.requested_model,
            provider,
            self.model,
            self.source,
        )


def resolved_upstream_model(
    provider: Mapping[str, Any], model: Mapping[str, Any], requested: str
) -> str:
    explicit = model.get("upstream_id", "")
    if explicit:
        explicit = str(explicit)
        prefix = str(provider.get("id", "")) + "/"
        return (
            explicit[len(prefix) :]
            if prefix != "/" and explicit.startswith(prefix)
            else explicit
        )
    prefix = str(provider.get("id", "")) + "/"
    return requested[len(prefix) :] if requested.startswith(prefix) else requested


def _native_route_model(
    native: Mapping[str, Any], requested_id: str, upstream_id: str
) -> Dict[str, Any]:
    model = copy.deepcopy(dict(native))
    model["id"] = requested_id
    model["upstream_id"] = upstream_id
    try:
        context_window = int(model.get("context_window", 0) or 0)
    except (TypeError, ValueError):
        context_window = 0
    if context_window > 0:
        raw_sources = model.get("capability_sources")
        sources = (
            copy.deepcopy(dict(raw_sources))
            if isinstance(raw_sources, Mapping)
            else {}
        )
        if not isinstance(sources.get("context_window"), Mapping):
            sources["context_window"] = make_provenance("official")
        model["capability_sources"] = sources
    return model


def _native_catalog_route_model(
    config: Dict[str, Any], requested_id: str, upstream_id: str
) -> Optional[Dict[str, Any]]:
    try:
        native = load_native_catalog(config)
    except Exception:
        return None
    for item in native.get("models", []):
        if not isinstance(item, Mapping) or item.get("slug") != upstream_id:
            continue
        if item.get("supported_in_api", True) is False:
            return None
        return _native_route_model(item, requested_id, upstream_id)
    return None


def _implicit_native_route(
    config: Dict[str, Any], model_id: str
) -> Optional[Tuple[Dict[str, Any], Dict[str, Any]]]:
    if "/" in model_id:
        return None
    native = load_native_catalog(config)
    for item in native.get("models", []):
        if not isinstance(item, dict) or item.get("slug") != model_id:
            continue
        if item.get("supported_in_api", True) is False:
            continue
        provider = {
            "id": "codex-native",
            "name": "Native Codex",
            "base_url": config.get(
                "codex_base_url", "https://chatgpt.com/backend-api/codex"
            ),
            "protocol": "responses",
            "auth_mode": "forward",
            "implicit_native": True,
        }
        native_auth_path = config.get("_native_auth_path")
        if isinstance(native_auth_path, str) and native_auth_path:
            provider["_native_auth_path"] = native_auth_path
        return provider, _native_route_model(item, model_id, model_id)
    return None


def resolve_route(config: Dict[str, Any], model_id: str) -> ResolvedRoute:
    """Resolve an explicit model, Subscription prefix, or implicit Native model."""

    for model in config.get("models", []):
        if model.get("enabled", True) and model.get("id") == model_id:
            providers = {item["id"]: item for item in config.get("providers", [])}
            provider = providers.get(model.get("provider"))
            if provider and provider.get("enabled", True):
                return ResolvedRoute.from_parts(
                    model_id, provider, model, EXPLICIT_MODEL
                )
            raise RouterError(
                "provider for model is missing or disabled: %s" % model_id, 503
            )

    for account in config.get("accounts", []):
        prefix = str(account.get("prefix", "")) + "/"
        if account.get("enabled", True) and prefix != "/" and model_id.startswith(prefix):
            upstream_id = model_id[len(prefix) :]
            if not upstream_id:
                break
            provider = {
                "id": account["id"],
                "name": account.get("name", account["id"]),
                "base_url": config.get(
                    "codex_base_url", "https://chatgpt.com/backend-api/codex"
                ),
                "protocol": "responses",
                "auth_mode": "account",
                "account": account,
            }
            model = _native_catalog_route_model(config, model_id, upstream_id)
            return ResolvedRoute.from_parts(
                model_id,
                provider,
                model or {"id": model_id, "upstream_id": upstream_id},
                SUBSCRIPTION_ACCOUNT,
            )

    forward = [
        item
        for item in config.get("providers", [])
        if item.get("enabled", True) and item.get("auth_mode") == "forward"
    ]
    if len(forward) == 1:
        return ResolvedRoute.from_parts(
            model_id,
            forward[0],
            {"id": model_id, "upstream_id": model_id},
            FORWARD_PROVIDER,
        )
    if not forward:
        implicit = _implicit_native_route(config, model_id)
        if implicit is not None:
            provider, model = implicit
            return ResolvedRoute.from_parts(
                model_id, provider, model, IMPLICIT_NATIVE
            )
    raise RouterError("unknown model: %s" % model_id, 404)
