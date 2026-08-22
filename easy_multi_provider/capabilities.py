"""Pure, safe capability records for configured Provider/model pairs."""

from __future__ import annotations

import hashlib
import math
import posixpath
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Dict, Iterable, Mapping, Optional, Tuple
from urllib.parse import quote, urlsplit


CAPABILITY_SOURCES = frozenset(
    {"official", "advertised", "observed", "manual", "inferred", "unknown"}
)
CAPABILITY_NAMES = (
    "configured_protocol",
    "effective_protocol",
    "streaming",
    "structured_tools",
    "parallel_tools",
    "reasoning_levels",
    "context_window",
    "output_limit",
    "websocket",
    "input_modalities",
    "supports_image_detail_original",
)
KNOWN_PROTOCOLS = frozenset(
    {"auto", "responses", "chat_completions", "anthropic_messages"}
)
_SAFE_ID = re.compile(r"^[A-Za-z0-9._/-]{1,256}$")
_INPUT_MODALITY_ID = re.compile(r"^[a-z0-9][a-z0-9._:-]{0,63}$")
MAX_INPUT_MODALITIES = 16
MAX_INPUT_MODALITY_ID_BYTES = 64
TEXT_MODALITY = "text"
IMAGE_MODALITY = "image"
DEFAULT_INPUT_MODALITIES = (TEXT_MODALITY,)
CODEX_INPUT_MODALITIES = frozenset({TEXT_MODALITY, IMAGE_MODALITY})
_DEFAULT_CONFIDENCE = {
    "official": 0.95,
    "advertised": 0.75,
    "observed": 1.0,
    "manual": 1.0,
    "inferred": 0.35,
    "unknown": 0.0,
}


def _parse_input_modalities(value: Any) -> Optional[list]:
    if not isinstance(value, list) or not value or len(value) > MAX_INPUT_MODALITIES:
        return None
    result = []
    for item in value:
        if not isinstance(item, str):
            return None
        identifier = item.strip().lower()
        if (
            not identifier
            or len(identifier.encode("utf-8")) > MAX_INPUT_MODALITY_ID_BYTES
            or not _INPUT_MODALITY_ID.fullmatch(identifier)
        ):
            return None
        if identifier not in result:
            result.append(identifier)
    return result or None


def normalize_input_modalities(value: Any) -> list:
    """Return bounded internal modality identifiers with a text fallback."""

    parsed = _parse_input_modalities(value)
    return parsed if parsed is not None else list(DEFAULT_INPUT_MODALITIES)


def input_modalities_known(value: Any) -> bool:
    return _parse_input_modalities(value) is not None


def input_modalities_metadata_source(value: Any) -> str:
    """Classify whether a discovery payload advertised a valid modality list."""

    return "advertised" if _parse_input_modalities(value) is not None else "unknown"


def codex_input_modalities(value: Any) -> list:
    """Project internal modalities onto the Codex text/image catalog contract."""

    normalized = normalize_input_modalities(value)
    projected = [
        item
        for item in (TEXT_MODALITY, IMAGE_MODALITY)
        if item in CODEX_INPUT_MODALITIES and item in normalized
    ]
    return projected or list(DEFAULT_INPUT_MODALITIES)


def observed_at_now() -> str:
    """Return a compact UTC timestamp suitable for persisted provenance."""

    return datetime.now(timezone.utc).isoformat()


def make_provenance(
    source: str = "unknown",
    confidence: Optional[float] = None,
    observed_at: Optional[str] = None,
) -> Dict[str, Any]:
    if source not in CAPABILITY_SOURCES:
        raise ValueError("unsupported capability source")
    value = _DEFAULT_CONFIDENCE[source] if confidence is None else float(confidence)
    if not math.isfinite(value) or not 0.0 <= value <= 1.0:
        raise ValueError("capability confidence must be between 0 and 1")
    if observed_at is not None and not isinstance(observed_at, str):
        raise ValueError("capability observed_at must be a string or null")
    if observed_at is not None:
        try:
            datetime.fromisoformat(observed_at.replace("Z", "+00:00"))
        except ValueError as exc:
            raise ValueError("capability observed_at must be an ISO timestamp") from exc
    return {
        "source": source,
        "confidence": value,
        "observed_at": observed_at,
    }


