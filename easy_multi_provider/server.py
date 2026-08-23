"""Small dependency-free Web UI and HTTP router server."""

from __future__ import annotations

import ast
import base64
from collections import deque
import json
import hmac
import os
import platform
import re
import secrets
import signal
import subprocess
import sys
import threading
import time
import uuid
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, Mapping, Optional, Tuple
from urllib.parse import parse_qs, unquote, urlparse
from urllib.request import getproxies

from . import __version__
from .accounts import (
    account_root,
    duplicate_account_status,
    import_account,
    public_accounts,
    valid_caller_authorization,
)
from .catalog import (
    build_catalog,
    generated_catalog_path,
    subscription_model_options,
    write_catalog,
)
from .capabilities import (
    deployment_identity,
    endpoint_fingerprint,
    input_modalities_metadata_source,
    make_provenance,
    normalize_input_modalities,
    normalize_output_modalities,
    normalize_supported_protocols,
    observed_at_now,
    output_modalities_metadata_source,
    safe_capability_list,
)
from .config import (
    MAX_CONTEXT_WINDOW,
    ConfigError,
    config_path,
    load,
    merge_web_update,
    public_config,
    save,
)
from .codex_runtime import (
    EMP_LOADED,
    NATIVE_LOADED,
    NOT_CHECKED,
    RELOAD_REQUIRED,
    STOPPED_WAITING_FOR_START,
    STOP_FAILED,
    UNSUPPORTED,
    VERIFICATION_FAILED,
    CodexRuntimeController,
    RuntimeRecoveryStore,
    RuntimeSyncError,
    RuntimeSyncResult,
    offline_runtime_snapshot,
)
from .context_guard import ContextGuard, ContextGuardBlocked
from .diagnostic_journal import NullJournal, create_journal
from .integration import IntegrationError, IntegrationManager, IntegrationResult, IntegrationStatus, ServiceNotReady
from .main import resolve_integration_paths
from .migration import export_bundle, import_bundle
from .quota import QuotaError, account_refresh_lock, read_native_login_quota, refresh_account_quota
from .provider_replay import ProviderReplayCache
from .continuity import (
    CompactionBindingStore,
    NativeCompactionObserver,
    ContinuityError,
    derive_native_route_identity,
    register_native_json,
)
from .router import (
    ContextHandoffRequiredError,
    ContextLengthError,
    RouterError,
    discover_models,
    forward_native_search,
    model_metadata,
    proxy,
    proxy_compact,
)
from .transport import (
    TransportError,
    WebSocketConnection,
    WebSocketProtocolError,
    decode_content,
    sse_json_events,
    websocket_accept,
)
from .vault import ensure_master_key


WEB_FILE = Path(__file__).with_name("web").joinpath("index.html")
PROXY_ENV_KEYS = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
)
_DIAGNOSTIC_CAPACITY = 64
_DIAGNOSTIC_ID = re.compile(r"^[A-Za-z0-9._/-]{1,256}$")
_DIAGNOSTIC_FINGERPRINT = re.compile(r"^sha256:[0-9a-f]{64}$")
_DIAGNOSTIC_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages", "unknown"}
)
_DIAGNOSTIC_DIALECTS = frozenset(
    {
        "codex_native",
        "portable_responses",
        "chat_completions",
        "anthropic_messages",
        "unknown",
    }
)
_DIAGNOSTIC_TRANSPORTS = frozenset({"http", "sse", "websocket", "unknown"})
_DISCOVERY_CAPABILITY_SOURCES = frozenset(
    {"official", "advertised", "inferred", "unknown"}
)
_DIAGNOSTIC_ERRORS = frozenset(
    {
        "none",
        "auth",
        "rate_limit",
        "protocol_rejection",
        "upstream_5xx",
        "timeout",
        "network",
        "router_error",
        "stream_error",
        "stream_incomplete",
        "client_disconnect",
        "client_cancelled",
        "client_websocket_close",
        "upstream_close_pre_output",
        "upstream_close_after_output",
        "upstream_close_after_tool",
        "malformed_terminal",
        "proxy_reset",
        "output_limit",
        "content_filter",
        "context_length_exceeded",
        "context_handoff_required",
        "unknown",
    }
)
_DIAGNOSTIC_DECISIONS = frozenset(
    {"explicit", "normal_order", "observed_priority", "fallback_rejection", "unknown"}
)
_DIAGNOSTIC_CONTEXT_DECISIONS = frozenset({"allowed", "warned", "blocked", "unknown"})
_DIAGNOSTIC_CONTEXT_SOURCES = frozenset(
    {
        "official",
        "advertised",
        "observed",
        "manual",
        "inherited",
        "inferred",
        "unknown",
    }
)
_DIAGNOSTIC_COMPLETENESS = frozenset({"high", "lost", "unknown"})
_DIAGNOSTIC_TOOL_PAIRING = frozenset({"none", "paired", "incomplete", "invalid"})
_DIAGNOSTIC_RECOVERY_MODES = frozenset(
    {"none", "pre_output_retry", "reattached", "native_http_fallback"}
)
_DIAGNOSTIC_BINDING_RESULTS = frozenset({
    "registered",
    "registration_failed",
    "hit",
    "missing",
    "stale",
    "unknown",
})
_DIAGNOSTIC_CONTINUITY_REASONS = frozenset({
    "none",
    "same_domain",
    "binding_missing",
    "binding_stale",
    "source_unavailable",
    "summary_invalid",
    "handoff_succeeded",
    "handoff_failed",
    "registration_failed",
})
_DIAGNOSTIC_CONTINUITY_ITEM_TYPES = frozenset({
    "compaction",
    "unknown",
})
_WS_REPLAY_MAX_ITEMS = 256
_WS_REPLAY_MAX_BYTES = 4 * 1024 * 1024
_HTTP_REQUEST_BYTES_MAX = 64 * 1024 * 1024
_HTTP_HANDLER_STAGE = "http_handler"
_MANAGEMENT_COUNT_MAX = 1_000_000
_MANAGEMENT_SCALAR_KEY = re.compile(r"^[A-Za-z][A-Za-z0-9_]{0,63}$")


class EmptyEmpCatalog(IntegrationError):
    """Raised before mutation when no visible EMP model can be verified."""


def _discovery_capability_source(item: Mapping[str, Any], field: str) -> str:
    sources = item.get("capability_sources")
    entry = sources.get(field) if isinstance(sources, Mapping) else None
    source = entry.get("source") if isinstance(entry, Mapping) else None
    if source in _DISCOVERY_CAPABILITY_SOURCES:
        return source
    if field == "input_modalities" and field in item:
        return input_modalities_metadata_source(item.get(field))
    return "advertised" if field in item else "unknown"


def _model_capability_source(model: Mapping[str, Any], field: str) -> str:
    sources = model.get("capability_sources")
    entry = sources.get(field) if isinstance(sources, Mapping) else None
    source = entry.get("source") if isinstance(entry, Mapping) else None
    return source if source in _DIAGNOSTIC_CONTEXT_SOURCES else "unknown"


_SOURCE_ORDER = (
    "manual",
    "observed",
    "advertised",
    "official",
    "inferred",
    "inherited",
    "unknown",
)
_SOURCE_RANK = {
    source: len(_SOURCE_ORDER) - index - 1
    for index, source in enumerate(_SOURCE_ORDER)
}

_MERGE_SCALAR_FIELDS = (
    "supports_reasoning",
    "reasoning_control",
    "context_window",
    "max_input_tokens",
    "output_limit",
    "supports_image_detail_original",
)

_MERGE_LIST_FIELDS = (
    "reasoning_levels",
    "input_modalities",
    "output_modalities",
    "supported_protocols",
)

_MERGE_NESTED_BOOL_FIELDS = (
    "streaming",
    "structured_tools",
    "parallel_tools",
    "structured_output",
    "web_search",
    "websocket",
)


def _field_source(model: Mapping[str, Any], field: str) -> str:
    sources = model.get("capability_sources")
    if not isinstance(sources, Mapping):
        return "unknown"
    entry = sources.get(field)
    if isinstance(entry, Mapping):
        source = entry.get("source")
        return source if source in _SOURCE_RANK else "unknown"
    if isinstance(entry, str) and entry in _SOURCE_RANK:
        return entry
    return "unknown"


def _has_provenance_entry(model: Mapping[str, Any], field: str) -> bool:
    """Return True when an explicit provenance entry exists for field.

    Distinguishes 'no entry at all' from an explicit {source: 'unknown'} entry.
    When an entry is absent, discovery may derive advertised provenance for
    genuinely live metadata.  When an explicit unknown entry exists, it must
    be preserved rather than reclassified as advertised.
    """
    sources = model.get("capability_sources")
    if not isinstance(sources, Mapping):
        return False
    return field in sources


def _incoming_can_refresh(incoming_source: str, existing_source: str) -> bool:
    """Return True when incoming_source may replace existing_source."""
    return _SOURCE_RANK.get(incoming_source, 0) >= _SOURCE_RANK.get(existing_source, 0)


def _merge_discovered_field(
    existing: Dict[str, Any],
    item: Mapping[str, Any],
    field: str,
    incoming_source: str,
    observed_at: str,
) -> None:
    """Merge one scalar or list capability field by provenance priority."""
    if field not in item:
        return
    incoming_value = item.get(field)
    if incoming_value is None:
        return
    has_entry = _has_provenance_entry(item, field)
    if isinstance(incoming_value, (list, str)) and not incoming_value:
        # An explicitly-provenanced empty list is meaningful and must be
        # persisted; without an explicit entry it must not erase an
        # existing useful value.
        if not (isinstance(incoming_value, list) and has_entry):
            return
    existing_source = _field_source(existing, field)
    if not _incoming_can_refresh(incoming_source, existing_source):
        return
    if field in _MERGE_LIST_FIELDS:
        if field == "input_modalities":
            existing[field] = normalize_input_modalities(incoming_value)
        elif field == "output_modalities":
            existing[field] = normalize_output_modalities(incoming_value)
        elif field == "supported_protocols":
            existing[field] = normalize_supported_protocols(incoming_value)
        else:
            existing[field] = list(incoming_value)
    else:
        existing[field] = incoming_value
    existing.setdefault("capability_sources", {})[field] = make_provenance(
        incoming_source, observed_at=observed_at
    )


def _merge_discovered_nested_bool(
    existing: Dict[str, Any],
    item: Mapping[str, Any],
    field: str,
    incoming_source: str,
    observed_at: str,
) -> None:
    """Merge one nested capability boolean by provenance priority."""
    incoming_caps = item.get("capabilities")
    if not isinstance(incoming_caps, Mapping) or field not in incoming_caps:
        return
    incoming_value = incoming_caps.get(field)
    if not isinstance(incoming_value, bool):
        return
    existing_source = _field_source(existing, field)
    if not _incoming_can_refresh(incoming_source, existing_source):
        return
    caps = existing.get("capabilities")
    if not isinstance(caps, dict):
        caps = {}
    caps[field] = incoming_value
    existing["capabilities"] = caps
    existing.setdefault("capability_sources", {})[field] = make_provenance(
        incoming_source, observed_at=observed_at
    )


def _build_new_model_from_discovery(
    item: Mapping[str, Any], provider_id: str, observed_at: str
) -> Dict[str, Any]:
    """Build a new model dict from discovery data with actual incoming sources."""
    upstream_id = item.get("upstream_id", "")
    model_id = provider_id + "/" + upstream_id
    capabilities = {}
    capability_sources = {}
    incoming_caps = item.get("capabilities")
    if isinstance(incoming_caps, Mapping):
        for field in _MERGE_NESTED_BOOL_FIELDS:
            if field in incoming_caps and isinstance(incoming_caps[field], bool):
                capabilities[field] = incoming_caps[field]
                has_entry = _has_provenance_entry(item, field)
                source = _field_source(item, field)
                if not has_entry:
                    source = "advertised"
                capability_sources[field] = make_provenance(
                    source,
                    observed_at=observed_at,
                )
    for field in _MERGE_SCALAR_FIELDS + _MERGE_LIST_FIELDS:
        if field not in item:
            continue
        value = item.get(field)
        if value is None:
            continue
        has_entry = _has_provenance_entry(item, field)
        if isinstance(value, (list, str)) and not value:
            # An explicitly-provenanced empty list is meaningful for a new
            # model; without an explicit entry it must not erase an
            # existing useful value.
            if not (isinstance(value, list) and has_entry):
                continue
        source = _field_source(item, field)
        if not has_entry:
            if field in ("context_window", "output_limit", "max_input_tokens"):
                source = "advertised" if value else "unknown"
            elif field == "input_modalities":
                source = input_modalities_metadata_source(value)
            elif field == "output_modalities":
                source = output_modalities_metadata_source(value)
            elif field in ("reasoning_levels", "supported_protocols"):
                source = "advertised" if value else "unknown"
        capability_sources[field] = make_provenance(source, observed_at=observed_at)
    model = {
        "id": model_id,
        "provider": provider_id,
        "upstream_id": upstream_id,
        "display_name": item.get("display_name") or upstream_id,
        "description": item.get("description", ""),
        "supports_reasoning": item.get("supports_reasoning")
        if isinstance(item.get("supports_reasoning"), bool)
        else None,
        "reasoning_levels": list(item.get("reasoning_levels") or []),
        "reasoning_control": item.get("reasoning_control", ""),
        "context_window": int(item.get("context_window", 0) or 0),
        "max_input_tokens": int(item.get("max_input_tokens", 0) or 0),
        "output_limit": int(
            item.get("output_limit", item.get("output_token_limit", 0)) or 0
        ),
        "created_at": int(item.get("created_at", 0) or 0),
        "enabled": True,
        "visibility": "list",
        "input_modalities": normalize_input_modalities(item.get("input_modalities")),
        "output_modalities": normalize_output_modalities(item.get("output_modalities")),
        "supported_protocols": normalize_supported_protocols(item.get("supported_protocols")),
        "supports_image_detail_original": item.get("supports_image_detail_original", False)
        if isinstance(item.get("supports_image_detail_original"), bool)
        else False,
        "capability_sources": capability_sources,
    }
    if capabilities:
        model["capabilities"] = capabilities
    return model


