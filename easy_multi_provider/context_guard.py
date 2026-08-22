"""Bounded, content-free context budgeting and calibration helpers.

The guard intentionally operates on the translated upstream payload only long
enough to calculate a conservative size estimate.  It retains no request
content; persisted calibration contains only numeric boundaries and safe
capability identity fields.
"""

from __future__ import annotations

import json
import math
import re
from dataclasses import dataclass
from datetime import datetime, timezone
from typing import Any, Dict, Iterable, Mapping, MutableMapping, Optional, Tuple

from .capabilities import deployment_identity, endpoint_fingerprint, observed_at_now


CONCRETE_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages"}
)
CALIBRATION_CAPACITY = 8
SAFETY_RESERVE_TOKENS = 256
CLEAR_EXCESS_TOKENS = 64
_SAFE_ID = re.compile(r"^[A-Za-z0-9._/-]{1,256}$")
_FINGERPRINT = re.compile(r"^sha256:[0-9a-f]{64}$")
_SOURCES = frozenset(
    {"official", "advertised", "observed", "manual", "inferred", "unknown"}
)


def _safe_id(value: Any, fallback: str = "unknown") -> str:
    text = str(value or "").strip()
    return text if _SAFE_ID.fullmatch(text) else fallback


def _positive_int(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        number = int(value)
    except (TypeError, ValueError):
        return None
    return number if number > 0 else None


def _nonnegative_int(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        number = int(value)
    except (TypeError, ValueError):
        return None
    return number if number >= 0 else None


def _valid_timestamp(value: Any) -> Optional[str]:
    if not isinstance(value, str) or not value:
        return None
    try:
        datetime.fromisoformat(value.replace("Z", "+00:00"))
    except ValueError:
        return None
    return value


@dataclass(frozen=True)
class ContextIdentity:
    endpoint_fingerprint: str
    upstream_model: str
    protocol: str
    deployment_identity: str

    def to_dict(self) -> Dict[str, str]:
        return {
            "endpoint_fingerprint": self.endpoint_fingerprint,
            "upstream_model": self.upstream_model,
            "protocol": self.protocol,
            "deployment_identity": self.deployment_identity,
        }


def context_identity(
    provider: Mapping[str, Any], model: Mapping[str, Any], protocol: str
) -> ContextIdentity:
    """Build the exact identity used for context calibration."""

    upstream = _safe_id(model.get("upstream_id") or model.get("id"))
    concrete = protocol if protocol in CONCRETE_PROTOCOLS else "unknown"
    return ContextIdentity(
        endpoint_fingerprint(provider.get("base_url")),
        upstream,
        concrete,
        _safe_id(deployment_identity(provider, model), "default"),
    )


def _source_for(source: Mapping[str, Any], field: str) -> Tuple[str, float, Optional[str]]:
    values = source.get("capability_sources", {})
    raw = values.get(field) if isinstance(values, Mapping) else None
    if not isinstance(raw, Mapping):
        return "inferred", 0.35, None
    name = raw.get("source", "unknown")
    if name not in _SOURCES:
        return "unknown", 0.0, None
    try:
        confidence = float(raw.get("confidence", {
            "official": 0.95,
            "advertised": 0.75,
            "observed": 1.0,
            "manual": 1.0,
            "inferred": 0.35,
            "unknown": 0.0,
        }[name]))
    except (KeyError, TypeError, ValueError):
        return "unknown", 0.0, None
    if not math.isfinite(confidence) or not 0.0 <= confidence <= 1.0:
        return "unknown", 0.0, None
    return name, confidence, _valid_timestamp(raw.get("observed_at"))


def _context_window(
    provider: Mapping[str, Any], model: Mapping[str, Any]
) -> Tuple[Optional[int], str, float, Optional[str]]:
    for source in (model, provider):
        value = _positive_int(source.get("context_window"))
        if value is None:
            continue
        provenance_source, confidence, observed_at = _source_for(source, "context_window")
        if provenance_source == "unknown":
            continue
        return value, provenance_source, confidence, observed_at
    return None, "unknown", 0.0, None


def _matching_identity(raw: Mapping[str, Any], identity: ContextIdentity) -> bool:
    return all(
        raw.get(field) == value
        for field, value in identity.to_dict().items()
    )


def _valid_calibration(raw: Any) -> Optional[Dict[str, Any]]:
    if not isinstance(raw, Mapping):
        return None
    fingerprint = raw.get("endpoint_fingerprint")
    if not isinstance(fingerprint, str) or not _FINGERPRINT.fullmatch(fingerprint):
        return None
    protocol = raw.get("protocol")
    if protocol not in CONCRETE_PROTOCOLS:
        return None
    upstream = _safe_id(raw.get("upstream_model"), "")
    deployment = _safe_id(raw.get("deployment_identity"), "")
    if not upstream or not deployment:
        return None
    largest = _positive_int(raw.get("largest_success_estimate"))
    smallest = _positive_int(raw.get("smallest_failure_estimate"))
    result = {
        "endpoint_fingerprint": fingerprint,
        "upstream_model": upstream,
        "protocol": protocol,
        "deployment_identity": deployment,
        "largest_success_estimate": largest,
        "smallest_failure_estimate": smallest,
        "largest_success_source": "observed" if largest is not None else "unknown",
        "smallest_failure_source": "observed" if smallest is not None else "unknown",
        "largest_success_confidence": 1.0 if largest is not None else 0.0,
        "smallest_failure_confidence": 1.0 if smallest is not None else 0.0,
        "largest_success_observed_at": _valid_timestamp(
            raw.get("largest_success_observed_at")
        ),
        "smallest_failure_observed_at": _valid_timestamp(
            raw.get("smallest_failure_observed_at")
        ),
    }
    return result


def calibration_for(
    provider: Mapping[str, Any], model: Mapping[str, Any], protocol: str
) -> Optional[Dict[str, Any]]:
    identity = context_identity(provider, model, protocol)
    for source in (model, provider):
        raw_values = source.get("context_calibrations", [])
        if not isinstance(raw_values, list):
            continue
        for raw in raw_values:
            value = _valid_calibration(raw)
            if value is not None and _matching_identity(value, identity):
                return value
    return None


def _calibration_from_observation(observation: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    identity = {
        "endpoint_fingerprint": observation.get("endpoint_fingerprint"),
        "upstream_model": observation.get("upstream_model"),
        "protocol": observation.get("protocol"),
        "deployment_identity": observation.get("deployment_identity"),
    }
    if (
        not isinstance(identity["endpoint_fingerprint"], str)
        or not _FINGERPRINT.fullmatch(identity["endpoint_fingerprint"])
        or identity["protocol"] not in CONCRETE_PROTOCOLS
    ):
        return None
    if not _safe_id(identity["upstream_model"], "") or not _safe_id(
        identity["deployment_identity"], ""
    ):
        return None
    return _valid_calibration({**identity}) or {
        **identity,
        "largest_success_estimate": None,
        "smallest_failure_estimate": None,
        "largest_success_source": "unknown",
        "smallest_failure_source": "unknown",
        "largest_success_confidence": 0.0,
        "smallest_failure_confidence": 0.0,
        "largest_success_observed_at": None,
        "smallest_failure_observed_at": None,
    }


def update_calibration(
    model: MutableMapping[str, Any],
    observation: Mapping[str, Any],
    outcome: str,
    estimate: Any,
    observed_at: Optional[str] = None,
) -> bool:
    """Monotonically update one model's numeric calibration entry."""

    if outcome not in ("success", "explicit_failure"):
        return False
    value = _positive_int(estimate)
    entry = _calibration_from_observation(observation)
    if value is None or entry is None:
        return False
    entries = []
    raw_values = model.get("context_calibrations", [])
    if isinstance(raw_values, list):
        for raw in raw_values:
            clean = _valid_calibration(raw)
            if clean is not None:
                entries.append(clean)
    current = next(
        (item for item in entries if _matching_identity(item, ContextIdentity(**{
            field: entry[field]
            for field in ("endpoint_fingerprint", "upstream_model", "protocol", "deployment_identity")
        }))),
        None,
    )
    if current is None:
        current = entry
        entries.append(current)
    timestamp = _valid_timestamp(observed_at) or observed_at_now()
    changed = False
    if outcome == "success":
        old = current.get("largest_success_estimate")
        if old is None or value > old:
            current["largest_success_estimate"] = value
            current["largest_success_source"] = "observed"
            current["largest_success_confidence"] = 1.0
            current["largest_success_observed_at"] = timestamp
            changed = True
    else:
        old = current.get("smallest_failure_estimate")
        if old is None or value < old:
            current["smallest_failure_estimate"] = value
            current["smallest_failure_source"] = "observed"
            current["smallest_failure_confidence"] = 1.0
            current["smallest_failure_observed_at"] = timestamp
            changed = True
    if not changed:
        return False
    # Keep the newest bounded set while retaining the entry just updated.
    entries = entries[-CALIBRATION_CAPACITY:]
    if current not in entries:
        entries[-1] = current
    model["context_calibrations"] = entries
    return True


def _payload_view(payload: Mapping[str, Any], protocol: str) -> Optional[Dict[str, Any]]:
    if not isinstance(payload, Mapping) or protocol not in CONCRETE_PROTOCOLS:
        return None
    fields = {
        "responses": ("input", "instructions", "tools", "text", "response_format"),
        "chat_completions": ("messages", "tools", "response_format"),
        "anthropic_messages": ("system", "messages", "tools"),
    }[protocol]
    view = {field: payload[field] for field in fields if field in payload}
    return view


def estimate_input_tokens(payload: Mapping[str, Any], protocol: str) -> Optional[int]:
    """Conservatively estimate translated input tokens, including tool schemas."""

    view = _payload_view(payload, protocol)
    if view is None:
        return None
    try:
        encoded = json.dumps(
            view, ensure_ascii=False, sort_keys=True, separators=(",", ":")
        ).encode("utf-8")
    except (TypeError, ValueError, OverflowError):
        return None
    if not encoded:
        return 0
    # Two UTF-8 bytes per token intentionally overestimates common BPE inputs.
    return max(1, int(math.ceil(len(encoded) / 2.0)))


def estimate_method() -> str:
    return "translated_json_utf8_bytes_div_2_ceil"


def _output_reserve(
    payload: Mapping[str, Any], model: Optional[Mapping[str, Any]] = None
) -> Optional[int]:
    for field in ("max_output_tokens", "max_tokens"):
        value = _positive_int(payload.get(field))
        if value is not None:
            return value
    if isinstance(model, Mapping):
        return _positive_int(model.get("output_limit"))
    return None


@dataclass(frozen=True)
class ContextAssessment:
    identity: ContextIdentity
    provider_id: str
    model_id: str
    estimate_method: str
    input_estimate: Optional[int]
    output_reserve: Optional[int]
    safety_reserve: int
    reserves: Optional[int]
    context_limit: Optional[int]
    safe_input_limit: Optional[int]
    confidence: float
    source: str
    completeness: str
    decision: str
    next_action: str
    reason: str
    explicit_failure: bool = False

    @property
    def context_decision(self) -> str:
        return {
            "allow": "allowed",
            "warn": "warned",
            "block": "blocked",
        }.get(self.decision, "unknown")

    def to_safe_dict(self) -> Dict[str, Any]:
        return {
            "provider_id": self.provider_id,
            "model_id": self.model_id,
            **self.identity.to_dict(),
            "estimate_method": self.estimate_method,
            "input_estimate": self.input_estimate,
            "estimated_tokens": self.input_estimate,
            "output_reserve": self.output_reserve,
            "safety_reserve": self.safety_reserve,
            "reserves": self.reserves,
            "context_limit": self.context_limit,
            "safe_input_limit": self.safe_input_limit,
            "confidence": self.confidence,
            "source": self.source,
            "completeness": self.completeness,
            "decision": self.decision,
            "context_decision": self.context_decision,
            "next_action": self.next_action,
            "reason": self.reason,
            "explicit_failure": self.explicit_failure,
        }


def _safe_limit(
    provider: Mapping[str, Any],
    model: Mapping[str, Any],
    protocol: str,
    reserves: Optional[int],
) -> Tuple[Optional[int], Optional[int], str, float, Optional[Dict[str, Any]]]:
    """Return total context and the effective *input* ceiling separately.

    ``context_window`` is a total budget.  A successful request only proves a
    lower bound and therefore never participates in the ceiling calculation.
    An explicit failure is different: its estimate is an input estimate, so
    ``failure - 1`` is already an input ceiling and must not have reserves
    subtracted from it again.
    """

    context_limit, base_source, base_confidence, _ = _context_window(provider, model)
    calibration = calibration_for(provider, model, protocol)
    failure = calibration.get("smallest_failure_estimate") if calibration else None
    failure_bound = max(0, failure - 1) if failure is not None else None
    base_input_limit = (
        max(0, context_limit - reserves)
        if context_limit is not None and reserves is not None
        else None
    )

    if failure_bound is not None and (
        base_input_limit is None or failure_bound <= base_input_limit
    ):
        return context_limit, failure_bound, "observed", 1.0, calibration
    if base_input_limit is not None:
        return context_limit, base_input_limit, base_source, base_confidence, calibration
    if context_limit is not None:
        # The total window is known, but no output reserve was available to
        # derive a safe input budget.  Keep that uncertainty explicit.
        return context_limit, None, base_source, base_confidence, calibration
    return None, None, "unknown", 0.0, calibration


def assess_context(
    provider: Mapping[str, Any],
    model: Mapping[str, Any],
    protocol: str,
    payload: Mapping[str, Any],
    completeness: str = "high",
) -> ContextAssessment:
    identity = context_identity(provider, model, protocol)
    provider_id = _safe_id(provider.get("id"))
    model_id = _safe_id(model.get("id"))
    input_estimate = estimate_input_tokens(payload, protocol)
    output_reserve = (
        _output_reserve(payload, model) if isinstance(payload, Mapping) else None
    )
    reserves = (
        output_reserve + SAFETY_RESERVE_TOKENS
        if output_reserve is not None
        else None
    )
    context_limit, safe_input_limit, source, confidence, calibration = _safe_limit(
        provider, model, protocol, reserves
    )
    completeness = completeness if completeness in ("high", "lost", "unknown") else "unknown"
    if completeness != "high":
        confidence = min(confidence, 0.25)
    if input_estimate is None:
        confidence = 0.0
    decision = "allow"
    reason = "translated payload is within the known safe budget"
    next_action = "continue"
    if input_estimate is None or completeness != "high":
        decision = "warn"
        reason = "context estimate or connection-local history is incomplete"
        next_action = "continue only with bounded state; use native compaction if needed"
    elif safe_input_limit is None:
        decision = "warn"
        reason = "upstream context limit or output reserve is unknown"
        next_action = "continue; reduce input or use native remote compaction if rejected"
    elif input_estimate > safe_input_limit:
        excess = input_estimate - safe_input_limit
        clear = excess >= max(CLEAR_EXCESS_TOKENS, int(max(1, safe_input_limit) * 0.01))
        if confidence >= 0.75 and clear:
            decision = "block"
            reason = "estimated input clearly exceeds the safe input limit"
            next_action = "reduce input or use native remote compaction, then retry"
        else:
            decision = "warn"
            reason = "estimated input is over a low-confidence or near-boundary limit"
            next_action = "reduce input or use native remote compaction if rejected"
    elif calibration and calibration.get("largest_success_estimate") is not None:
        reason = "within the known limit; successful boundary is retained for calibration"
    return ContextAssessment(
        identity,
        provider_id,
        model_id,
        estimate_method(),
        input_estimate,
        output_reserve,
        SAFETY_RESERVE_TOKENS,
        reserves,
        context_limit,
        safe_input_limit,
        max(0.0, min(1.0, confidence)),
        source,
        completeness,
        decision,
        next_action,
        reason,
    )


def mark_explicit_failure(
    observation: Mapping[str, Any], attempted_estimate: Optional[int] = None
) -> Dict[str, Any]:
    value = dict(observation)
    estimate = _positive_int(attempted_estimate)
    if estimate is not None:
        value["input_estimate"] = estimate
        value["estimated_tokens"] = value["input_estimate"]
        prior_limit = _nonnegative_int(value.get("safe_input_limit"))
        boundary = max(0, estimate - 1)
        value["safe_input_limit"] = (
            min(prior_limit, boundary) if prior_limit is not None else boundary
        )
        if prior_limit is not None and prior_limit <= boundary:
            # The existing base bound is already the controlling bound.  Do
            # not relabel it as observed merely because a failure was seen.
            pass
        else:
            value["source"] = "observed"
            value["confidence"] = 1.0
    value["explicit_failure"] = True
    value["decision"] = "block"
    value["context_decision"] = "blocked"
    value["next_action"] = "reduce input or use native remote compaction, then retry"
    value["reason"] = "upstream explicitly rejected the context length"
    return value


def format_context_error(observation: Mapping[str, Any]) -> str:
    provider = _safe_id(observation.get("provider_id"))
    model = _safe_id(observation.get("model_id"))
    estimate = _positive_int(observation.get("input_estimate"))
    limit = _nonnegative_int(observation.get("safe_input_limit"))
    estimate_text = str(estimate) if estimate is not None else "unknown"
    limit_text = str(limit) if limit is not None else "unknown"
    return (
        "context length exceeded: estimated input %s tokens, safe input limit %s; "
        "provider %s, model %s; next action: reduce input or use native remote compaction"
        % (estimate_text, limit_text, provider, model)
    )


def is_explicit_context_error(status: Any, content_type: Any, raw: bytes) -> bool:
    """Classify only structured provider evidence, never generic/WAF HTML."""

    if isinstance(status, int) and status not in (200, 400, 413, 422):
        # Authentication, rate-limit, protocol-rejection, and server/WAF
        # statuses are never context evidence, even when their JSON mentions it.
        return False
    if not isinstance(raw, (bytes, bytearray)):
        return False
    media = str(content_type or "").lower()
    stripped = bytes(raw).lstrip()
    if "json" not in media and not stripped.startswith((b"{", b"[")):
        return False
    try:
        value = json.loads(bytes(raw).decode("utf-8", "replace"))
    except (TypeError, ValueError, UnicodeDecodeError):
        return False
    return _context_evidence(value)


def _context_evidence(value: Any) -> bool:
    if isinstance(value, Mapping):
        for key, item in value.items():
            name = str(key).lower().replace("-", "_").replace(" ", "_")
            text = str(item or "").lower()
            compact = re.sub(r"[^a-z0-9_]+", "_", text)
            if name in {"code", "type", "error_code", "param", "reason"}:
                if any(
                    marker in compact
                    for marker in (
                        "context_length_exceeded",
                        "context_window_exceeded",
                        "maximum_context_length_exceeded",
                        "prompt_is_too_long",
                        "input_too_long",
                    )
                ):
                    return True
            if name in {"message", "detail", "error", "type", "code"}:
                if re.search(
                    r"(?:context|prompt|input).{0,32}(?:length|window|token|limit).{0,32}"
                    r"(?:exceed|too\s+long|maximum|max\b)",
                    text,
                ):
                    return True
                if re.search(
                    r"(?:maximum|max\b|limit).{0,32}(?:context|prompt|input).{0,32}"
                    r"(?:length|window|token)",
                    text,
                ):
                    return True
            if _context_evidence(item):
                return True
    elif isinstance(value, list):
        return any(_context_evidence(item) for item in value)
    return False


def safe_context_status(
    provider: Mapping[str, Any], model: Mapping[str, Any], protocol: str
) -> Dict[str, Any]:
    identity = context_identity(provider, model, protocol)
    output_reserve = _output_reserve({}, model)
    reserves = (
        output_reserve + SAFETY_RESERVE_TOKENS
        if output_reserve is not None
        else None
    )
    context_limit, safe_input_limit, source, confidence, calibration = _safe_limit(
        provider, model, protocol, reserves
    )
    return {
        **identity.to_dict(),
        "context_limit": context_limit,
        "safe_input_limit": safe_input_limit,
        "output_reserve": output_reserve,
        "safety_reserve": SAFETY_RESERVE_TOKENS,
        "reserves": reserves,
        "confidence": confidence,
        "source": source,
        "largest_success_estimate": (
            calibration.get("largest_success_estimate") if calibration else None
        ),
        "smallest_failure_estimate": (
            calibration.get("smallest_failure_estimate") if calibration else None
        ),
    }


class ContextGuardBlocked(Exception):
    def __init__(self, assessment: ContextAssessment):
        self.assessment = assessment
        super().__init__(format_context_error(assessment.to_safe_dict()))


class ContextGuard:
    """Small state-free facade used by the request lifecycle.

    Calibration is stored on the normalized model configuration by the caller;
    this object deliberately owns no conversation or request history.
    """

    def assess(
        self,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        protocol: str,
        payload: Mapping[str, Any],
        completeness: str = "high",
    ) -> ContextAssessment:
        return assess_context(provider, model, protocol, payload, completeness)

    def status(
        self, provider: Mapping[str, Any], model: Mapping[str, Any], protocol: str
    ) -> Dict[str, Any]:
        return safe_context_status(provider, model, protocol)

    def update(
        self,
        model: MutableMapping[str, Any],
        observation: Mapping[str, Any],
        outcome: str,
        estimate: Any,
        observed_at: Optional[str] = None,
    ) -> bool:
        return update_calibration(model, observation, outcome, estimate, observed_at)


__all__ = [
    "CALIBRATION_CAPACITY",
    "CONCRETE_PROTOCOLS",
    "ContextAssessment",
    "ContextGuard",
    "ContextGuardBlocked",
    "ContextIdentity",
    "SAFETY_RESERVE_TOKENS",
    "assess_context",
    "calibration_for",
    "context_identity",
    "estimate_input_tokens",
    "estimate_method",
    "format_context_error",
    "is_explicit_context_error",
    "mark_explicit_failure",
    "safe_context_status",
    "update_calibration",
]