def normalize_endpoint(endpoint: Any) -> str:
    """Canonicalize endpoint identity without query, fragment, or userinfo."""

    try:
        parsed = urlsplit(str(endpoint or "").strip())
        scheme = parsed.scheme.lower()
        hostname = parsed.hostname
        if scheme not in ("http", "https") or not hostname:
            return ""
        hostname = hostname.encode("idna").decode("ascii").lower()
        port = parsed.port
    except (UnicodeError, ValueError):
        return ""
    default_port = 80 if scheme == "http" else 443
    host = "[%s]" % hostname if ":" in hostname and not hostname.startswith("[") else hostname
    port_part = "" if port in (None, default_port) else ":%d" % port
    path = parsed.path or "/"
    normalized_path = posixpath.normpath("/" + path.lstrip("/"))
    if normalized_path == ".":
        normalized_path = "/"
    return "%s://%s%s%s" % (scheme, host, port_part, normalized_path.rstrip("/") or "/")


def endpoint_fingerprint(endpoint: Any) -> str:
    """Hash the canonical endpoint identity; never return the endpoint itself."""

    canonical = normalize_endpoint(endpoint) or "invalid-endpoint"
    return "sha256:" + hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def deployment_identity(
    provider: Mapping[str, Any], model: Mapping[str, Any]
) -> str:
    for source in (model, provider):
        for field in ("deployment_identity", "deployment_id", "deployment"):
            value = source.get(field)
            if isinstance(value, str) and value.strip() and _SAFE_ID.fullmatch(value.strip()):
                return value.strip()
    return "default"


@dataclass(frozen=True)
class CapabilityValue:
    value: Any
    source: str
    confidence: float
    observed_at: Optional[str]

    def __post_init__(self) -> None:
        if self.source not in CAPABILITY_SOURCES:
            raise ValueError("unsupported capability source")
        if not math.isfinite(float(self.confidence)) or not 0.0 <= float(self.confidence) <= 1.0:
            raise ValueError("capability confidence must be between 0 and 1")
        if self.observed_at is not None and not isinstance(self.observed_at, str):
            raise ValueError("capability observed_at must be a string or null")

    def to_dict(self) -> Dict[str, Any]:
        return {
            "value": self.value,
            "source": self.source,
            "confidence": float(self.confidence),
            "observed_at": self.observed_at,
        }


@dataclass(frozen=True)
class CapabilityKey:
    endpoint_fingerprint: str
    upstream_model: str
    protocol_identity: str
    deployment_identity: str

    def to_dict(self) -> Dict[str, str]:
        return {
            "endpoint_fingerprint": self.endpoint_fingerprint,
            "upstream_model": self.upstream_model,
            "protocol_identity": self.protocol_identity,
            "deployment_identity": self.deployment_identity,
        }

    @property
    def key_id(self) -> str:
        return "cap:v1:%s:%s:%s:%s" % (
            self.endpoint_fingerprint,
            quote(self.upstream_model, safe=""),
            quote(self.protocol_identity, safe=""),
            quote(self.deployment_identity, safe=""),
        )


@dataclass(frozen=True)
class CapabilityRecord:
    key: CapabilityKey
    provider_id: str
    model_id: str
    capabilities: Mapping[str, CapabilityValue]

    def to_dict(self) -> Dict[str, Any]:
        return {
            "key": self.key.to_dict(),
            "key_id": self.key.key_id,
            "provider_id": self.provider_id,
            "model_id": self.model_id,
            "capabilities": {
                name: self.capabilities[name].to_dict() for name in CAPABILITY_NAMES
            },
        }