def _safe_diagnostic_text(value: Any, pattern: re.Pattern[str]) -> str:
    value = value.strip() if isinstance(value, str) else ""
    return value if pattern.fullmatch(value) else ""


def _safe_diagnostic_int(value: Any, maximum: int) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        value = int(value)
    except (TypeError, ValueError):
        return None
    return max(0, min(maximum, value))


def _safe_diagnostic_float(value: Any) -> float:
    try:
        value = float(value)
    except (TypeError, ValueError):
        return 0.0
    if value != value or value in (float("inf"), float("-inf")):
        return 0.0
    return max(0.0, min(1.0, value))


def _safe_transport_ratio(value: Any, decoded_bytes: Optional[int]) -> Optional[float]:
    if decoded_bytes == 0 or isinstance(value, bool):
        return None
    try:
        value = float(value)
    except (TypeError, ValueError):
        return None
    if value != value or value in (float("inf"), float("-inf")):
        return None
    return value if 0.0 <= value <= 64.0 else None


def _safe_diagnostic_sequence(value: Any) -> list:
    if not isinstance(value, list):
        return []
    return [
        item
        for item in (
            _safe_diagnostic_text(entry, _DIAGNOSTIC_ID) for entry in value[:256]
        )
        if item
    ]


def _diagnostic_http_path(raw_target: Any) -> str:
    """Return a query-free path with account route parameters removed."""

    target = raw_target if isinstance(raw_target, str) else ""
    path = urlparse(target).path
    prefix = "/api/accounts/"
    if not path.startswith(prefix):
        return path
    suffix = path[len(prefix) :]
    if suffix == "import":
        return path
    if suffix and "/" not in suffix:
        return prefix + "{account}"
    account, separator, operation = suffix.partition("/")
    if account and separator and operation == "quota":
        return prefix + "{account}/quota"
    return path


def _ws_replay_size(*parts: Any) -> Optional[Tuple[int, int]]:
    """Bound connection-local replay state without retaining another payload copy."""

    item_count = 0
    byte_count = 0
    for part in parts:
        if not isinstance(part, list):
            return None
        item_count += len(part)
        if item_count > _WS_REPLAY_MAX_ITEMS:
            return None
        for item in part:
            try:
                byte_count += len(
                    json.dumps(item, ensure_ascii=False, separators=(",", ":"))
                    .encode("utf-8")
                )
            except (TypeError, ValueError, OverflowError):
                return None
            if byte_count > _WS_REPLAY_MAX_BYTES:
                return None
    return item_count, byte_count


class ObservationRing:
    """Small process-local diagnostics ring containing only safe route facts."""

    def __init__(self, capacity: int = _DIAGNOSTIC_CAPACITY, sink=None):
        self.capacity = max(1, min(_DIAGNOSTIC_CAPACITY, int(capacity)))
        self._records = deque(maxlen=self.capacity)
        self._lock = threading.RLock()
        self._sink = sink

    def record(self, event: Mapping[str, Any]) -> None:
        if not isinstance(event, Mapping):
            return
        protocol = event.get("resolved_protocol", event.get("protocol"))
        protocol = protocol if protocol in _DIAGNOSTIC_PROTOCOLS else "unknown"
        dialect = event.get("dialect")
        dialect = dialect if dialect in _DIAGNOSTIC_DIALECTS else "unknown"
        transport = event.get("transport", "unknown")
        transport = transport if transport in _DIAGNOSTIC_TRANSPORTS else "unknown"
        error_class = event.get("error_class", "unknown")
        error_class = error_class if error_class in _DIAGNOSTIC_ERRORS else "unknown"
        decision = event.get("protocol_decision", event.get("decision", "unknown"))
        decision = decision if decision in _DIAGNOSTIC_DECISIONS else "unknown"
        context = event.get("context_observation", {})
        if not isinstance(context, Mapping):
            context = {}
        context_decision = context.get("context_decision", context.get("decision", "unknown"))
        if context_decision in ("allow", "allowed"):
            context_decision = "allowed"
        elif context_decision in ("warn", "warned"):
            context_decision = "warned"
        elif context_decision in ("block", "blocked"):
            context_decision = "blocked"
        else:
            context_decision = "unknown"
        context_source = context.get("source", "unknown")
        if context_source not in _DIAGNOSTIC_CONTEXT_SOURCES:
            context_source = "unknown"
        completeness = context.get("completeness", "unknown")
        if completeness not in _DIAGNOSTIC_COMPLETENESS:
            completeness = "unknown"
        status = _safe_diagnostic_int(event.get("status"), 599)
        if status is not None and status < 100:
            status = None
        duration = event.get("duration_ms")
        try:
            duration = int(round(float(duration)))
        except (TypeError, ValueError):
            duration = 0
        duration = max(0, min(3_600_000, duration))
        tool_pairing = event.get("tool_pairing_status", "none")
        if tool_pairing not in _DIAGNOSTIC_TOOL_PAIRING:
            tool_pairing = "invalid"
        close_code = _safe_diagnostic_int(event.get("close_code"), 4999)
        if close_code is not None and not 1000 <= close_code <= 4999:
            close_code = None
        recovery_mode = event.get("recovery_mode", "none")
        if recovery_mode not in _DIAGNOSTIC_RECOVERY_MODES:
            recovery_mode = "none"
        decoded_request_bytes = _safe_diagnostic_int(
            event.get("decoded_request_bytes"), 64 * 1024 * 1024
        )
        upstream_request_bytes = _safe_diagnostic_int(
            event.get("upstream_request_bytes"), 64 * 1024 * 1024
        )
        upstream_content_encoding = event.get("upstream_content_encoding")
        if upstream_content_encoding not in ("zstd", "identity"):
            upstream_content_encoding = "unknown"
        compression_ratio = _safe_transport_ratio(
            event.get("compression_ratio"), decoded_request_bytes
        )
        record = {
            "observed_at": observed_at_now(),
            "route": _safe_diagnostic_text(event.get("route", ""), _DIAGNOSTIC_ID)
            or "unknown",
            "provider_id": _safe_diagnostic_text(event.get("provider_id"), _DIAGNOSTIC_ID),
            "model_id": _safe_diagnostic_text(event.get("model_id"), _DIAGNOSTIC_ID),
            "endpoint_fingerprint": _safe_diagnostic_text(
                event.get("endpoint_fingerprint"), _DIAGNOSTIC_FINGERPRINT
            ),
            "deployment_identity": _safe_diagnostic_text(
                event.get("deployment_identity"), _DIAGNOSTIC_ID
            )
            or "default",
            "protocol": protocol,
            "dialect": dialect,
            "transport": transport,
            "request_item_count": _safe_diagnostic_int(
                event.get("request_item_count"), 256
            ),
            "request_item_types": _safe_diagnostic_sequence(
                event.get("request_item_types")
            ),
            "content_part_types": _safe_diagnostic_sequence(
                event.get("content_part_types")
            ),
            "tool_pairing_status": tool_pairing,
            "close_code": close_code,
            "output_emitted": bool(event.get("output_emitted", False)),
            "tool_activity": bool(event.get("tool_activity", False)),
            "terminal_event_observed": bool(
                event.get("terminal_event_observed", False)
            ),
            "recovery_succeeded": bool(event.get("recovery_succeeded", False)),
            "recovery_mode": recovery_mode,
            "request_bytes": _safe_diagnostic_int(event.get("request_bytes"), 64 * 1024 * 1024),
            "decoded_request_bytes": decoded_request_bytes,
            "upstream_request_bytes": upstream_request_bytes,
            "upstream_content_encoding": upstream_content_encoding,
            "response_bytes": _safe_diagnostic_int(event.get("response_bytes"), 64 * 1024 * 1024),
            "duration_ms": duration,
            "status": status,
            "error_class": error_class,
            "fallback": bool(event.get("protocol_fallback", event.get("fallback", False))),
            "decision": decision,
            "context_decision": context_decision,
            "estimated_tokens": _safe_diagnostic_int(
                context.get("estimated_tokens", context.get("input_estimate")),
                MAX_CONTEXT_WINDOW,
            ),
            "context_limit": _safe_diagnostic_int(
                context.get("context_limit"), MAX_CONTEXT_WINDOW
            ),
            "safe_input_limit": _safe_diagnostic_int(
                context.get("safe_input_limit"), MAX_CONTEXT_WINDOW
            ),
            "context_confidence": _safe_diagnostic_float(context.get("confidence")),
            "context_source": context_source,
            "context_estimate_method": _safe_diagnostic_text(
                context.get("estimate_method"), _DIAGNOSTIC_ID
            ),
            "context_reserves": _safe_diagnostic_int(
                context.get("reserves"), MAX_CONTEXT_WINDOW
            ),
            "context_completeness": completeness,
            "source_binding_result": (
                event.get("source_binding_result")
                if event.get("source_binding_result") in _DIAGNOSTIC_BINDING_RESULTS
                else "unknown"
            ),
            "same_domain": bool(event.get("same_domain", False)),
            "handoff_attempted": bool(event.get("handoff_attempted", False)),
            "handoff_succeeded": bool(event.get("handoff_succeeded", False)),
            "item_index": _safe_diagnostic_int(event.get("item_index"), 9999),
            "item_type": (
                event.get("item_type")
                if event.get("item_type") in _DIAGNOSTIC_CONTINUITY_ITEM_TYPES
                else "unknown"
            ),
            "continuity_reason": (
                event.get("reason")
                if event.get("reason") in _DIAGNOSTIC_CONTINUITY_REASONS
                else "none"
            ),
        }
        if compression_ratio is not None:
            record["compression_ratio"] = compression_ratio
        with self._lock:
            self._records.append(record)
        if self._sink is not None:
            try:
                self._sink(dict(record))
            except Exception:
                pass

    def snapshot(self) -> Dict[str, Any]:
        with self._lock:
            return {
                "capacity": self.capacity,
                "records": [dict(record) for record in self._records],
            }


def _gsettings_value(schema: str, key: str) -> Any:
    try:
        result = subprocess.run(
            ["gsettings", "get", schema, key],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode:
        return None
    value = result.stdout.strip()
    if value == "true":
        return True
    if value == "false":
        return False
    try:
        return ast.literal_eval(value)
    except (SyntaxError, ValueError):
        return None


def _proxy_url(host: Any, port: Any, scheme: str) -> str:
    if not isinstance(host, str) or not host.strip():
        return ""
    host = host.strip()
    if any(character.isspace() or character in "/@?#" for character in host):
        return ""
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        return ""
    if ":" in host and not host.startswith("["):
        host = "[" + host + "]"
    return "%s://%s:%d" % (scheme, host, port)


def _gnome_proxy_settings() -> Dict[str, Any]:
    if not sys.platform.startswith("linux"):
        return {}
    root = "org.gnome.system.proxy"
    if _gsettings_value(root, "mode") != "manual":
        return {}
    http = _proxy_url(
        _gsettings_value(root + ".http", "host"),
        _gsettings_value(root + ".http", "port"),
        "http",
    )
    https = _proxy_url(
        _gsettings_value(root + ".https", "host"),
        _gsettings_value(root + ".https", "port"),
        "http",
    )
    if not https and _gsettings_value(root, "use-same-proxy") is True:
        https = http
    socks = _proxy_url(
        _gsettings_value(root + ".socks", "host"),
        _gsettings_value(root + ".socks", "port"),
        "socks5",
    )
    ignored = _gsettings_value(root, "ignore-hosts")
    return {
        "http": http,
        "https": https,
        "all": socks,
        "no": ",".join(item for item in ignored if isinstance(item, str))
        if isinstance(ignored, list)
        else "",
    }


def _apply_proxy_settings(settings: Dict[str, Any]) -> bool:
    applied = False
    for scheme in ("http", "https", "all"):
        value = settings.get(scheme)
        if not isinstance(value, str) or not value:
            continue
        parsed = urlparse(value)
        try:
            valid = parsed.scheme in ("http", "https", "socks5", "socks5h") and bool(
                parsed.hostname and parsed.port
            )
        except ValueError:
            valid = False
        if not valid:
            continue
        os.environ.setdefault(scheme + "_proxy", value)
        os.environ.setdefault(scheme.upper() + "_PROXY", value)
        applied = True
    ignored = settings.get("no")
    if applied and isinstance(ignored, str) and ignored:
        os.environ.setdefault("no_proxy", ignored)
        os.environ.setdefault("NO_PROXY", ignored)
    return applied


def configure_proxy_environment() -> str:
    """Prefer explicit environment proxies, then safe operating-system settings."""
    if any(os.environ.get(key) for key in PROXY_ENV_KEYS):
        return "environment"
    if _apply_proxy_settings(getproxies()):
        return "system"
    if _apply_proxy_settings(_gnome_proxy_settings()):
        return "system"
    return "direct"

def _handoff_error_body(exc) -> Dict[str, Any]:
    """Build a structured 409 body for ContextHandoffRequiredError.

    Carries only error_class, reason, item_index, item_type in addition
    to the safe public recovery message. No opaque/checkpoint/content.
    """
    return {
        "error": {
            "code": "context_handoff_required",
            "message": str(exc),
            "error_class": getattr(exc, "error_class", "context_handoff_required"),
            "reason": getattr(exc, "reason", "binding_missing"),
            "item_index": getattr(exc, "item_index", 0),
            "item_type": getattr(exc, "item_type", "compaction"),
        }
    }


def _handoff_sse_frame(exc) -> bytes:
    """Build exactly one response.failed SSE event for a handoff failure."""
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "failed",
        "error": {
            "code": "context_handoff_required",
            "message": str(exc),
            "error_class": getattr(exc, "error_class", "context_handoff_required"),
            "reason": getattr(exc, "reason", "binding_missing"),
            "item_index": getattr(exc, "item_index", 0),
            "item_type": getattr(exc, "item_type", "compaction"),
        },
    }
    payload = {"type": "response.failed", "response": response}
    data = json.dumps(payload, ensure_ascii=False)
    frame = "event: response.failed" + chr(10) + "data: " + data + chr(10) + chr(10)
    return frame.encode("utf-8")


def _handoff_ws_event(exc) -> Dict[str, Any]:
    """Build one request-scoped response.failed event for WS handoff failure."""
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "failed",
        "error": {
            "code": "context_handoff_required",
            "message": str(exc),
            "error_class": getattr(exc, "error_class", "context_handoff_required"),
            "reason": getattr(exc, "reason", "binding_missing"),
            "item_index": getattr(exc, "item_index", 0),
            "item_type": getattr(exc, "item_type", "compaction"),
        },
    }
    return {"type": "response.failed", "response": response}


class AppState:
    def __init__(
        self,
        path: Optional[Path] = None,
        integration_manager: Optional[IntegrationManager] = None,
        catalog_path: Optional[Path] = None,
        diagnostics: Optional[ObservationRing] = None,
        runtime_controller: Optional[CodexRuntimeController] = None,
        journal=None,
    ):
        self.path = Path(path or config_path())
        self.lock = threading.RLock()
        self.bootstrap_token = secrets.token_urlsafe(32)
        self.bootstrap_used = False
        self.session_token = secrets.token_urlsafe(32)
        self.journal = journal if journal is not None else NullJournal()
        self.diagnostics = (
            diagnostics
            if diagnostics is not None
            else ObservationRing(sink=self._persist_route_observation)
        )
        self.context_guard = ContextGuard()
        self.provider_replay = ProviderReplayCache()
        self.integration_manager = integration_manager
        self.runtime_controller = runtime_controller or CodexRuntimeController(
            target_codex_home=(
                integration_manager.config_path.parent
                if integration_manager is not None
                else None
            )
        )
        if integration_manager is not None and isinstance(
            self.runtime_controller, CodexRuntimeController
        ):
            self.runtime_controller.set_target_codex_home(
                integration_manager.config_path.parent
            )
        self._runtime_sync = {
            "state": NOT_CHECKED,
            "target": "native",
            "verified": False,
            "confidence": "not_checked",
            "detail": "Codex runtime has not been checked",
            "last_known": None,
        }
        self._runtime_expected_models: Tuple[str, ...] = ()
        self._runtime_should_be_present = True
        self.runtime_recovery_store = (
            RuntimeRecoveryStore(integration_manager.lease_path.with_name("runtime.json"))
            if integration_manager is not None
            else None
        )
        if catalog_path is not None:
            self.integration_catalog_path = Path(catalog_path)
        elif integration_manager is not None:
            self.integration_catalog_path = generated_catalog_path(
                integration_manager.config_path.parent
            )
        else:
            self.integration_catalog_path = generated_catalog_path()
        self._service_ready = False
        self._integration_owned = False
        self._startup_conflicts: Tuple[str, ...] = ()
        # Discovery is low-frequency and upstream-bound; one fixed lock avoids
        # retaining attacker-controlled provider IDs in process state.
        self.discovery_lock = threading.Lock()
        self.config = load(self.path)
        state_dir = self.path.parent / "state"
        self._compaction_bindings_path = state_dir / "continuity-bindings.enc"
        try:
            self.compaction_bindings = CompactionBindingStore(
                self._compaction_bindings_path
            )
        except Exception:
            # A corrupt or unusable vault fails closed at lookup time; the
            # server itself must still start for management endpoints.
            self.compaction_bindings = None
        self._load_runtime_recovery()
        if any(
            provider.get("api_key") for provider in self.config.get("providers", [])
        ):
            save(self.config, self.path)
            self.config = load(self.path)

    def _persist_route_observation(self, record: Mapping[str, Any]) -> None:
        self.journal.event("info", "route_observation", **record)

    def snapshot(self) -> Dict[str, Any]:
        with self.lock:
            return json.loads(json.dumps(self.config))

    def ensure_integration_manager(self) -> IntegrationManager:
        with self.lock:
            if self.integration_manager is None:
                paths = resolve_integration_paths()
                self.integration_manager = IntegrationManager(
                    paths.config_path,
                    paths.lease_path,
                    lock_path=paths.lock_path,
                )
                if isinstance(self.runtime_controller, CodexRuntimeController):
                    self.runtime_controller.set_target_codex_home(
                        self.integration_manager.config_path.parent
                    )
                self.integration_catalog_path = generated_catalog_path(paths.codex_home)
                self.runtime_recovery_store = RuntimeRecoveryStore(
                    self.integration_manager.lease_path.with_name("runtime.json")
                )
                self._load_runtime_recovery()
            return self.integration_manager

    def _load_runtime_recovery(self) -> None:
        store = self.runtime_recovery_store
        if store is None:
            return
        try:
            record = store.load()
        except RuntimeSyncError as error:
            with self.lock:
                self._runtime_sync = {
                    "state": UNSUPPORTED,
                    "target": "native",
                    "verified": False,
                    "confidence": "stale",
                    "detail": str(error),
                    "last_known": None,
                }
            return
        if record is None:
            return
        with self.lock:
            self._runtime_expected_models = record.expected_models
            self._runtime_should_be_present = record.target == "emp"
            self._runtime_sync = offline_runtime_snapshot(record, confidence="stale")

    def _persist_runtime_recovery(self) -> None:
        store = self.runtime_recovery_store
        if store is None:
            return
        with self.lock:
            snapshot = dict(self._runtime_sync)
            expected_models = self._runtime_expected_models
            manager = self.integration_manager
        try:
            relation = manager.status().relation if manager is not None else "unleased"
        except (IntegrationError, OSError):
            relation = "other"
        store.save(
            snapshot.get("state", NOT_CHECKED),
            snapshot.get("target", "native"),
            relation,
            expected_models,
            bool(snapshot.get("verified", False)),
            snapshot.get("detail", ""),
        )

    def mark_service_ready(self) -> None:
        with self.lock:
            self._service_ready = True

    def service_ready(self) -> bool:
        with self.lock:
            return self._service_ready

    def set_startup_conflicts(self, conflicts: Tuple[str, ...]) -> None:
        with self.lock:
            self._startup_conflicts = tuple(dict.fromkeys(conflicts))

    def startup_conflicts(self) -> Tuple[str, ...]:
        with self.lock:
            return self._startup_conflicts

    def integration_status(self) -> IntegrationStatus:
        return self.ensure_integration_manager().status()

    def _runtime_model_ids(self) -> Tuple[str, ...]:
        catalog = build_catalog(self.snapshot())
        return tuple(
            item["slug"]
            for item in catalog.get("models", [])
            if isinstance(item, Mapping)
            and isinstance(item.get("slug"), str)
            and "/" in item["slug"]
            and item.get("visibility", "list") == "list"
        )

    def _mark_runtime_pending(self, intent: str, detail: str) -> None:
        """Record that a catalog mutation needs one confirmed graceful stop."""

        expected_models = self._runtime_model_ids()
        with self.lock:
            self._runtime_expected_models = expected_models
            self._runtime_should_be_present = intent == "emp"
            self._runtime_sync = {
                "state": RELOAD_REQUIRED,
                "target": intent,
                "verified": False,
                "confidence": "pending",
                "detail": detail,
                "last_known": None,
            }
        self._persist_runtime_recovery()

    def runtime_sync_snapshot(self) -> Dict[str, Any]:
        with self.lock:
            return dict(self._runtime_sync)

    def sync_integration_runtime(self, confirm_reload: bool) -> RuntimeSyncResult:
        """Perform one confirmed graceful stop and bounded model/list observation."""

        manager = self.ensure_integration_manager()
        with manager.operation_lock():
            return self._sync_integration_runtime_unlocked(confirm_reload)

    def _sync_integration_runtime_unlocked(
        self, confirm_reload: bool
    ) -> RuntimeSyncResult:
        with self.lock:
            expected_models = self._runtime_expected_models
            target = "emp" if self._runtime_should_be_present else "native"
            if confirm_reload:
                self._runtime_sync = {
                    "state": "stopping",
                    "target": target,
                    "verified": False,
                    "confidence": "pending",
                    "detail": "Codex graceful stop is in progress",
                    "last_known": None,
                }
        if confirm_reload:
            self._persist_runtime_recovery()
        result = self.runtime_controller.reload(
            expected_models,
            target,
            confirm_reload=confirm_reload,
        )
        with self.lock:
            self._runtime_sync = {
                "state": result.state,
                "target": result.target,
                "verified": result.verified,
                "confidence": "live",
                "detail": result.detail,
                "last_known": None,
            }
        self._persist_runtime_recovery()
        return result

    def enable_integration(
        self, base_url: str, *, confirm_reload: bool
    ) -> IntegrationResult:
        manager = self.ensure_integration_manager()
        config = self.snapshot()
        if not self._runtime_model_ids():
            raise EmptyEmpCatalog("EMP has no visible models to expose to Codex")
        # Catalog creation is deliberately before the lease transaction.  The
        # Codex config never points at a catalog that EMP failed to write.
        catalog_path = write_catalog(config, self.integration_catalog_path)
        with manager.operation_lock():
            result = manager.enable(
                base_url,
                str(catalog_path.resolve()),
                service_ready=self.service_ready,
            )
            if result.ok and result.state == "active":
                with self.lock:
                    self._integration_owned = True
                    self._startup_conflicts = ()
                self._mark_runtime_pending("emp", "EMP configuration applied")
                self._sync_integration_runtime_unlocked(confirm_reload)
        return result

    def restore_integration(self, *, confirm_reload: bool = False) -> IntegrationResult:
        """Explicit recovery may repair an orphaned lease after a crash."""

        manager = self.ensure_integration_manager()
        with self.lock:
            expected_models = (
                self._runtime_expected_models
                if self._runtime_should_be_present and self._runtime_expected_models
                else self._runtime_model_ids()
            )
        with manager.operation_lock():
            result = manager.restore()
            if result.ok and result.state in ("native", "restored"):
                with self.lock:
                    self._integration_owned = False
                    self._startup_conflicts = ()
                    self._runtime_expected_models = expected_models
                self._mark_runtime_pending("native", "Native Codex configuration restored")
                with self.lock:
                    self._runtime_expected_models = expected_models
                self._persist_runtime_recovery()
                if confirm_reload:
                    self._sync_integration_runtime_unlocked(True)
        return result

    def shutdown_restore(self) -> IntegrationResult:
        """Only automatic shutdown may be limited to this process's ownership."""

        manager = self.ensure_integration_manager()
        with self.lock:
            owned = self._integration_owned
        if not owned:
            status = manager.status()
            return IntegrationResult(
                "noop",
                status.state,
                status.relation,
                status.fields,
                status.lease,
                status.conflicts,
            )
        result = manager.restore()
        if result.ok and result.state in ("native", "restored"):
            with self.lock:
                self._integration_owned = False
                self._startup_conflicts = ()
        return result

    def reconcile_startup(self, service_ready: Any) -> IntegrationResult:
        result = self.ensure_integration_manager().recover(
            re_adopt=True,
            service_ready=service_ready,
        )
        if result.action == "re_adopted" and result.state == "active":
            with self.lock:
                self._integration_owned = True
                self._startup_conflicts = ()
            self._mark_runtime_pending("emp", "EMP restarted; runtime catalog was not assumed")
        return result

    def update(self, incoming: Dict[str, Any]) -> Dict[str, Any]:
        with self.lock:
            updated = merge_web_update(self.config, incoming, self.path)
            save(updated, self.path)
            self.config = load(self.path)
            result = self.snapshot()
        try:
            if self.integration_status().state == "active":
                self._mark_runtime_pending("emp", "EMP configuration changed")
        except (IntegrationError, OSError):
            pass
        return result

    def refresh_catalog(self) -> Path:
        catalog_path = write_catalog(self.snapshot(), self.integration_catalog_path)
        try:
            if self.integration_status().state == "active":
                self._mark_runtime_pending("emp", "EMP model catalog changed")
        except (IntegrationError, OSError):
            pass
        return catalog_path

    def _remember_resolved_protocol(
        self, metadata: Dict[str, Any], requested_model: Optional[str] = None
    ) -> None:
        provider_id = metadata.get("provider_id")
        protocol = metadata.get("resolved_protocol")
        if protocol not in ("responses", "chat_completions", "anthropic_messages"):
            return
        status = metadata.get("status")
        if not isinstance(status, int) or not 200 <= status < 300:
            return
        if metadata.get("success") is False:
            return
        with self.lock:
            provider = next(
                (
                    item
                    for item in self.config.get("providers", [])
                    if item.get("id") == provider_id
                ),
                None,
            )
            if provider is None or provider.get("protocol") != "auto":
                return
            model = next(
                (
                    item
                    for item in self.config.get("models", [])
                    if item.get("id") == requested_model
                    and item.get("provider") == provider_id
                ),
                None,
            )
            observation = {
                "source": "observed",
                "confidence": 1.0,
                "observed_at": observed_at_now(),
                "endpoint_fingerprint": endpoint_fingerprint(provider.get("base_url")),
                "deployment_identity": deployment_identity(provider, model or {}),
                "upstream_model": (model or {}).get("upstream_id") or requested_model or "",
            }
            provider["resolved_protocol"] = protocol
            provider["protocol_observation"] = observation
            if model is not None:
                model["resolved_protocol"] = protocol
                model["protocol_observation"] = dict(observation)
            save(self.config, self.path)
            self.config = load(self.path)

    @staticmethod
    def _request_size(body: Dict[str, Any]) -> int:
        try:
            return len(json.dumps(body, ensure_ascii=False).encode("utf-8"))
        except (TypeError, ValueError):
            return 0

    @staticmethod
    def _diagnostic_error_class(status: Any) -> str:
        if status in (401, 403):
            return "auth"
        if status == 429:
            return "rate_limit"
        if status in (404, 405, 415, 501):
            return "protocol_rejection"
        if status in (408, 504):
            return "timeout"
        if status == 502:
            return "network"
        if isinstance(status, int) and 500 <= status <= 599:
            return "upstream_5xx"
        return "router_error"

    def _record_route_event(
        self,
        event: Mapping[str, Any],
        body: Dict[str, Any],
        started: float,
        transport: str,
        route: str,
    ) -> None:
        safe_event = dict(event)
        safe_event["route"] = route
        safe_event["transport"] = transport
        safe_event["request_bytes"] = self._request_size(body)
        safe_event["duration_ms"] = max(0, int(round((time.monotonic() - started) * 1000)))
        context = safe_event.get("context_observation")
        if isinstance(context, Mapping):
            safe_event["context_observation"] = dict(context)
            self._remember_context_calibration(safe_event, body)
        self._remember_resolved_protocol(safe_event, body.get("model"))
        self.diagnostics.record(safe_event)

    def _remember_context_calibration(
        self, event: Mapping[str, Any], body: Mapping[str, Any]
    ) -> None:
        observation = event.get("context_observation")
        if not isinstance(observation, Mapping):
            return
        if event.get("success") is True:
            outcome = "success"
        elif event.get("error_class") == "context_length_exceeded" or observation.get(
            "explicit_failure"
        ):
            outcome = "explicit_failure"
        else:
            return
        estimate = observation.get("input_estimate", observation.get("estimated_tokens"))
        model_id = event.get("model_id") or observation.get("model_id") or body.get("model")
        if not isinstance(model_id, str):
            return
        with self.lock:
            model = next(
                (item for item in self.config.get("models", []) if item.get("id") == model_id),
                None,
            )
            if model is None:
                return
            if not self.context_guard.update(model, observation, outcome, estimate):
                return
            save(self.config, self.path)
            self.config = load(self.path)

    def _record_route_failure(
        self,
        body: Dict[str, Any],
        started: float,
        transport: str,
        route: str,
        status: Any,
        context_observation: Optional[Mapping[str, Any]] = None,
        error_class: Optional[str] = None,
    ) -> None:
        model_id = body.get("model") if isinstance(body.get("model"), str) else ""
        provider_id = ""
        endpoint = ""
        deployment = "default"
        protocol = "unknown"
        snapshot = self.snapshot()
        model = next(
            (item for item in snapshot.get("models", []) if item.get("id") == model_id),
            None,
        )
        if model is not None:
            provider = next(
                (
                    item
                    for item in snapshot.get("providers", [])
                    if item.get("id") == model.get("provider")
                ),
                None,
            )
            if provider is not None:
                provider_id = provider.get("id", "")
                endpoint = endpoint_fingerprint(provider.get("base_url"))
                deployment = deployment_identity(provider, model)
                protocol = provider.get("protocol", "unknown")
        context = dict(context_observation) if isinstance(context_observation, Mapping) else {}
        if context.get("protocol") in ("responses", "chat_completions", "anthropic_messages"):
            protocol = context["protocol"]
        self._record_route_event(
            {
                "provider_id": provider_id,
                "model_id": model_id,
                "endpoint_fingerprint": endpoint,
                "deployment_identity": deployment,
                "resolved_protocol": protocol,
                "status": status,
                "success": False,
                "error_class": error_class or self._diagnostic_error_class(status),
                "protocol_decision": "explicit" if protocol != "auto" else "normal_order",
                "protocol_fallback": False,
                "response_bytes": 0,
                "context_observation": context,
            },
            body,
            started,
            transport,
            route,
        )

    def _diagnostic_stream(
        self,
        result: Any,
        metadata: Dict[str, Any],
        body: Dict[str, Any],
        started: float,
        transport: str,
        route: str,
    ):
        response_bytes = 0
        natural_end = False
        terminal = None
        try:
            for chunk in result:
                raw = chunk if isinstance(chunk, bytes) else str(chunk).encode("utf-8")
                response_bytes += len(raw)
                if b"response.completed" in raw:
                    terminal = {"status": 200, "success": True, "error_class": "none"}
                elif b"response.failed" in raw:
                    terminal = {"status": 502, "success": False, "error_class": "stream_error"}
                yield chunk
            natural_end = True
        except GeneratorExit:
            raise
        except TimeoutError:
            terminal = {"status": 504, "success": False, "error_class": "timeout"}
            raise
        except OSError:
            terminal = {"status": 502, "success": False, "error_class": "network"}
            raise
        except Exception:
            terminal = {"status": 502, "success": False, "error_class": "stream_error"}
            raise
        finally:
            close = getattr(result, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass
            event = dict(metadata)
            event.update(terminal or {
                "status": 502 if natural_end else None,
                "success": False,
                "error_class": "stream_incomplete" if natural_end else "client_disconnect",
            })
            event["response_bytes"] = response_bytes
            self._record_route_event(event, body, started, transport, route)

    def _continuity_diagnostic(
        self,
        transport: str,
        source_binding_result: str,
        same_domain: bool = False,
        handoff_attempted: bool = False,
        handoff_succeeded: bool = False,
        item_index: Optional[int] = None,
        item_type: Optional[str] = None,
        reason: Optional[str] = None,
        error_class: Optional[str] = None,
    ) -> None:
        """Record a content-free continuity outcome using strict safe fields.

        Only the allowed continuity result flags from spec section 7 are
        recorded: source_binding_result, same_domain, handoff_attempted,
        handoff_succeeded, item_index, item_type, reason, error_class,
        transport, and status. No provider/model/account/URL/opaque/
        checkpoint/prompt content is ever placed in the diagnostic.
        """
        fields: Dict[str, Any] = {
            "route": "responses",
            "transport": transport,
            "source_binding_result": source_binding_result,
            "same_domain": bool(same_domain),
            "handoff_attempted": bool(handoff_attempted),
            "handoff_succeeded": bool(handoff_succeeded),
        }
        if isinstance(item_index, int) and not isinstance(item_index, bool):
            fields["item_index"] = max(0, item_index)
        if isinstance(item_type, str) and item_type:
            fields["item_type"] = item_type[:64]
        if isinstance(reason, str) and reason:
            fields["reason"] = reason[:64]
        if isinstance(error_class, str) and error_class:
            fields["error_class"] = error_class[:64]
        try:
            self.diagnostics.record(fields)
        except Exception:
            pass

    def _register_native_compaction(
        self,
        provider: Dict[str, Any],
        model: Dict[str, Any],
        requested_slug: str,
        incoming: Dict[str, str],
        identity,
        result: bytes,
        success: bool,
    ) -> None:
        """Content-free native JSON compaction registration callback.

        The identity is the already-verified NativeRouteIdentity resolved
        by the router from the actual native route (forward or account
        auth mode). It is never re-derived from destination or incoming
        headers here.
        """
        store = self.compaction_bindings
        if store is None:
            return
        if identity is None or not success:
            return
        try:
            count = register_native_json(store, identity, json.loads(result.decode("utf-8")), success)
            if count:
                self._continuity_diagnostic(
                    "http", "registered"
                )
        except Exception:
            self._continuity_diagnostic(
                "http", "registration_failed", error_class="observer_error"
            )

    def _native_observer_stream(self, result, observer):
        """Wrap a stream so observer.abandon() runs in the finally path.

        Client disconnect, iterator close, and parse failure all reach the
        finally, ensuring pending opaque values are discarded rather than
        leaked across concurrent SSE/WS requests.
        """
        try:
            for chunk in result:
                yield chunk
        finally:
            try:
                observer.abandon()
            except Exception:
                pass
            closer = getattr(result, "close", None)
            if callable(closer):
                try:
                    closer()
                except Exception:
                    pass

    def route(
        self,
        body: Dict[str, Any],
        incoming: Dict[str, str],
        transport: Optional[str] = None,
        context_completeness: str = "high",
    ) -> Tuple[Dict[str, Any], Any]:
        body = self.provider_replay.prepare(body)
        started = time.monotonic()
        selected_transport = transport or ("sse" if body.get("stream") else "http")
        observed = False

        def on_context(
            provider: Dict[str, Any],
            model: Dict[str, Any],
            protocol: str,
            payload: Dict[str, Any],
            stream: bool,
            operation: str,
        ) -> Dict[str, Any]:
            assessment = self.context_guard.assess(
                provider,
                model,
                protocol,
                payload,
                context_completeness,
            )
            observation = assessment.to_safe_dict()
            if assessment.decision == "block":
                raise ContextGuardBlocked(assessment)
            return observation

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            self._record_route_event(
                event,
                body,
                started,
                selected_transport,
                "responses",
            )

        observer_box = []

        def on_native_stream_start(identity):
            if self.compaction_bindings is None or identity is None:
                return None
            try:
                observer = NativeCompactionObserver(
                    self.compaction_bindings, identity
                )
                observer_box.append(observer)

                def safe_observe(event):
                    try:
                        count = observer.observe(event)
                        if count:
                            self._continuity_diagnostic(
                                "sse", "registered"
                            )
                    except Exception:
                        self._continuity_diagnostic(
                            "sse", "registration_failed",
                            error_class="observer_error"
                        )

                return safe_observe
            except Exception:
                return None

        def on_native_response(provider, model, slug, transient, identity, raw, success):
            self._register_native_compaction(
                dict(provider), dict(model), slug, dict(transient),
                identity, raw, success
            )

        def on_continuity(
            source_binding_result, same_domain,
            handoff_attempted, handoff_succeeded,
            item_index, item_type, reason
        ):
            self._continuity_diagnostic(
                selected_transport,
                source_binding_result,
                same_domain=same_domain,
                handoff_attempted=handoff_attempted,
                handoff_succeeded=handoff_succeeded,
                item_index=item_index,
                item_type=item_type,
                reason=reason,
            )

        try:
            metadata, result = proxy(
                self.snapshot(),
                body,
                incoming,
                on_observation,
                on_context,
                binding_store=self.compaction_bindings,
                on_native_response=on_native_response,
                on_native_stream_start=on_native_stream_start,
                on_continuity=on_continuity,
            )
        except RouterError as exc:
            if not observed:
                self._record_route_failure(
                    body,
                    started,
                    selected_transport,
                    "responses",
                    exc.status,
                    getattr(exc, "context_observation", None),
                    "context_length_exceeded"
                    if isinstance(exc, ContextLengthError)
                    else None,
                )
            raise
        except Exception:
            if not observed:
                self._record_route_failure(
                    body, started, selected_transport, "responses", 500
                )
            raise
        if metadata.get("kind") == "stream":
            if not metadata.get("observation_attached"):
                result = self._diagnostic_stream(
                    result,
                    metadata,
                    body,
                    started,
                    selected_transport,
                    "responses",
                )
            if observer_box:
                result = self._native_observer_stream(result, observer_box[0])
            result = self.provider_replay.observe_stream(body.get("model"), result)
        else:
            self.provider_replay.observe_bytes(body.get("model"), result)
            if not observed:
                event = dict(metadata)
                event["response_bytes"] = len(result) if isinstance(result, (bytes, bytearray)) else 0
                self._record_route_event(
                    event,
                    body,
                    started,
                    selected_transport,
                    "responses",
                )
        return metadata, result

    def route_compact(
        self,
        body: Dict[str, Any],
        incoming: Dict[str, str],
        transport: Optional[str] = None,
        context_completeness: str = "high",
    ) -> Tuple[Dict[str, Any], bytes]:
        started = time.monotonic()
        selected_transport = transport or "http"
        observed = False

        def on_context(
            provider: Dict[str, Any],
            model: Dict[str, Any],
            protocol: str,
            payload: Dict[str, Any],
            stream: bool,
            operation: str,
        ) -> Dict[str, Any]:
            assessment = self.context_guard.assess(
                provider,
                model,
                protocol,
                payload,
                context_completeness,
            )
            observation = assessment.to_safe_dict()
            if assessment.decision == "block":
                raise ContextGuardBlocked(assessment)
            return observation

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            self._record_route_event(
                event,
                body,
                started,
                selected_transport,
                "compact",
            )

        def on_native_response_compact(provider, model, slug, transient, identity, raw, success):
            self._register_native_compaction(
                dict(provider), dict(model), slug, dict(transient),
                identity, raw, success
            )

        def on_continuity_compact(
            source_binding_result, same_domain,
            handoff_attempted, handoff_succeeded,
            item_index, item_type, reason
        ):
            self._continuity_diagnostic(
                selected_transport,
                source_binding_result,
                same_domain=same_domain,
                handoff_attempted=handoff_attempted,
                handoff_succeeded=handoff_succeeded,
                item_index=item_index,
                item_type=item_type,
                reason=reason,
            )

        try:
            metadata, result = proxy_compact(
                self.snapshot(), body, incoming, on_observation, on_context,
                binding_store=self.compaction_bindings,
                on_native_response=on_native_response_compact,
                on_continuity=on_continuity_compact,
            )
        except RouterError as exc:
            if not observed:
                self._record_route_failure(
                    body,
                    started,
                    selected_transport,
                    "compact",
                    exc.status,
                    getattr(exc, "context_observation", None),
                    "context_length_exceeded"
                    if isinstance(exc, ContextLengthError)
                    else None,
                )
            raise
        except Exception:
            if not observed:
                self._record_route_failure(
                    body, started, selected_transport, "compact", 500
                )
            raise
        if not observed:
            event = dict(metadata)
            event["response_bytes"] = len(result) if isinstance(result, (bytes, bytearray)) else 0
            self._record_route_event(
                event,
                body,
                started,
                selected_transport,
                "compact",
            )
        return metadata, result

    def export_migration(self, password: str) -> bytes:
        with self.lock:
            return export_bundle(self.config, self.path, password)

    def import_migration(self, bundle: bytes, password: str) -> Dict[str, int]:
        with self.lock:
            self.config, summary = import_bundle(self.config, bundle, password, self.path)
            catalog_path = write_catalog(self.config, generated_catalog_path())
            summary["catalog_path"] = str(catalog_path.resolve())
            return summary

    def discover_provider_models(
        self, provider_id: str, selected: Optional[list] = None
    ) -> Dict[str, Any]:
        with self.lock:
            provider = next(
                (item for item in self.config.get("providers", []) if item.get("id") == provider_id),
                None,
            )
            if provider is None or not provider.get("enabled", True):
                raise ConfigError("provider is missing or disabled: %s" % provider_id)
            provider = dict(provider)

        with self.discovery_lock:
            discovered = discover_models(provider)
        available_count = len(discovered)
        with self.lock:
            if selected is None:
                return {
                    "provider": provider_id,
                    "protocol": provider.get("protocol"),
                    "available": available_count,
                    "models": discovered,
                    "added": 0,
                }
            if not isinstance(selected, list) or any(not isinstance(item, str) for item in selected):
                raise ConfigError("selected models must be a list of model IDs")
            selected_ids = set(selected)
            available_ids = {item.get("upstream_id") for item in discovered}
            unknown = selected_ids - available_ids
            if unknown:
                raise ConfigError("selected model is not in the discovered list")
            models = list(self.config.get("models", []))
            by_id = {item.get("id"): item for item in models}
            hidden = 0
            for existing in models:
                if (
                    existing.get("provider") == provider_id
                    and existing.get("upstream_id") in available_ids
                    and existing.get("upstream_id") not in selected_ids
                    and existing.get("enabled", True)
                ):
                    existing["enabled"] = False
                    hidden += 1
            discovered = [
                item for item in discovered if item.get("upstream_id") in selected_ids
            ]
            added = 0
            observed_at = observed_at_now()
            for item in discovered:
                upstream_id = item.get("upstream_id", "")
                model_id = provider_id + "/" + upstream_id
                if not upstream_id or model_id in by_id:
                    existing = by_id.get(model_id)
                    if existing:
                        for field in _MERGE_SCALAR_FIELDS + _MERGE_LIST_FIELDS:
                            has_entry = _has_provenance_entry(item, field)
                            incoming_source = _field_source(item, field)
                            if not has_entry:
                                raw_val = item.get(field)
                                if field in (
                                    "context_window",
                                    "output_limit",
                                    "max_input_tokens",
                                ):
                                    incoming_source = "advertised" if raw_val else "unknown"
                                elif field == "input_modalities":
                                    incoming_source = input_modalities_metadata_source(raw_val)
                                elif field == "output_modalities":
                                    incoming_source = output_modalities_metadata_source(raw_val)
                                elif field in ("reasoning_levels", "supported_protocols"):
                                    incoming_source = "advertised" if raw_val else "unknown"
                            _merge_discovered_field(
                                existing, item, field, incoming_source, observed_at
                            )
                        for field in _MERGE_NESTED_BOOL_FIELDS:
                            has_entry = _has_provenance_entry(item, field)
                            incoming_source = _field_source(item, field)
                            if not has_entry:
                                incoming_source = "advertised"
                            _merge_discovered_nested_bool(
                                existing, item, field, incoming_source, observed_at
                            )
                        if item.get("created_at") and not existing.get("created_at"):
                            existing["created_at"] = item["created_at"]
                    continue
                model = _build_new_model_from_discovery(item, provider_id, observed_at)
                models.append(model)
                by_id[model_id] = model
                added += 1
            updated = dict(self.config)
            updated["models"] = models
            self.config = load_from_value(updated)
            save(self.config, self.path)
            self.config = load(self.path)
            catalog_path = write_catalog(self.config, generated_catalog_path())
            return {
                "provider": provider_id,
                "protocol": provider.get("protocol"),
                "available": available_count,
                "added": added,
                "hidden": hidden,
                "catalog_path": str(catalog_path.resolve()),
                "model_count": len(build_catalog(self.config)["models"]),
            }

    def import_account(self, metadata: Dict[str, Any], auth_json: Dict[str, Any]) -> Dict[str, Any]:
        with account_refresh_lock(metadata.get("id")):
            with self.lock:
                account_id = metadata.get("id")
                prefix = metadata.get("prefix")
                current_accounts = self.config.get("accounts", [])
                for account in current_accounts:
                    if account.get("id") != account_id and account.get("prefix") == prefix:
                        raise ConfigError("account prefix is already in use: %s" % prefix)
                account = import_account(self.config, metadata, auth_json, self.path)
                accounts = [item for item in current_accounts if item.get("id") != account["id"]]
                accounts.append(account)
                updated = dict(self.config)
                updated["accounts"] = accounts
                self.config = load_from_value(updated)
                save(self.config, self.path)
                return account

    def refresh_account(self, account_id: str) -> Dict[str, Any]:
        with self.lock:
            if not any(item.get("id") == account_id for item in self.config.get("accounts", [])):
                raise QuotaError("unknown account: %s" % account_id)
        with account_refresh_lock(account_id):
            with self.lock:
                account = next(
                    (item for item in self.config.get("accounts", []) if item.get("id") == account_id),
                    None,
                )
                if account is None:
                    raise QuotaError("unknown account: %s" % account_id)
                target = dict(account)
            # An imported account that duplicates the current native Codex
            # login has an EMP snapshot that can become stale after Codex
            # rotates tokens. Query the live native credential instead of the
            # stale snapshot; do not request token rotation and never persist
            # back to the EMP vault or mutate the native auth file.
            duplicates = duplicate_account_status([target])
            if account_id in duplicates:
                quota = read_native_login_quota()
            else:
                quota = refresh_account_quota(target)
            with self.lock:
                for item in self.config.get("accounts", []):
                    if item.get("id") == account_id and item.get("auth_file") == target.get("auth_file"):
                        item["quota"] = quota
                        save(self.config, self.path)
                        return dict(item)
                raise QuotaError("account changed during quota refresh")

    def delete_account(self, account_id: str) -> None:
        with self.lock:
            if not any(item.get("id") == account_id for item in self.config.get("accounts", [])):
                raise ConfigError("unknown account: %s" % account_id)
        with account_refresh_lock(account_id):
            self._delete_account(account_id)

    def _delete_account(self, account_id: str) -> None:
        with self.lock:
            accounts = self.config.get("accounts", [])
            target = next((item for item in accounts if item.get("id") == account_id), None)
            if target is None:
                raise ConfigError("unknown account: %s" % account_id)

            root = account_root(self.config, self.path).resolve()
            account_dir = (root / account_id).resolve()
            auth_path = Path(target.get("auth_file", "")).expanduser()
            if not auth_path.is_absolute():
                auth_path = self.path.parent / auth_path
            if auth_path.resolve() != account_dir / "auth.json.enc":
                raise ConfigError("refusing to delete credentials outside the account store")

            updated = dict(self.config)
            updated["accounts"] = [item for item in accounts if item.get("id") != account_id]
            save(updated, self.path)
            self.config = load(self.path)

            for private_file in (auth_path, account_dir / "config.toml"):
                try:
                    private_file.unlink()
                except FileNotFoundError:
                    pass
            try:
                account_dir.rmdir()
            except OSError:
                pass


def load_from_value(value: Dict[str, Any]) -> Dict[str, Any]:
    """Normalize an in-memory config without creating a second file format."""
    from .config import normalize

    return normalize(value)


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False).encode("utf-8")