def _safe_id(value: Any, fallback: str = "unknown") -> str:
    text = str(value or "").strip()
    return text if _SAFE_ID.fullmatch(text) else fallback


def _provenance(source: Mapping[str, Any], field: str, default_source: str) -> Dict[str, Any]:
    raw = source.get("capability_sources", {})
    value = raw.get(field) if isinstance(raw, Mapping) else None
    if not isinstance(value, Mapping):
        return make_provenance(default_source)
    try:
        return make_provenance(
            value.get("source", default_source),
            value.get("confidence"),
            value.get("observed_at"),
        )
    except (TypeError, ValueError):
        return make_provenance("unknown")


def _capability(
    value: Any,
    source: Mapping[str, Any],
    field: str,
    known: bool,
    default_source: str = "inferred",
) -> CapabilityValue:
    provenance = _provenance(source, field, default_source if known else "unknown")
    if not known or provenance["source"] == "unknown":
        provenance = make_provenance("unknown", observed_at=provenance["observed_at"])
        value = "unknown"
    return CapabilityValue(
        value,
        provenance["source"],
        provenance["confidence"],
        provenance["observed_at"],
    )


def _lookup_capability(
    provider: Mapping[str, Any], model: Mapping[str, Any], names: Iterable[str]
) -> Tuple[Any, Mapping[str, Any]]:
    for source in (model, provider):
        nested = source.get("capabilities")
        for name in names:
            if name in source:
                return source[name], source
            if isinstance(nested, Mapping) and name in nested:
                value = nested[name]
                if isinstance(value, Mapping) and "value" in value:
                    return value["value"], {"capability_sources": {name: value}}
                return value, source
    return None, model


def _resolved_protocol(
    provider: Mapping[str, Any], model: Mapping[str, Any], endpoint_fp: str, deployment: str
) -> Tuple[str, Dict[str, Any]]:
    upstream = _safe_id(model.get("upstream_id") or model.get("id"))
    candidates = (model, provider)
    for source in candidates:
        protocol = source.get("resolved_protocol")
        if protocol not in KNOWN_PROTOCOLS - {"auto"}:
            continue
        observation = source.get("protocol_observation", {})
        if not isinstance(observation, Mapping):
            continue
        stored_fp = observation.get("endpoint_fingerprint")
        stored_deployment = observation.get("deployment_identity")
        stored_upstream = observation.get("upstream_model")
        if stored_fp and stored_fp != endpoint_fp:
            continue
        if stored_deployment and stored_deployment != deployment:
            continue
        if stored_upstream and stored_upstream != upstream:
            continue
        try:
            return protocol, make_provenance(
                observation.get("source", "observed"),
                observation.get("confidence", 1.0),
                observation.get("observed_at"),
            )
        except (TypeError, ValueError):
            continue
    return "unknown", make_provenance("unknown")