def _management_config(config: Dict[str, Any]) -> Dict[str, Any]:
    result = public_config(config)
    result["subscription_models"] = subscription_model_options(config)
    return result


def _management_capabilities(state: AppState) -> Dict[str, Any]:
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
        protocol = effective if effective in _DIAGNOSTIC_PROTOCOLS else "unknown"
        record["context"] = state.context_guard.status(provider, model, protocol)
    return {"capabilities": records}


def _integration_next_action(state: str, service_health: str) -> str:
    if state in ("prepared", "restoring"):
        return "restore"
    if state == "active":
        return "none" if service_health == "ready" else "confirm service health or restore"
    if state == "conflict":
        return "restore"
    if state == "native":
        return "enable default Codex"
    return "none"


def _integration_summary(
    state: AppState,
    result: Optional[IntegrationResult] = None,
) -> Dict[str, Any]:
    observed = state.integration_status()
    service_health = "ready" if state.service_ready() else "not_ready"
    summary_state = observed.state
    relation = observed.relation
    conflicts = list(observed.conflicts)
    startup_conflicts = state.startup_conflicts()
    if startup_conflicts:
        summary_state = "conflict"
        conflicts = list(startup_conflicts)
    if result is not None and not result.ok:
        summary_state = "conflict"
        relation = result.relation
        conflicts = list(result.conflicts)
    lease_status = observed.lease.status if observed.lease is not None else "none"
    runtime = state.runtime_sync_snapshot()
    runtime_state = runtime.get("state", NOT_CHECKED)
    runtime_action_required = runtime_state in {
        RELOAD_REQUIRED,
        STOP_FAILED,
        VERIFICATION_FAILED,
        UNSUPPORTED,
    }
    if runtime_action_required:
        next_action = "reconnect Codex"
    else:
        next_action = _integration_next_action(summary_state, service_health)
    if summary_state == "active":
        configuration_state = "emp_applied"
    elif summary_state in ("native", "restored"):
        configuration_state = "native"
    else:
        configuration_state = summary_state
    return {
        "configuration": {
            "state": configuration_state,
            "relation": relation,
            "config_exists": observed.config_exists,
            "lease_status": lease_status,
            "conflicts": conflicts,
        },
        "runtime": {
            "state": runtime_state,
            "target": runtime.get("target", "native"),
            "verified": bool(runtime.get("verified", False)),
            "confidence": runtime.get("confidence", "not_checked"),
            "action_required": runtime_action_required,
            "detail": runtime.get("detail", ""),
            "last_known": runtime.get("last_known"),
        },
        "service_health": service_health,
        "next_action": next_action,
    }


def _integration_error_message(error: BaseException) -> str:
    if isinstance(error, ServiceNotReady):
        return "EMP service is not ready"
    if isinstance(error, IntegrationError):
        return "integration state is unavailable"
    return "integration operation failed"


def _bound_base_url(server_address: Tuple[Any, ...]) -> str:
    host = str(server_address[0])
    if host in ("0.0.0.0", "::"):
        host = "127.0.0.1"
    elif ":" in host and not host.startswith("["):
        host = "[%s]" % host
    return "http://%s:%d/v1" % (host, int(server_address[1]))


def _integration_target(
    state: AppState,
    server_address: Tuple[Any, ...],
) -> Tuple[str, str]:
    return _bound_base_url(server_address), str(state.integration_catalog_path.resolve())


def _startup_target_conflict(
    state: AppState,
    base_url: str,
    catalog_path: str,
) -> Optional[IntegrationResult]:
    status = state.integration_status()
    lease = status.lease
    if (
        lease is None
        or lease.status == "restored"
        or status.relation != "applied"
    ):
        return None
    conflicts = []
    if lease.fields["openai_base_url"].applied.value != base_url:
        conflicts.append("listener_mismatch")
    if lease.fields["model_catalog_json"].applied.value != catalog_path:
        conflicts.append("catalog_mismatch")
    if not conflicts:
        return None
    names = tuple(conflicts)
    state.set_startup_conflicts(names)
    return IntegrationResult(
        "conflict",
        "conflict",
        status.relation,
        status.fields,
        lease,
        names,
    )