def capability_record(
    provider: Mapping[str, Any],
    model: Mapping[str, Any],
    provider_id: Optional[str] = None,
    model_id: Optional[str] = None,
) -> CapabilityRecord:
    """Build a capability record without retaining any provider secret material."""

    endpoint_fp = endpoint_fingerprint(provider.get("base_url"))
    upstream = _safe_id(model.get("upstream_id") or model.get("id"))
    deployment = deployment_identity(provider, model)
    configured = provider.get("protocol")
    configured_value = configured if configured in KNOWN_PROTOCOLS else "unknown"
    resolved, resolved_provenance = _resolved_protocol(provider, model, endpoint_fp, deployment)
    if resolved_provenance["source"] == "unknown":
        resolved = "unknown"
    effective = configured_value if configured_value != "auto" else resolved
    configured_source = "manual" if configured_value != "unknown" else "unknown"
    capabilities: Dict[str, CapabilityValue] = {
        "configured_protocol": CapabilityValue(
            configured_value,
            configured_source,
            1.0 if configured_source == "manual" else 0.0,
            None,
        ),
        "effective_protocol": CapabilityValue(
            effective,
            resolved_provenance["source"] if effective == resolved else configured_source,
            resolved_provenance["confidence"] if effective == resolved else 1.0,
            resolved_provenance["observed_at"] if effective == resolved else None,
        ),
    }

    streaming, streaming_source = _lookup_capability(
        provider, model, ("streaming", "supports_streaming", "supports_stream")
    )
    structured, structured_source = _lookup_capability(
        provider,
        model,
        ("structured_tools", "supports_structured_tools", "supports_tools"),
    )
    parallel, parallel_source = _lookup_capability(
        provider, model, ("parallel_tools", "supports_parallel_tools")
    )
    websocket, websocket_source = _lookup_capability(
        provider, model, ("websocket", "supports_websocket")
    )
    reasoning = model.get("reasoning_levels")
    reasoning_known = isinstance(reasoning, list) and bool(reasoning)
    if reasoning_known:
        reasoning = [str(level) for level in reasoning if str(level).strip()]
        reasoning_known = bool(reasoning)
    context = model.get("context_window")
    context_known = isinstance(context, int) and not isinstance(context, bool) and context > 0
    output_limit = model.get("output_limit")
    output_known = (
        isinstance(output_limit, int) and not isinstance(output_limit, bool) and output_limit > 0
    )
    input_modalities = model.get("input_modalities")
    input_modalities_known = _parse_input_modalities(input_modalities) is not None
    if input_modalities_known:
        input_modalities = normalize_input_modalities(input_modalities)
    supports_image_detail_original = model.get("supports_image_detail_original")
    supports_image_detail_original_known = isinstance(
        supports_image_detail_original, bool
    )
    capabilities.update(
        {
            "streaming": _capability(
                streaming, streaming_source, "streaming", isinstance(streaming, bool)
            ),
            "structured_tools": _capability(
                structured, structured_source, "structured_tools", isinstance(structured, bool)
            ),
            "parallel_tools": _capability(
                parallel, parallel_source, "parallel_tools", isinstance(parallel, bool)
            ),
            "reasoning_levels": _capability(
                reasoning, model, "reasoning_levels", reasoning_known
            ),
            "context_window": _capability(
                context, model, "context_window", context_known
            ),
            "output_limit": _capability(
                output_limit, model, "output_limit", output_known
            ),
            "websocket": _capability(
                websocket, websocket_source, "websocket", isinstance(websocket, bool)
            ),
            "input_modalities": _capability(
                input_modalities,
                model,
                "input_modalities",
                input_modalities_known,
            ),
            "supports_image_detail_original": _capability(
                supports_image_detail_original,
                model,
                "supports_image_detail_original",
                supports_image_detail_original_known,
            ),
        }
    )
    return CapabilityRecord(
        CapabilityKey(
            endpoint_fp,
            upstream,
            effective if effective != "unknown" else configured_value,
            deployment,
        ),
        _safe_id(provider_id or provider.get("id")),
        _safe_id(model_id or model.get("id")),
        capabilities,
    )


def capability_records(config: Mapping[str, Any]) -> Tuple[CapabilityRecord, ...]:
    """Return records for enabled configured providers and enabled models only."""

    providers = {
        item.get("id"): item
        for item in config.get("providers", [])
        if isinstance(item, Mapping) and item.get("enabled", True)
    }
    records = []
    for model in config.get("models", []):
        if not isinstance(model, Mapping) or not model.get("enabled", True):
            continue
        provider = providers.get(model.get("provider"))
        if provider is None:
            continue
        records.append(capability_record(provider, model))
    return tuple(records)


def safe_capability_list(config: Mapping[str, Any]) -> list:
    """Serialize only the safe capability projection for management APIs."""

    return [record.to_dict() for record in capability_records(config)]


# Explicit aliases keep the small functional API discoverable to callers.
build_capability_record = capability_record
build_capability_records = capability_records