def make_handler(state: AppState):
    class Handler(BaseHTTPRequestHandler):
        server_version = "EasyMultiProvider/%s" % __version__

        def setup(self) -> None:
            super().setup()
            self._request_started_at = time.monotonic()
            self._response_status = None
            self._http_request_logged = False
            self._unexpected_exception_logged = False
            self.connection.settimeout(30)
            # Reaching a handler means the listener is bound and accepting
            # requests; this is the readiness proof used by enable.
            state.mark_service_ready()

        def _begin_http_request(self) -> None:
            self._request_started_at = time.monotonic()
            self._response_status = None
            self._http_request_logged = False
            self._unexpected_exception_logged = False
            self.command = ""
            self.path = ""
            self.raw_requestline = b""

        def _declared_request_bytes(self) -> int:
            headers = getattr(self, "headers", None)
            raw_value = headers.get("Content-Length", "0") if headers else "0"
            try:
                value = int(raw_value)
            except (TypeError, ValueError):
                return 0
            if value < 0:
                return 0
            return min(value, _HTTP_REQUEST_BYTES_MAX)

        def _record_http_request_once(self) -> None:
            if self._http_request_logged:
                return
            self._http_request_logged = True
            if not getattr(self, "raw_requestline", b""):
                return
            try:
                method = self.command if isinstance(self.command, str) else ""
                raw_path = self.path if isinstance(self.path, str) else ""
                path = _diagnostic_http_path(raw_path)
                status = self._response_status
                elapsed = max(0.0, time.monotonic() - self._request_started_at)
                duration_ms = min(3_600_000, int(round(elapsed * 1000)))
                state.journal.event(
                    "info",
                    "http_request",
                    method=method,
                    path=path,
                    status=status,
                    request_bytes=self._declared_request_bytes(),
                    duration_ms=duration_ms,
                )
            except Exception:
                pass

        def _record_unexpected_exception(self, exception: BaseException) -> None:
            if self._unexpected_exception_logged:
                return
            self._unexpected_exception_logged = True
            try:
                state.journal.exception_event(
                    "error",
                    "internal_error",
                    _HTTP_HANDLER_STAGE,
                    exception,
                )
            except Exception:
                pass

        def _record_management_event(
            self,
            event_name: str,
            started: float,
            result_class: str,
            **fields: Any,
        ) -> None:
            try:
                elapsed = max(0.0, time.monotonic() - started)
                state.journal.event(
                    "info",
                    event_name,
                    duration_ms=min(3_600_000, int(round(elapsed * 1000))),
                    result_class=result_class,
                    **fields,
                )
            except Exception:
                pass

        def _management_count(self, value: Any) -> int:
            result = _safe_diagnostic_int(value, _MANAGEMENT_COUNT_MAX)
            return result if result is not None else 0

        def _management_failure_class(self, exception: BaseException) -> str:
            if isinstance(exception, EmptyEmpCatalog):
                return "empty_catalog"
            if isinstance(exception, IntegrationError):
                return "integration_error"
            if isinstance(exception, QuotaError):
                return "quota_error"
            if isinstance(exception, RouterError):
                return "router_error"
            if isinstance(exception, ConfigError):
                return "config_error"
            if isinstance(exception, ValueError):
                return "value_error"
            if isinstance(exception, OSError):
                return "io_error"
            return "internal_error"

        def _account_ref(self, account_id: Any) -> str:
            if not isinstance(account_id, str) or not account_id:
                return ""
            try:
                value = state.journal.pseudonym(account_id)
                return value if isinstance(value, str) else ""
            except Exception:
                return ""

        def _integration_operation_fields(
            self,
            operation: str,
            summary: Optional[Mapping[str, Any]],
        ) -> Dict[str, Any]:
            configuration = (
                summary.get("configuration", {})
                if isinstance(summary, Mapping)
                else {}
            )
            if not isinstance(configuration, Mapping):
                configuration = {}
            return {
                "operation": operation,
                "state": _safe_diagnostic_text(
                    configuration.get("state"), _DIAGNOSTIC_ID
                )
                or "unknown",
                "relation": _safe_diagnostic_text(
                    configuration.get("relation"), _DIAGNOSTIC_ID
                )
                or "unknown",
                "conflicts": _safe_diagnostic_sequence(
                    configuration.get("conflicts")
                ),
            }

        def _migration_numeric_fields(
            self, summary: Optional[Mapping[str, Any]]
        ) -> Dict[str, Any]:
            fields = {}
            if not isinstance(summary, Mapping):
                return fields
            forbidden_fragments = (
                "auth",
                "bundle",
                "credential",
                "key",
                "password",
                "path",
                "secret",
                "token",
            )
            for raw_key, value in summary.items():
                key = raw_key if isinstance(raw_key, str) else ""
                lowered = key.lower()
                if (
                    not _MANAGEMENT_SCALAR_KEY.fullmatch(key)
                    or key in {"duration_ms", "operation", "result_class"}
                    or any(fragment in lowered for fragment in forbidden_fragments)
                ):
                    continue
                if isinstance(value, bool):
                    fields[key] = value
                elif isinstance(value, int):
                    fields[key] = self._management_count(value)
            return fields

        def handle_one_request(self) -> None:
            self._begin_http_request()
            try:
                super().handle_one_request()
            except Exception as exc:
                self._record_unexpected_exception(exc)
                raise
            finally:
                self._record_http_request_once()

        def finish(self) -> None:
            try:
                super().finish()
            finally:
                self._record_http_request_once()

        def send_response(self, code: int, message: Optional[str] = None) -> None:
            try:
                status = int(code)
            except (TypeError, ValueError):
                status = None
            self._response_status = status if status is not None and 100 <= status <= 599 else None
            super().send_response(code, message)

        def log_message(self, format: str, *args: Any) -> None:
            # The bootstrap URL contains a one-time secret; never log its query.
            message = format % args if args else format
            message = message.replace(self.path, urlparse(self.path).path)
            super().log_message("%s", message)

        def _same_origin(self) -> bool:
            host = self.headers.get("Host", "").lower().rstrip(".")
            port = self.server.server_address[1]
            allowed_hosts = {"127.0.0.1:%d" % port, "localhost:%d" % port}
            if host not in allowed_hosts:
                return False
            origin = self.headers.get("Origin", "")
            if not origin:
                return True
            try:
                parsed = urlparse(origin)
                return (
                    parsed.scheme in ("http", "https")
                    and not parsed.username
                    and not parsed.password
                    and parsed.hostname in ("127.0.0.1", "localhost")
                    and parsed.port == port
                )
            except ValueError:
                return False

        def _has_session(self) -> bool:
            cookie = SimpleCookie()
            try:
                cookie.load(self.headers.get("Cookie", ""))
            except (TypeError, ValueError):
                return False
            supplied = cookie.get("emp_session")
            return bool(
                supplied
                and hmac.compare_digest(supplied.value, state.session_token)
            )

        def _has_bootstrap(self) -> bool:
            query = parse_qs(urlparse(self.path).query, keep_blank_values=True)
            values = query.get("bootstrap", [])
            if len(values) != 1 or not hmac.compare_digest(values[0], state.bootstrap_token):
                return False
            with state.lock:
                if state.bootstrap_used:
                    return False
                state.bootstrap_used = True
                return True

        def _management_allowed(self) -> bool:
            return self._same_origin() and self._has_session()

        def _proxy_allowed(self) -> bool:
            return self._same_origin() and (
                self._has_session()
                or valid_caller_authorization(self.headers.get("Authorization", ""))
            )

        def _session_header(self) -> str:
            return "emp_session=%s; HttpOnly; SameSite=Strict; Path=/" % state.session_token

        def _send(
            self,
            status: int,
            body: bytes,
            content_type: str = "application/json",
            headers: Optional[Dict[str, str]] = None,
        ) -> None:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            if content_type == "application/json":
                self.send_header("Cache-Control", "no-store")
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)

        def _error(self, status: int, message: str) -> None:
            self._send(status, _json_bytes({"error": {"message": message}}))

        def _body(self, max_length: int = 5 * 1024 * 1024) -> Dict[str, Any]:
            if self.headers.get_content_type() != "application/json":
                raise ConfigError("Content-Type must be application/json")
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                raise ConfigError("invalid Content-Length")
            if length < 0:
                raise ConfigError("Content-Length cannot be negative")
            if length > max_length:
                raise ConfigError("request body is too large")
            raw = self.rfile.read(length)
            if len(raw) != length:
                raise ConfigError("request body is incomplete")
            try:
                raw = decode_content(
                    raw,
                    self.headers.get("Content-Encoding", ""),
                    max_length,
                )
            except TransportError as exc:
                raise ConfigError(str(exc)) from exc
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                raise ConfigError("request body must be valid JSON: %s" % exc)
            if not isinstance(value, dict):
                raise ConfigError("request body must be a JSON object")
            return value

        def _websocket_error(
            self,
            websocket: WebSocketConnection,
            message: str,
            status: int = 400,
            code: str = "invalid_request",
            context: Optional[Mapping[str, Any]] = None,
        ) -> None:
            error = {"code": code, "message": message}
            if isinstance(context, Mapping):
                error["context"] = dict(context)
            websocket.send_json({"type": "error", "status": status, "error": error})

        def _websocket_events(self, metadata: Dict[str, Any], result: Any):
            kind = metadata.get("kind")
            if kind == "stream":
                iterator = iter(result)
                try:
                    for event in sse_json_events(iterator):
                        yield event
                finally:
                    close = getattr(iterator, "close", None)
                    if callable(close):
                        close()
                return
            if kind == "raw_stream":
                try:
                    chunks = iter(lambda: result.read(8192), b"")
                    for event in sse_json_events(chunks):
                        yield event
                finally:
                    result.close()
                return
            try:
                response = json.loads(result.decode("utf-8"))
            except (AttributeError, UnicodeDecodeError, ValueError) as exc:
                raise TransportError("upstream response is not valid JSON") from exc
            if not isinstance(response, dict):
                raise TransportError("upstream response must be a JSON object")
            response_id = response.get("id") or "resp_" + uuid.uuid4().hex
            response["id"] = response_id
            yield {"type": "response.created", "response": {"id": response_id}}
            for index, item in enumerate(response.get("output", []) or []):
                if isinstance(item, dict):
                    yield {
                        "type": "response.output_item.done",
                        "output_index": index,
                        "item": item,
                    }
            response_status = str(response.get("status") or "completed")
            terminal_type = {
                "completed": "response.completed",
                "incomplete": "response.incomplete",
                "failed": "response.failed",
            }.get(response_status, "response.completed")
            yield {"type": terminal_type, "response": response}

        def _serve_responses_websocket(self) -> None:
            if not self._proxy_allowed():
                self._error(
                    401 if self._same_origin() else 403,
                    "proxy caller authentication is required",
                )
                return
            connection_tokens = {
                item.strip().lower()
                for item in self.headers.get("Connection", "").split(",")
            }
            if (
                self.headers.get("Upgrade", "").lower() != "websocket"
                or "upgrade" not in connection_tokens
                or self.headers.get("Sec-WebSocket-Version") != "13"
            ):
                self._error(400, "invalid websocket upgrade")
                return
            try:
                accept = websocket_accept(self.headers.get("Sec-WebSocket-Key", ""))
            except TransportError as exc:
                self._error(400, str(exc))
                return
            self.wfile.write(
                (
                    "HTTP/1.1 101 Switching Protocols\r\n"
                    "Upgrade: websocket\r\n"
                    "Connection: Upgrade\r\n"
                    "Sec-WebSocket-Accept: %s\r\n\r\n" % accept
                ).encode("ascii")
            )
            self._response_status = 101
            self.wfile.flush()
            self.close_connection = True
            self.connection.settimeout(180)
            websocket = WebSocketConnection(self.rfile, self.wfile)
            last_response_id = None
            last_input = None
            try:
                while True:
                    text = websocket.receive_text()
                    if text is None:
                        return
                    try:
                        request = json.loads(text)
                        if not isinstance(request, dict):
                            raise TransportError("websocket request must be a JSON object")
                        if request.pop("type", None) != "response.create":
                            raise TransportError("websocket request.type must be response.create")
                        previous_response_id = request.pop("previous_response_id", None)
                        generate = request.pop("generate", None)
                        current_input = request.get("input", [])
                        if previous_response_id is not None:
                            if previous_response_id != last_response_id or last_input is None:
                                self._websocket_error(
                                    websocket,
                                    "Previous response was not found. Retrying the full request.",
                                    404,
                                    "previous_response_not_found",
                                    {
                                        "context_decision": "warned",
                                        "confidence": 0.0,
                                        "source": "unknown",
                                        "completeness": "lost",
                                        "next_action": "start a new native session",
                                    },
                                )
                                continue
                            if not isinstance(last_input, list) or not isinstance(current_input, list):
                                raise TransportError("incremental websocket input must be a list")
                            if _ws_replay_size(last_input, current_input) is None:
                                self._websocket_error(
                                    websocket,
                                    "WebSocket context state is too large; start a new native session.",
                                    413,
                                    "context_state_too_large",
                                    {
                                        "context_decision": "warned",
                                        "confidence": 0.0,
                                        "source": "unknown",
                                        "completeness": "unknown",
                                        "next_action": "start a new native session",
                                    },
                                )
                                last_response_id = None
                                last_input = None
                                continue
                            request["input"] = last_input + current_input
                        if generate is False:
                            response_id = "resp_" + uuid.uuid4().hex
                            candidate_input = request.get("input", [])
                            if _ws_replay_size(candidate_input) is None:
                                self._websocket_error(
                                    websocket,
                                    "WebSocket context state is too large; start a new native session.",
                                    413,
                                    "context_state_too_large",
                                    {
                                        "context_decision": "warned",
                                        "confidence": 0.0,
                                        "source": "unknown",
                                        "completeness": "unknown",
                                        "next_action": "start a new native session",
                                    },
                                )
                                last_response_id = None
                                last_input = None
                                continue
                            last_response_id = response_id
                            last_input = candidate_input
                            usage = {
                                "input_tokens": 0,
                                "input_tokens_details": None,
                                "output_tokens": 0,
                                "output_tokens_details": None,
                                "total_tokens": 0,
                            }
                            websocket.send_json(
                                {"type": "response.created", "response": {"id": response_id}}
                            )
                            websocket.send_json(
                                {
                                    "type": "response.completed",
                                    "response": {
                                        "id": response_id,
                                        "object": "response",
                                        "status": "completed",
                                        "output": [],
                                        "usage": usage,
                                    },
                                }
                            )
                            continue
                        metadata, result = state.route(
                            request,
                            {key: value for key, value in self.headers.items()},
                            transport="websocket",
                            context_completeness="high",
                        )
                        output_items = []
                        completed_id = None
                        for event in self._websocket_events(metadata, result):
                            websocket.send_json(event)
                            if event.get("type") == "response.output_item.done":
                                item = event.get("item")
                                if isinstance(item, dict):
                                    output_items.append(item)
                            if event.get("type") == "response.completed":
                                response = event.get("response")
                                if isinstance(response, dict):
                                    completed_id = response.get("id")
                                    if not output_items and isinstance(response.get("output"), list):
                                        output_items = [
                                            item for item in response["output"] if isinstance(item, dict)
                                        ]
                        if isinstance(completed_id, str) and completed_id:
                            base_input = request.get("input", [])
                            if isinstance(base_input, list):
                                if _ws_replay_size(base_input, output_items) is None:
                                    self._websocket_error(
                                        websocket,
                                        "WebSocket context state is too large; start a new native session.",
                                        413,
                                        "context_state_too_large",
                                        {
                                            "context_decision": "warned",
                                            "confidence": 0.0,
                                            "source": "unknown",
                                            "completeness": "unknown",
                                            "next_action": "start a new native session",
                                        },
                                    )
                                    last_response_id = None
                                    last_input = None
                                else:
                                    last_response_id = completed_id
                                    last_input = base_input + output_items
                    except ContextHandoffRequiredError as exc:
                        websocket.send_json(_handoff_ws_event(exc))
                        continue
                    except ContextLengthError as exc:
                        self._websocket_error(
                            websocket,
                            str(exc),
                            exc.status,
                            "context_length_exceeded",
                        )
                    except RouterError as exc:
                        self._websocket_error(websocket, str(exc), exc.status, "router_error")
                    except (TransportError, ValueError) as exc:
                        self._websocket_error(websocket, str(exc))
            except EOFError:
                return
            except WebSocketProtocolError as exc:
                websocket.close(exc.code, str(exc))
            except Exception as exc:
                self._record_unexpected_exception(exc)
                websocket.close(1011, "internal server error")
            finally:
                last_response_id = None
                last_input = None
                websocket.close()

        def do_GET(self) -> None:
            path = urlparse(self.path).path
            if path == "/v1/responses" and self.headers.get("Upgrade", "").lower() == "websocket":
                self._serve_responses_websocket()
                return
            if path in ("/", "/index.html") and not self._same_origin():
                self._error(403, "cross-origin Web UI request rejected")
                return
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            if path in ("/", "/index.html"):
                if not self._has_session():
                    if not self._has_bootstrap():
                        self._error(401, "open the management URL printed by EasyMultiProvider")
                        return
                    self._send(
                        303,
                        b"",
                        "text/plain; charset=utf-8",
                        {
                            "Location": "/",
                            "Set-Cookie": self._session_header(),
                        },
                    )
                    return
                self._send(
                    200,
                    WEB_FILE.read_bytes(),
                    "text/html; charset=utf-8",
                    {"Set-Cookie": self._session_header()},
                )
                return
            if path == "/healthz":
                self._send(200, _json_bytes({"status": "ok"}))
                return
            if path == "/api/config":
                self._send(200, _json_bytes(_management_config(state.snapshot())))
                return
            if path == "/api/capabilities":
                try:
                    self._send(200, _json_bytes(_management_capabilities(state)))
                except (ConfigError, OSError, ValueError):
                    self._error(409, "capability state is unavailable")
                return
            if path == "/api/diagnostics":
                self._send(200, _json_bytes(state.diagnostics.snapshot()))
                return
            if path == "/api/accounts":
                self._send(200, _json_bytes({"accounts": public_accounts(state.snapshot().get("accounts", []))}))
                return
            if path == "/api/integration":
                try:
                    self._send(200, _json_bytes(_integration_summary(state)))
                except (IntegrationError, OSError) as exc:
                    self._send(
                        503,
                        _json_bytes(
                            {
                                "error": {
                                    "code": "integration_unavailable",
                                    "message": _integration_error_message(exc),
                                }
                            }
                        ),
                    )
                return
            if path == "/v1/models":
                catalog = build_catalog(state.snapshot())
                data = [
                    {
                        "id": model.get("slug"),
                        "object": "model",
                        "created": 0,
                        "owned_by": "easy-multi-provider",
                    }
                    for model in catalog["models"]
                    if model.get("visibility", "list") == "list"
                ]
                self._send(200, _json_bytes({"object": "list", "data": data}))
                return
            if path.startswith("/v1/models/"):
                model_id = unquote(path[len("/v1/models/"):])
                catalog = build_catalog(state.snapshot())
                if any(model.get("slug") == model_id for model in catalog["models"]):
                    self._send(200, _json_bytes({"id": model_id, "object": "model", "created": 0}))
                else:
                    self._error(404, "unknown model: %s" % model_id)
                return
            self._error(404, "not found")

        def do_POST(self) -> None:
            path = urlparse(self.path).path
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            if path in ("/v1/responses", "/v1/responses/compact", "/v1/alpha/search") and not self._proxy_allowed():
                self._error(401 if self._same_origin() else 403, "proxy caller authentication is required")
                return
            operation_started = time.monotonic()
            operation_logged = False
            body: Dict[str, Any] = {}

            def emit_operation(
                event_name: str, result_class: str, **fields: Any
            ) -> None:
                nonlocal operation_logged
                if operation_logged:
                    return
                operation_logged = True
                self._record_management_event(
                    event_name,
                    operation_started,
                    result_class,
                    **fields,
                )

            def emit_operation_failure(exception: BaseException) -> None:
                result_class = self._management_failure_class(exception)
                if path == "/api/providers/discover":
                    selected = body.get("selected")
                    emit_operation(
                        "provider_discovery",
                        result_class,
                        provider_id=_safe_diagnostic_text(
                            body.get("provider"), _DIAGNOSTIC_ID
                        ),
                        available=0,
                        selected=self._management_count(
                            len(selected) if isinstance(selected, list) else 0
                        ),
                        added=0,
                        hidden=0,
                        model_count=0,
                    )
                elif path == "/api/catalog/refresh":
                    emit_operation(
                        "catalog_refresh",
                        result_class,
                        visible_model_count=0,
                    )
                elif path.startswith("/api/integration/"):
                    operation = path.rsplit("/", 1)[-1]
                    if operation in {"enable", "restore", "reload"}:
                        emit_operation(
                            "integration_operation",
                            result_class,
                            **self._integration_operation_fields(operation, None),
                        )
                elif path == "/api/accounts/import":
                    emit_operation(
                        "account_operation",
                        result_class,
                        operation="import",
                        account_ref=self._account_ref(body.get("id")),
                    )
                elif path.startswith("/api/accounts/") and path.endswith("/quota"):
                    account_id = unquote(
                        path[len("/api/accounts/") : -len("/quota")].rstrip("/")
                    )
                    emit_operation(
                        "account_operation",
                        result_class,
                        operation="quota_refresh",
                        account_ref=self._account_ref(account_id),
                    )
                elif path in {"/api/migration/import", "/api/migration/export"}:
                    emit_operation(
                        "migration_operation",
                        result_class,
                        operation=path.rsplit("/", 1)[-1],
                    )
            try:
                body = self._body(
                    32 * 1024 * 1024
                    if path in ("/api/migration/import", "/v1/responses", "/v1/responses/compact")
                    else 5 * 1024 * 1024
                )
                if path == "/v1/alpha/search":
                    status, content_type, raw = forward_native_search(
                        state.snapshot(),
                        body,
                        {key: value for key, value in self.headers.items()},
                    )
                    self._send(status, raw, content_type)
                    return
                if path == "/api/integration/enable":
                    if body.get("confirm_reload") is not True:
                        summary = _integration_summary(state)
                        summary["error"] = {
                            "message": "Confirmation is required before reconnecting Codex"
                        }
                        emit_operation(
                            "integration_operation",
                            "confirmation_required",
                            **self._integration_operation_fields("enable", summary),
                        )
                        self._send(409, _json_bytes(summary))
                        return
                    try:
                        base_url, _ = _integration_target(
                            state,
                            self.server.server_address,
                        )
                        result = state.enable_integration(
                            base_url,
                            confirm_reload=True,
                        )
                        summary = _integration_summary(state, result)
                        emit_operation(
                            "integration_operation",
                            "success" if result.ok else "conflict",
                            **self._integration_operation_fields("enable", summary),
                        )
                        self._send(
                            200 if result.ok else 409,
                            _json_bytes(summary),
                        )
                    except EmptyEmpCatalog:
                        summary = _integration_summary(state)
                        summary["error"] = {
                            "code": "empty_emp_catalog",
                            "message": "Add or show at least one EMP model before enabling Codex",
                        }
                        emit_operation(
                            "integration_operation",
                            "empty_catalog",
                            **self._integration_operation_fields("enable", summary),
                        )
                        self._send(409, _json_bytes(summary))
                    except (IntegrationError, OSError) as exc:
                        emit_operation(
                            "integration_operation",
                            "integration_error",
                            **self._integration_operation_fields("enable", None),
                        )
                        self._error(409, _integration_error_message(exc))
                    return
                if path == "/api/integration/restore":
                    if body.get("confirm_reload") is not True:
                        summary = _integration_summary(state)
                        summary["error"] = {
                            "message": "Confirmation is required before reconnecting Codex"
                        }
                        emit_operation(
                            "integration_operation",
                            "confirmation_required",
                            **self._integration_operation_fields("restore", summary),
                        )
                        self._send(409, _json_bytes(summary))
                        return
                    try:
                        result = state.restore_integration(confirm_reload=True)
                        summary = _integration_summary(state, result)
                        emit_operation(
                            "integration_operation",
                            "success" if result.ok else "conflict",
                            **self._integration_operation_fields("restore", summary),
                        )
                        self._send(
                            200 if result.ok else 409,
                            _json_bytes(summary),
                        )
                    except (IntegrationError, OSError) as exc:
                        emit_operation(
                            "integration_operation",
                            "integration_error",
                            **self._integration_operation_fields("restore", None),
                        )
                        self._error(409, _integration_error_message(exc))
                    return
                if path == "/api/integration/reload":
                    confirm_reload = body.get("confirm_reload") is True
                    result = state.sync_integration_runtime(confirm_reload)
                    summary = _integration_summary(state)
                    successful = result.state in {
                        EMP_LOADED,
                        NATIVE_LOADED,
                        STOPPED_WAITING_FOR_START,
                    }
                    if not successful:
                        summary["error"] = {"message": result.detail}
                    emit_operation(
                        "integration_operation",
                        "success" if successful else "runtime_failure",
                        **self._integration_operation_fields("reload", summary),
                    )
                    self._send(
                        200 if successful else 409,
                        _json_bytes(summary),
                    )
                    return
                if path == "/api/config":
                    updated = state.update(body)
                    self._send(200, _json_bytes(_management_config(updated)))
                    return
                if path == "/api/providers/discover":
                    provider_id = body.get("provider")
                    if not isinstance(provider_id, str) or not provider_id:
                        raise ConfigError("provider is required")
                    selected = body.get("selected") if "selected" in body else None
                    result = state.discover_provider_models(provider_id, selected)
                    discovered_models = result.get("models")
                    emit_operation(
                        "provider_discovery",
                        "success",
                        provider_id=_safe_diagnostic_text(
                            provider_id, _DIAGNOSTIC_ID
                        ),
                        available=self._management_count(result.get("available")),
                        selected=self._management_count(
                            len(selected) if isinstance(selected, list) else 0
                        ),
                        added=self._management_count(result.get("added")),
                        hidden=self._management_count(result.get("hidden")),
                        model_count=self._management_count(
                            result.get("model_count")
                            if "model_count" in result
                            else len(discovered_models)
                            if isinstance(discovered_models, list)
                            else 0
                        ),
                    )
                    self._send(200, _json_bytes(result))
                    return
                if path == "/api/accounts/import":
                    auth_json = body.pop("auth_json", None)
                    account = state.import_account(body, auth_json)
                    emit_operation(
                        "account_operation",
                        "success",
                        operation="import",
                        account_ref=self._account_ref(account.get("id")),
                    )
                    self._send(200, _json_bytes({"account": public_accounts([account])[0]}))
                    return
                if path == "/api/migration/export":
                    bundle = state.export_migration(body.get("password"))
                    migration_snapshot = state.snapshot()
                    emit_operation(
                        "migration_operation",
                        "success",
                        operation="export",
                        accounts=self._management_count(
                            len(migration_snapshot.get("accounts", []))
                        ),
                        providers=self._management_count(
                            len(migration_snapshot.get("providers", []))
                        ),
                        models=self._management_count(
                            len(migration_snapshot.get("models", []))
                        ),
                    )
                    self._send(
                        200,
                        bundle,
                        "application/octet-stream",
                        {
                            "Cache-Control": "no-store",
                            "Content-Disposition": 'attachment; filename="easy-multi-provider-%s.emp"' % __version__,
                        },
                    )
                    return
                if path == "/api/migration/import":
                    encoded = body.get("bundle")
                    if not isinstance(encoded, str) or not encoded:
                        raise ConfigError("migration bundle is required")
                    try:
                        bundle = base64.b64decode(encoded.encode("ascii"), validate=True)
                    except (UnicodeEncodeError, ValueError) as exc:
                        raise ConfigError("migration bundle is not valid base64") from exc
                    summary = state.import_migration(bundle, body.get("password"))
                    emit_operation(
                        "migration_operation",
                        "success",
                        operation="import",
                        **self._migration_numeric_fields(summary),
                    )
                    self._send(200, _json_bytes({"status": "ok", **summary}))
                    return
                if path.startswith("/api/accounts/") and path.endswith("/quota"):
                    account_id = unquote(path[len("/api/accounts/") : -len("/quota")].rstrip("/"))
                    account = state.refresh_account(account_id)
                    emit_operation(
                        "account_operation",
                        "success",
                        operation="quota_refresh",
                        account_ref=self._account_ref(account_id),
                    )
                    self._send(200, _json_bytes({"account": public_accounts([account])[0]}))
                    return
                if path == "/api/catalog/refresh":
                    catalog_path = state.refresh_catalog()
                    visible_model_count = len(build_catalog(state.snapshot())["models"])
                    emit_operation(
                        "catalog_refresh",
                        "success",
                        visible_model_count=self._management_count(
                            visible_model_count
                        ),
                    )
                    self._send(
                        200,
                        _json_bytes(
                            {
                                "status": "ok",
                                "catalog_path": str(catalog_path.resolve()),
                                "model_count": visible_model_count,
                            }
                        ),
                    )
                    return
                if path == "/api/models/metadata":
                    provider_id = body.get("provider")
                    upstream_model = body.get("model")
                    if not isinstance(provider_id, str) or not isinstance(upstream_model, str):
                        raise ConfigError("provider and model are required")
                    provider = next(
                        (item for item in state.snapshot().get("providers", []) if item.get("id") == provider_id),
                        None,
                    )
                    if provider is None or not provider.get("enabled", True):
                        raise ConfigError("provider is missing or disabled: %s" % provider_id)
                    prefix = provider_id + "/"
                    if upstream_model.startswith(prefix):
                        upstream_model = upstream_model[len(prefix):]
                    self._send(200, _json_bytes(model_metadata(provider, upstream_model)))
                    return
                if path == "/v1/responses/compact":
                    metadata, result = state.route_compact(
                        body,
                        {key: value for key, value in self.headers.items()},
                        transport="http",
                    )
                    self._send(
                        metadata.get("status", 200),
                        result,
                        metadata.get("content_type", "application/json"),
                    )
                    return
                if path == "/v1/responses":
                    metadata, result = state.route(
                        body,
                        {key: value for key, value in self.headers.items()},
                        transport="sse" if body.get("stream") else "http",
                    )
                    if metadata["kind"] == "stream":
                        iterator = iter(result)
                        first_chunk = next(iterator, b"")
                        close_after_stream = b'"type": "response.failed"' in first_chunk
                        self.send_response(200)
                        self.send_header("Content-Type", metadata["content_type"])
                        self.send_header("Cache-Control", "no-cache")
                        self.send_header("Connection", "close" if close_after_stream else "keep-alive")
                        self.end_headers()
                        try:
                            if first_chunk:
                                if b'"type": "response.failed"' in first_chunk:
                                    close_after_stream = True
                                self.wfile.write(first_chunk)
                                self.wfile.flush()
                            for chunk in iterator:
                                if b'"type": "response.failed"' in chunk:
                                    close_after_stream = True
                                self.wfile.write(chunk)
                                self.wfile.flush()
                        finally:
                            close_iterator = getattr(iterator, "close", None)
                            if callable(close_iterator):
                                close_iterator()
                        if close_after_stream:
                            self.close_connection = True
                    elif metadata["kind"] == "raw_stream":
                        self.send_response(metadata.get("status", 200))
                        self.send_header("Content-Type", metadata["content_type"])
                        self.send_header("Cache-Control", "no-cache")
                        self.send_header("Connection", "keep-alive")
                        self.end_headers()
                        try:
                            while True:
                                chunk = result.read(8192)
                                if not chunk:
                                    break
                                self.wfile.write(chunk)
                                self.wfile.flush()
                        finally:
                            result.close()
                    else:
                        self._send(
                            metadata.get("status", 200),
                            result,
                            metadata.get("content_type", "application/json"),
                        )
                    return
                self._error(404, "not found")
            except ContextHandoffRequiredError as exc:
                emit_operation_failure(exc)
                if body.get("stream"):
                    frame = _handoff_sse_frame(exc)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Cache-Control", "no-cache")
                    self.send_header("Connection", "close")
                    self.end_headers()
                    try:
                        self.wfile.write(frame)
                        self.wfile.flush()
                    except Exception:
                        pass
                    self.close_connection = True
                else:
                    self._send(409, _json_bytes(_handoff_error_body(exc)))
            except (ConfigError, RouterError, QuotaError, ValueError) as exc:
                emit_operation_failure(exc)
                status = exc.status if isinstance(exc, RouterError) else 400
                if isinstance(exc, QuotaError):
                    status = 503
                self._error(status, str(exc))
            except Exception as exc:  # Keep server alive and avoid leaking request details.
                emit_operation_failure(exc)
                self._record_unexpected_exception(exc)
                self._error(500, "internal server error: %s" % exc)

        def do_DELETE(self) -> None:
            path = urlparse(self.path).path
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            operation_started = time.monotonic()
            operation_logged = False
            account_id = ""

            def emit_account_operation(result_class: str) -> None:
                nonlocal operation_logged
                if operation_logged or not path.startswith("/api/accounts/"):
                    return
                operation_logged = True
                self._record_management_event(
                    "account_operation",
                    operation_started,
                    result_class,
                    operation="delete",
                    account_ref=self._account_ref(account_id),
                )
            try:
                if path.startswith("/api/accounts/"):
                    account_id = unquote(path[len("/api/accounts/") :].rstrip("/"))
                    state.delete_account(account_id)
                    emit_account_operation("success")
                    self._send(200, _json_bytes({"status": "ok"}))
                    return
                self._error(404, "not found")
            except (ConfigError, QuotaError, ValueError) as exc:
                emit_account_operation(self._management_failure_class(exc))
                self._error(400, str(exc))
            except Exception as exc:  # Keep server alive and avoid leaking request details.
                emit_account_operation(self._management_failure_class(exc))
                self._record_unexpected_exception(exc)
                self._error(500, "internal server error: %s" % exc)

    return Handler


class BoundedThreadingHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    request_limit = 32

    def __init__(self, server_address, handler_cls):
        super().__init__(server_address, handler_cls)
        self._request_slots = threading.BoundedSemaphore(self.request_limit)

    def process_request(self, request, client_address):
        if not self._request_slots.acquire(blocking=False):
            request.close()
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._request_slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._request_slots.release()


class _GracefulShutdown(Exception):
    """Internal control flow for a handled termination signal."""


def _raise_graceful_shutdown(signum: int, frame: Any) -> None:
    raise _GracefulShutdown()


def _install_sigterm_handler() -> Optional[Tuple[Any, Any]]:
    """Install SIGTERM handling only where Python permits signal handlers."""

    if threading.current_thread() is not threading.main_thread():
        return None
    sigterm = getattr(signal, "SIGTERM", None)
    if sigterm is None:
        return None
    try:
        previous = signal.getsignal(sigterm)
        signal.signal(sigterm, _raise_graceful_shutdown)
    except (OSError, ValueError):
        return None
    return sigterm, previous


def _restore_sigterm_handler(previous: Optional[Tuple[Any, Any]]) -> None:
    if previous is None:
        return
    try:
        signal.signal(previous[0], previous[1])
    except (OSError, ValueError):
        pass


def startup_reconcile(state: AppState, server: BoundedThreadingHTTPServer) -> IntegrationResult:
    """Reconcile stale integration only after the listener has been bound."""

    if server.fileno() < 0:
        raise ServiceNotReady("EMP listener is not bound")
    state.mark_service_ready()
    base_url, catalog_path = _integration_target(state, server.server_address)
    conflict = _startup_target_conflict(state, base_url, catalog_path)
    if conflict is not None:
        return conflict
    result = state.reconcile_startup(state.service_ready)
    if result.action == "re_adopted" and result.state == "active":
        state.refresh_catalog()
    return result


def _journal_event(journal: Any, level: str, event_name: str, **fields: Any) -> None:
    try:
        journal.event(level, event_name, **fields)
    except Exception:
        pass


def _journal_exception(
    journal: Any,
    level: str,
    event_name: str,
    stage: str,
    exception: BaseException,
) -> None:
    try:
        journal.exception_event(level, event_name, stage, exception)
    except Exception:
        pass


def _lifecycle_result_fields(result: Any) -> Dict[str, Any]:
    try:
        conflicts = getattr(result, "conflicts", ())
        return {
            "action": _safe_diagnostic_text(
                getattr(result, "action", ""), _DIAGNOSTIC_ID
            ),
            "state": _safe_diagnostic_text(
                getattr(result, "state", ""), _DIAGNOSTIC_ID
            ),
            "relation": _safe_diagnostic_text(
                getattr(result, "relation", ""), _DIAGNOSTIC_ID
            ),
            "conflicts": _safe_diagnostic_sequence(
                list(conflicts) if isinstance(conflicts, (list, tuple)) else []
            ),
            "result_class": (
                "success" if bool(getattr(result, "ok", False)) else "conflict"
            ),
        }
    except Exception:
        return {
            "action": "",
            "state": "",
            "relation": "",
            "conflicts": [],
            "result_class": "unknown",
        }


def serve(path: Optional[Path] = None, host: Optional[str] = None, port: Optional[int] = None) -> None:
    effective_config_path = Path(path or config_path())
    try:
        journal = create_journal(effective_config_path.parent)
    except Exception:
        journal = NullJournal()

    state = None
    server = None
    previous_sigterm = None
    shutdown_reason = "startup_failure"
    stage = "host_validation"
    try:
        try:
            if host and host != "127.0.0.1":
                raise ConfigError("host must be 127.0.0.1 for local-only management")

            stage = "ensure_master_key"
            ensure_master_key()
            stage = "proxy_config"
            proxy_source = configure_proxy_environment()
            stage = "integration_paths"
            paths = resolve_integration_paths()
            stage = "integration_manager"
            manager = IntegrationManager(
                paths.config_path,
                paths.lease_path,
                lock_path=paths.lock_path,
            )
            stage = "app_state"
            state = AppState(
                effective_config_path,
                integration_manager=manager,
                catalog_path=generated_catalog_path(paths.codex_home),
                journal=journal,
            )
            if host:
                state.config["host"] = host
            if port is not None:
                state.config["port"] = port
            config = state.snapshot()
            bind_host = host or config["host"]
            bind_port = port if port is not None else config["port"]
            stage = "listener_bind"
            server = BoundedThreadingHTTPServer((bind_host, bind_port), make_handler(state))
            base_url = _bound_base_url(server.server_address).rsplit("/v1", 1)[0]
            previous_sigterm = _install_sigterm_handler()

            _journal_event(
                journal,
                "info",
                "process_start",
                emp_version=__version__,
                python_version=platform.python_version(),
                platform_family=(platform.system() or "unknown").lower(),
                pid=os.getpid(),
                host=bind_host,
                port=bind_port,
                account_count=len(config.get("accounts", [])),
                provider_count=len(config.get("providers", [])),
                model_count=len(config.get("models", [])),
            )
            safe_proxy_source = (
                proxy_source
                if proxy_source in ("environment", "system", "direct")
                else "unknown"
            )
            _journal_event(
                journal,
                "info",
                "proxy_selected",
                source=safe_proxy_source,
            )

            stage = "startup_reconcile"
            reconcile_started = time.monotonic()
            reconcile_result = startup_reconcile(state, server)
            reconcile_fields = _lifecycle_result_fields(reconcile_result)
            reconcile_fields["duration_ms"] = max(
                0, int((time.monotonic() - reconcile_started) * 1000)
            )
            _journal_event(
                journal,
                "info",
                "startup_reconcile",
                **reconcile_fields,
            )
            listening_host, listening_port = server.server_address[:2]
            _journal_event(
                journal,
                "info",
                "service_listening",
                host=str(listening_host),
                port=int(listening_port),
            )

            print("EasyMultiProvider listening on %s" % base_url, flush=True)
            print("Network proxy: %s" % proxy_source, flush=True)
            print(
                "Open in browser: %s/?bootstrap=%s" % (base_url, state.bootstrap_token),
                flush=True,
            )
            try:
                if journal.enabled and journal.current_path is not None:
                    print("Diagnostic log: %s" % journal.current_path, flush=True)
            except Exception:
                pass
        except Exception as exc:
            _journal_exception(journal, "error", "startup_failure", stage, exc)
            raise

        try:
            shutdown_reason = "server_stopped"
            server.serve_forever()
        except KeyboardInterrupt:
            shutdown_reason = "keyboard_interrupt"
        except _GracefulShutdown:
            shutdown_reason = "sigterm"
        except Exception as exc:
            shutdown_reason = "internal_error"
            _journal_exception(journal, "error", "internal_error", "serve_forever", exc)
            raise
    finally:
        active_exception = sys.exc_info()[1] is not None
        _journal_event(
            journal,
            "info",
            "shutdown_start",
            reason=shutdown_reason,
        )
        restore_result = None
        restore_error = None
        close_error = None
        try:
            _restore_sigterm_handler(previous_sigterm)
        finally:
            try:
                if state is not None:
                    restore_result = state.shutdown_restore()
            except Exception as exc:
                restore_error = exc
            finally:
                try:
                    if server is not None:
                        server.server_close()
                except Exception as exc:
                    close_error = exc

        if restore_result is not None:
            restore_fields = _lifecycle_result_fields(restore_result)
        elif restore_error is not None:
            restore_fields = {
                "action": "",
                "state": "",
                "relation": "",
                "result_class": restore_error.__class__.__name__,
            }
        else:
            restore_fields = {
                "action": "",
                "state": "",
                "relation": "",
                "result_class": "not_started",
            }
        restore_fields.pop("conflicts", None)
        restore_fields["reason"] = shutdown_reason
        _journal_event(
            journal,
            "info",
            "shutdown_complete",
            **restore_fields,
        )
        try:
            journal.close()
        except Exception:
            pass

        if not active_exception:
            if close_error is not None:
                raise close_error
            if restore_error is not None:
                raise restore_error
