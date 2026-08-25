"""Translate Codex Responses requests to configured upstream providers."""

from __future__ import annotations

import copy
from dataclasses import dataclass
import json
import re
import time
import uuid
from typing import Any, Callable, Dict, Iterable, Iterator, Mapping, Optional, Tuple
from urllib.error import HTTPError, URLError
from urllib.parse import urlparse
from urllib.request import HTTPRedirectHandler, Request, build_opener

from . import __version__
from .accounts import AccountError, auth_headers, native_auth_headers
from .capabilities import (
    deployment_identity,
    endpoint_fingerprint,
    normalize_supported_protocols,
    observed_at_now,
)
from .catalog import load_native_catalog
from .config import api_key
from .context_guard import (
    ContextGuardBlocked,
    is_explicit_context_error,
    mark_explicit_failure,
)
from .native_identity import (
    NativeIdentityError,
    NativeRouteIdentity,
    derive_native_route_identity,
)
from .dialects import (
    CODEX_NATIVE,
    PORTABLE_RESPONSES,
    ProjectionError,
    classify_dialect,
    custom_tool_arguments,
    custom_tool_ids,
    custom_tool_input,
    custom_tool_names,
    portable_tool_definitions,
    project_request,
    project_response,
    project_stream_event,
    request_shape,
)
from .quota import QuotaError, refresh_account_quota
from .native_websocket import NativeWebSocketTarget
from .model_discovery import (
    DiscoveryIO,
    advertised_reasoning as _discovery_advertised_reasoning,
    advertised_reasoning_summaries as _discovery_advertised_reasoning_summaries,
    anthropic_discovery_headers as _owned_anthropic_discovery_headers,
    discover_models as _owned_discover_models,
    discovery_headers as _owned_discovery_headers,
    model_metadata as _owned_model_metadata,
)
from .protocol_projection import (
    _anthropic_content,
    _anthropic_incomplete_reason,
    _anthropic_messages,
    _anthropic_tool_arguments,
    _anthropic_tool_choice,
    _anthropic_tool_input,
    _anthropic_tools,
    _chat_content,
    _chat_incomplete_reason,
    _chat_tool_choice,
    _content_text,
    _decode_compaction,
    _encode_compaction,
    _message_item,
    _messages,
    _normalize_compaction_input,
    _response_from_anthropic,
    _response_from_chat,
    _tools,
    _upstream_error_text,
    _validate_textual_protocol,
    responses_terminal_observation,
    responses_to_anthropic,
    responses_to_chat,
    validate_responses_body,
)
from .router_errors import (
    ContextLengthError,
    ExternalCompactionError,
    ExternalProtocolError,
    HistoryReconstructionError,
    RouterError,
    StreamBoundaryError,
    UpstreamHTTPError,
)
from .stream_adapters import (
    StreamAdapterIO,
    _notify_terminal,
    _reliable_responses_stream,
    _response_failure_frame,
    _response_json_stream,
    _route_error_class,
    _sse_data,
    _sse_frame,
    _stream_event_activity,
    _stream_exception,
    _stream_terminal,
    _terminal_exception,
    _validated_responses_stream as _owned_validated_responses_stream,
    stream_anthropic_completion as _owned_stream_anthropic_completion,
    stream_chat_completion as _owned_stream_chat_completion,
)
from .transport import TransportError, sse_json_events, zstd_encode


HistoryPreparationCallback = Callable[
    [
        Mapping[str, Any],
        Mapping[str, Any],
        Mapping[str, Any],
        str,
        Mapping[str, Any],
        Mapping[str, str],
    ],
    Dict[str, Any],
]


@dataclass(frozen=True)
class NativeWebSocketPlan:
    """Transient native WebSocket route data; never persist this object."""

    target: NativeWebSocketTarget
    provider: Dict[str, Any]
    model: Dict[str, Any]
    requested_slug: str
    payload: Dict[str, Any]
    identity: NativeRouteIdentity
    context_observation: Dict[str, Any]


class _NoRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        raise URLError("upstream redirects are disabled")


# Credential-bearing requests must never replay headers to a redirected URL.
def urlopen(request: Request, timeout: float):
    """Build after startup so automatically imported system proxies are used."""
    return build_opener(_NoRedirectHandler()).open(request, timeout=timeout)


MAX_UPSTREAM_BODY_BYTES = 16 * 1024 * 1024
MAX_UPSTREAM_ERROR_BYTES = 4096
MAX_UPSTREAM_ERROR_TEXT_CHARS = 512
MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS = 256 * 1024
MAX_DISCOVERY_BODY_BYTES = 4 * 1024 * 1024
MAX_DISCOVERY_TOTAL_BYTES = 8 * 1024 * 1024
MAX_DISCOVERY_SECONDS = 60
MAX_DISCOVERY_FIELD_BYTES = 4096
MAX_DISCOVERY_TOKEN_BYTES = 4096
MAX_SSE_FRAME_BYTES = 1024 * 1024
MAX_STREAM_TEXT_BYTES = 16 * 1024 * 1024
MAX_DISCOVERED_MODELS = 1000
MAX_UPSTREAM_SECONDS = 180
UPSTREAM_SOCKET_TIMEOUT = 30
_COMPACTION_PROMPT = """You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another language model that will resume the task.

Include current progress, key decisions, constraints, user preferences, remaining steps, and critical data or references. Be concise, structured, and focused on seamless continuation."""
_COMPACTION_SUMMARY_PREFIX = (
    "Another language model started this task and produced a continuation summary. "
    "Use it to continue without repeating completed work:"
)
_COMPACTION_PREFIX = "emp1:"
_PROTOCOL_REJECTION_STATUSES = frozenset({404, 405, 415, 501})
_AUTO_PROTOCOL_REJECTION_STATUSES = _PROTOCOL_REJECTION_STATUSES
_CONCRETE_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages"}
)
_REASONING_LEVEL_ID = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")
def _http_error_detail(exc: HTTPError, raw: Optional[bytes] = None) -> Tuple[str, str]:
    """Return a bounded diagnostic without echoing an upstream HTML page."""
    headers = getattr(exc, "headers", {}) or {}
    content_type = str(headers.get("Content-Type", "")).split(";", 1)[0].strip().lower()
    if raw is None:
        raw = exc.read(MAX_UPSTREAM_ERROR_BYTES)
    decoded = raw.decode("utf-8", "replace")
    if content_type in ("text/html", "application/xhtml+xml") or re.match(
        r"\s*(?:<!doctype\s+html|<html\b)", decoded, re.I
    ):
        return (
            content_type or "text/html",
            "HTML error page omitted; the upstream gateway or WAF may have rejected the request",
        )

    detail = decoded
    if "json" in content_type or decoded.lstrip().startswith(("{", "[")):
        try:
            value = json.loads(decoded)
        except ValueError:
            pass
        else:
            if isinstance(value, dict):
                detail = _upstream_error_text(value.get("error") or value.get("message") or value)
            else:
                detail = str(value)
    detail = re.sub(r"\s+", " ", detail).strip()
    if not detail:
        detail = str(getattr(exc, "reason", "") or "no response detail")
    if len(detail) > MAX_UPSTREAM_ERROR_TEXT_CHARS:
        detail = detail[: MAX_UPSTREAM_ERROR_TEXT_CHARS - 3].rstrip() + "..."
    return content_type or "unknown content type", detail


def _http_error_message(prefix: str, exc: HTTPError) -> str:
    content_type, detail = _http_error_detail(exc)
    if content_type in {"text/html", "application/xhtml+xml"}:
        return "%s %d (%s): %s" % (prefix, exc.code, content_type, detail)
    return "%s %d (%s)" % (prefix, exc.code, content_type)


def _upstream_failure_reason(status: int, detail: str) -> str:
    text = re.sub(r"[^a-z0-9]+", " ", str(detail or "").lower()).strip()
    if status in (401, 403):
        return "auth_rejected"
    if status == 413 or any(
        phrase in text
        for phrase in ("request too large", "payload too large", "input too large")
    ):
        return "request_too_large"
    if any(
        phrase in text
        for phrase in ("context length", "context window", "maximum context")
    ):
        return "context_length_exceeded"
    if status == 429:
        if any(word in text for word in ("quota", "credit", "balance", "insufficient")):
            return "quota_exhausted"
        if any(word in text for word in ("capacity", "overloaded", "saturated")):
            return "upstream_capacity"
        return "rate_limited"
    if status in (500, 502, 503, 504):
        return "upstream_unavailable"
    return "upstream_rejected"


def _set_response_timeout(response: Any, timeout: float) -> None:
    # urllib wraps a TLS socket differently across Python/proxy paths. A
    # typical chunked response is addinfourl.fp -> HTTPResponse.fp ->
    # BufferedReader.raw -> SocketIO._sock. Walk only these known wrapper
    # attributes, with a strict bound, so the global request deadline reaches
    # the actual socket instead of silently leaving a blocking read unbounded.
    pending = [response]
    seen = set()
    for _ in range(12):
        if not pending:
            return
        candidate = pending.pop(0)
        if candidate is None or id(candidate) in seen:
            continue
        seen.add(id(candidate))
        if hasattr(candidate, "settimeout"):
            candidate.settimeout(max(0.1, timeout))
            return
        for attribute in ("_sock", "sock", "fp", "raw"):
            nested = getattr(candidate, attribute, None)
            if nested is not None and id(nested) not in seen:
                pending.append(nested)


def _read_limited(
    response: Any, limit: int, purpose: str, deadline: float = None
) -> bytes:
    response_headers = getattr(response, "headers", {})
    content_length = response_headers.get("Content-Length") if response_headers else None
    if content_length:
        try:
            if int(content_length) < 0 or int(content_length) > limit:
                raise RouterError("upstream %s is too large" % purpose, 502)
        except ValueError:
            raise RouterError("upstream %s has invalid Content-Length" % purpose, 502)
    chunks = []
    total = 0
    while True:
        if deadline is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                response.close()
                raise RouterError("upstream %s timed out" % purpose, 504)
            _set_response_timeout(response, min(UPSTREAM_SOCKET_TIMEOUT, remaining))
        try:
            chunk = response.read(64 * 1024)
        except TypeError:
            # Small test doubles may expose read() without a size argument.
            chunk = response.read()
            if len(chunk) > limit:
                raise RouterError("upstream %s is too large" % purpose, 502)
            return chunk
        except OSError as exc:
            if deadline is not None and time.monotonic() >= deadline:
                response.close()
                raise RouterError("upstream %s timed out" % purpose, 504) from exc
            raise
        if not chunk:
            break
        total += len(chunk)
        if total > limit:
            raise RouterError("upstream %s is too large" % purpose, 502)
        chunks.append(chunk)
    return b"".join(chunks)


class _DeadlineResponse:
    def __init__(self, response: Any, deadline: float):
        self._response = response
        self._deadline = deadline
        self._unsized_read = False

    @property
    def status(self):
        return self._response.status

    @property
    def headers(self):
        return self._response.headers

    def _check_deadline(self) -> None:
        if time.monotonic() > self._deadline:
            self.close()
            raise RouterError("upstream request timed out", 504)

    def read(self, size: int = -1) -> bytes:
        self._check_deadline()
        if self._unsized_read:
            return b""
        _set_response_timeout(
            self._response,
            self._deadline - time.monotonic(),
        )
        try:
            return self._response.read(size)
        except TypeError:
            self._unsized_read = True
            return self._response.read()
        except TimeoutError as exc:
            self.close()
            raise RouterError("upstream request timed out", 504) from exc
        except OSError as exc:
            if time.monotonic() >= self._deadline:
                self.close()
                raise RouterError("upstream request timed out", 504) from exc
            raise

    def __iter__(self):
        return self

    def __next__(self):
        self._check_deadline()
        _set_response_timeout(
            self._response,
            self._deadline - time.monotonic(),
        )
        try:
            return next(self._response)
        except TimeoutError as exc:
            self.close()
            raise RouterError("upstream request timed out", 504) from exc
        except OSError as exc:
            if time.monotonic() >= self._deadline:
                self.close()
                raise RouterError("upstream request timed out", 504) from exc
            raise

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def close(self) -> None:
        close = getattr(self._response, "close", None)
        if close:
            close()


class _LimitedResponse:
    def __init__(self, response: Any, limit: int):
        self._response = response
        self._limit = limit
        self._read = 0

    @property
    def status(self):
        return self._response.status

    @property
    def headers(self):
        return self._response.headers

    def read(self, size: int = -1) -> bytes:
        if self._read >= self._limit:
            return b""
        remaining = self._limit - self._read
        requested = remaining if size is None or size < 0 else min(size, remaining)
        try:
            chunk = self._response.read(requested)
        except TypeError:
            chunk = self._response.read()
        if not chunk:
            return b""
        self._read += len(chunk)
        if self._read > self._limit:
            raise RouterError("upstream stream is too large", 502)
        return chunk

    def __iter__(self):
        for chunk in self._response:
            if not chunk:
                return
            self._read += len(chunk)
            if self._read > self._limit:
                raise RouterError("upstream stream is too large", 502)
            yield chunk

    def close(self) -> None:
        self._response.close()


def _bounded_stream_response(response: Any) -> _LimitedResponse:
    headers = getattr(response, "headers", {})
    content_length = headers.get("Content-Length") if headers else None
    if content_length:
        try:
            if int(content_length) < 0 or int(content_length) > MAX_UPSTREAM_BODY_BYTES:
                response.close()
                raise RouterError("upstream stream is too large", 502)
        except ValueError:
            response.close()
            raise RouterError("upstream stream has invalid Content-Length", 502)
    return _LimitedResponse(response, MAX_UPSTREAM_BODY_BYTES)


_advertised_reasoning = _discovery_advertised_reasoning
_advertised_reasoning_summaries = _discovery_advertised_reasoning_summaries

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
        return provider, {"id": model_id, "upstream_id": model_id}
    return None


def find_route(config: Dict[str, Any], model_id: str) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    for model in config.get("models", []):
        if model.get("enabled", True) and model.get("id") == model_id:
            providers = {item["id"]: item for item in config.get("providers", [])}
            provider = providers.get(model.get("provider"))
            if provider and provider.get("enabled", True):
                return provider, model
            raise RouterError("provider for model is missing or disabled: %s" % model_id, 503)

    for account in config.get("accounts", []):
        prefix = str(account.get("prefix", "")) + "/"
        if account.get("enabled", True) and prefix != "/" and model_id.startswith(prefix):
            upstream_id = model_id[len(prefix):]
            if not upstream_id:
                break
            return {
                "id": account["id"],
                "name": account.get("name", account["id"]),
                "base_url": config.get("codex_base_url", "https://chatgpt.com/backend-api/codex"),
                "protocol": "responses",
                "auth_mode": "account",
                "account": account,
            }, {"id": model_id, "upstream_id": upstream_id}

    forward = [
        item
        for item in config.get("providers", [])
        if item.get("enabled", True) and item.get("auth_mode") == "forward"
    ]
    if len(forward) == 1:
        return forward[0], {"id": model_id, "upstream_id": model_id}
    if not forward:
        implicit = _implicit_native_route(config, model_id)
        if implicit is not None:
            return implicit
    raise RouterError("unknown model: %s" % model_id, 404)


def _upstream_model(provider: Dict[str, Any], model: Dict[str, Any], requested: str) -> str:
    explicit = model.get("upstream_id", "")
    if explicit:
        prefix = str(provider.get("id", "")) + "/"
        return explicit[len(prefix):] if prefix != "/" and explicit.startswith(prefix) else explicit
    prefix = provider.get("id", "") + "/"
    return requested[len(prefix):] if requested.startswith(prefix) else requested


def _endpoint(provider: Dict[str, Any], operation: str = "") -> str:
    provider = resolve_provider_protocol(provider)
    base = provider["base_url"].rstrip("/")
    if operation == "alpha_search":
        if provider.get("auth_mode") != "forward":
            raise RouterError("native search passthrough requires caller authentication")
        return base if base.endswith("/alpha/search") else base + "/alpha/search"
    if operation == "responses_compact":
        if provider["protocol"] != "responses":
            raise RouterError("remote compact passthrough requires the Responses protocol")
        if base.endswith("/responses/compact"):
            return base
        if base.endswith("/responses"):
            return base + "/compact"
        return base + "/responses/compact"
    suffixes = {
        "responses": "/responses",
        "chat_completions": "/chat/completions",
        "anthropic_messages": "/messages",
    }
    suffix = suffixes[provider["protocol"]]
    return base if base.endswith(suffix) else base + suffix


def forward_native_search(
    config: Mapping[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    """Preserve Codex-owned web search while EMP owns openai_base_url."""

    provider = {
        "id": "codex-native-search",
        "name": "Native Codex Search",
        "base_url": config.get(
            "codex_base_url", "https://chatgpt.com/backend-api/codex"
        ),
        "protocol": "responses",
        "auth_mode": "forward",
    }
    with _request(
        provider,
        body,
        incoming,
        False,
        "alpha_search",
    ) as response:
        content_type = response.headers.get("Content-Type", "application/json")
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "native search response")
        return response.status, content_type, raw


def _discovery_headers(
    provider: Dict[str, Any], native_gemini: bool
) -> Dict[str, str]:
    return _owned_discovery_headers(provider, native_gemini, __version__)


def _anthropic_discovery_headers(provider: Dict[str, Any]) -> Dict[str, str]:
    return _owned_anthropic_discovery_headers(provider, __version__)


def _discovery_io() -> DiscoveryIO:
    return DiscoveryIO(
        open_url=urlopen,
        read_limited=_read_limited,
        http_error_message=_http_error_message,
        discovery_headers=_discovery_headers,
        anthropic_headers=_anthropic_discovery_headers,
    )


def model_metadata(provider: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    return _owned_model_metadata(_discovery_io(), provider, upstream_model)


def discover_models(provider: Dict[str, Any]) -> list:
    return _owned_discover_models(_discovery_io(), provider)


def resolve_provider_protocol(
    provider: Dict[str, Any], preferred: str = "chat_completions"
) -> Dict[str, Any]:
    """Return an explicit protocol candidate without mutating saved config."""
    if provider.get("protocol") != "auto":
        return provider
    resolved = dict(provider)
    resolved["protocol"] = (
        "anthropic_messages"
        if provider.get("auth_mode") == "anthropic_api_key"
        else preferred
    )
    return resolved


def _headers(
    provider: Dict[str, Any],
    incoming: Dict[str, str],
    stream: bool,
    resolved_native_auth: Optional[Mapping[str, str]] = None,
) -> Dict[str, str]:
    headers = {
        "Content-Type": "application/json",
        "Accept": "text/event-stream" if stream else "application/json",
        "User-Agent": "EasyMultiProvider/%s" % __version__,
    }
    if provider.get("auth_mode") == "api_key":
        key = api_key(provider)
        if not key:
            raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
        headers["Authorization"] = "Bearer " + key
    elif provider.get("auth_mode") == "anthropic_api_key":
        key = api_key(provider)
        if not key:
            raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
        headers["x-api-key"] = key
        headers["anthropic-version"] = provider.get("anthropic_version", "2023-06-01")
    elif provider.get("auth_mode") == "account":
        try:
            headers.update(auth_headers(provider["account"]))
        except AccountError as exc:
            raise RouterError(str(exc), 503)
    elif provider.get("implicit_native") is True:
        try:
            selected = (
                resolved_native_auth
                if resolved_native_auth is not None
                else native_auth_headers(provider.get("_native_auth_path"))
            )
            headers.update(selected)
        except AccountError:
            # Preserve the existing pass-through compatibility path when the
            # active Codex home has no readable ChatGPT login.
            lower = {key.lower(): value for key, value in incoming.items()}
            if lower.get("authorization"):
                headers["Authorization"] = lower["authorization"]
            if lower.get("chatgpt-account-id"):
                headers["chatgpt-account-id"] = lower["chatgpt-account-id"]
            if "Authorization" not in headers:
                raise RouterError(
                    "native Codex login credentials are unavailable", 401
                )
    else:
        # Only forward the headers needed by a local Codex-to-Codex pass-through.
        allowed = {
            "authorization": "Authorization",
            "chatgpt-account-id": "chatgpt-account-id",
            "openai-beta": "OpenAI-Beta",
            "originator": "originator",
        }
        lower = {key.lower(): value for key, value in incoming.items()}
        for source, target in allowed.items():
            if lower.get(source):
                headers[target] = lower[source]
        if "Authorization" not in headers:
            raise RouterError("forward provider requires an incoming Authorization header", 401)
    if provider.get("auth_mode") in {"account", "forward"}:
        lower = {key.lower(): value for key, value in incoming.items()}
        native_headers = {
            "openai-beta": "OpenAI-Beta",
            "originator": "originator",
            "session-id": "session-id",
            "thread-id": "thread-id",
            "x-client-request-id": "x-client-request-id",
            "x-codex-beta-features": "x-codex-beta-features",
            "x-codex-installation-id": "x-codex-installation-id",
            "x-codex-parent-thread-id": "x-codex-parent-thread-id",
            "x-codex-routing-hint": "x-codex-routing-hint",
            "x-codex-turn-state": "x-codex-turn-state",
            "x-codex-turn-metadata": "x-codex-turn-metadata",
            "x-codex-window-id": "x-codex-window-id",
            "x-oai-attestation": "x-oai-attestation",
            "x-openai-memgen-request": "x-openai-memgen-request",
            "x-openai-internal-codex-responses-lite": (
                "x-openai-internal-codex-responses-lite"
            ),
            "x-openai-subagent": "x-openai-subagent",
            "x-responsesapi-include-timing-metrics": (
                "x-responsesapi-include-timing-metrics"
            ),
        }
        for source, target in native_headers.items():
            value = lower.get(source)
            if isinstance(value, str) and value:
                headers[target] = value
    return headers


def _encoded_request_body(
    provider: Mapping[str, Any], payload: Mapping[str, Any]
) -> Tuple[bytes, Dict[str, Any]]:
    serialized = json.dumps(
        payload, ensure_ascii=False, separators=(",", ":")
    ).encode("utf-8")
    encoding = "identity"
    encoded = serialized
    if classify_dialect(provider) == CODEX_NATIVE:
        try:
            encoded = zstd_encode(serialized)
            encoding = "zstd"
        except TransportError as exc:
            raise RouterError("native request compression failed", 502) from exc
    metadata: Dict[str, Any] = {
        "decoded_request_bytes": len(serialized),
        "upstream_request_bytes": len(encoded),
        "upstream_content_encoding": encoding,
    }
    if serialized:
        ratio = len(encoded) / len(serialized)
        if 0.0 <= ratio <= 64.0:
            metadata["compression_ratio"] = ratio
    return encoded, metadata


def _safe_transport_metadata(value: Any) -> Dict[str, Any]:
    """Copy only bounded numeric transport facts and the encoding enum."""

    if not isinstance(value, Mapping):
        return {}
    result: Dict[str, Any] = {}
    for key in ("decoded_request_bytes", "upstream_request_bytes"):
        item = value.get(key)
        if isinstance(item, int) and not isinstance(item, bool) and 0 <= item <= 64 * 1024 * 1024:
            result[key] = item
    encoding = value.get("upstream_content_encoding")
    if encoding in ("zstd", "identity"):
        result["upstream_content_encoding"] = encoding
    ratio = value.get("compression_ratio")
    if isinstance(ratio, (int, float)) and not isinstance(ratio, bool):
        ratio = float(ratio)
        if ratio == ratio and ratio not in (float("inf"), float("-inf")) and 0.0 <= ratio <= 64.0:
            result["compression_ratio"] = ratio
    if result.get("decoded_request_bytes") == 0:
        result.pop("compression_ratio", None)
    return result


def _request(
    provider: Dict[str, Any],
    payload: Dict[str, Any],
    incoming: Dict[str, str],
    stream: bool = False,
    operation: str = "",
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    allow_retries: bool = True,
):
    data, transport_metadata = _encoded_request_body(provider, payload)
    provider["_transport_metadata"] = transport_metadata
    deadline = time.monotonic() + MAX_UPSTREAM_SECONDS
    # Bridge callers disable retries; ordinary retries keep one route/auth identity.
    endpoint = None
    headers = None
    for attempt in range(2):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RouterError("upstream request timed out", 504)
        if context_check is not None:
            try:
                observation = context_check(payload, stream, operation)
            except ContextGuardBlocked as exc:
                raise ContextLengthError(exc.assessment.to_safe_dict(), preflight=True) from exc
            if isinstance(observation, Mapping):
                # This is a short-lived safe assessment attached to the resolved
                # provider copy.  It is never serialized or persisted here.
                provider["_context_observation"] = dict(observation)
        if endpoint is None:
            endpoint = _endpoint(provider, operation)
        if headers is None:
            headers = _headers(provider, incoming, stream)
            if transport_metadata["upstream_content_encoding"] == "zstd":
                headers["Content-Encoding"] = "zstd"
        request = Request(
            endpoint,
            data=data,
            headers=headers,
            method="POST",
        )
        try:
            response = urlopen(request, timeout=min(UPSTREAM_SOCKET_TIMEOUT, remaining))
        except TimeoutError as exc:
            if transport_metadata["upstream_content_encoding"] == "zstd":
                if attempt == 0 and allow_retries:
                    continue
                raise RouterError("upstream request timed out", 504) from exc
            raise
        except HTTPError as exc:
            raw = exc.read(MAX_UPSTREAM_ERROR_BYTES)
            if is_explicit_context_error(
                exc.code,
                (getattr(exc, "headers", {}) or {}).get("Content-Type", ""),
                raw,
            ):
                observation = mark_explicit_failure(
                    provider.get("_context_observation", {}),
                    provider.get("_context_observation", {}).get("input_estimate"),
                )
                raise ContextLengthError(observation) from exc
            if (
                allow_retries
                and attempt == 0
                and exc.code == 401
                and provider.get("auth_mode") == "account"
            ):
                try:
                    refresh_account_quota(provider["account"])
                except (OSError, QuotaError, ValueError):
                    pass
                else:
                    headers = None
                    continue
            if allow_retries and attempt == 0 and exc.code in (500, 502, 503, 504):
                time.sleep(0.5)
                continue
            content_type, detail = _http_error_detail(exc, raw)
            if (
                allow_retries
                and attempt == 0
                and exc.code == 400
                and "reasoning_effort" in payload
                and "reasoning_effort" in detail
            ):
                payload = dict(payload)
                payload.pop("reasoning_effort", None)
                data, transport_metadata = _encoded_request_body(provider, payload)
                provider["_transport_metadata"] = transport_metadata
                headers = None
                continue
            public_message = "upstream returned %d (%s)" % (
                exc.code,
                content_type,
            )
            if content_type in {"text/html", "application/xhtml+xml"}:
                public_message += ": " + detail
            raise UpstreamHTTPError(
                public_message,
                exc.code,
                _upstream_failure_reason(exc.code, detail),
            )
        except URLError as exc:
            if (
                isinstance(exc.reason, TimeoutError)
                and transport_metadata["upstream_content_encoding"] == "zstd"
            ):
                if attempt == 0 and allow_retries:
                    continue
                raise RouterError("upstream request timed out", 504) from exc
            if allow_retries and attempt == 0:
                time.sleep(0.5)
                continue
            raise RouterError("upstream connection failed: %s" % exc.reason, 502)
        return (
            _DeadlineResponse(response, deadline)
            if hasattr(response, "close")
            else response
        )
    raise RouterError("upstream request failed", 502)


def _raise_if_context_response(
    provider: Dict[str, Any], status: Any, content_type: Any, raw: bytes
) -> None:
    if not is_explicit_context_error(status, content_type, raw):
        return
    observation = mark_explicit_failure(
        provider.get("_context_observation", {}),
        provider.get("_context_observation", {}).get("input_estimate"),
    )
    raise ContextLengthError(observation)


def forward_responses(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    allow_retries: bool = True,
) -> Tuple[int, str, bytes]:
    payload = _responses_payload(provider, body, model)
    with _request(
        provider,
        payload,
        incoming,
        bool(body.get("stream")),
        context_check=context_check,
        allow_retries=allow_retries,
    ) as response:
        content_type = response.headers.get("Content-Type", "application/json")
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses 响应")
        _raise_if_context_response(provider, response.status, content_type, raw)
        if classify_dialect(provider) == PORTABLE_RESPONSES:
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                raise RouterError("upstream Responses response is not valid JSON", 502) from exc
            if not isinstance(value, dict):
                raise RouterError("upstream Responses response must be a JSON object", 502)
            validate_responses_body(value)
            try:
                projected = project_response(
                    provider,
                    value,
                    custom_names=custom_tool_names(body),
                    preserve_reasoning_summary=model.get(
                        "_emp_preserve_reasoning_summary"
                    )
                    is True,
                    preserve_reasoning_state=model.get(
                        "_emp_preserve_reasoning_state"
                    )
                    is True,
                )
            except ProjectionError as exc:
                if exc.failure_class == "external_compaction":
                    raise ExternalProtocolError(
                        "external upstream returned an unexpected compaction item"
                    ) from exc
                raise ExternalProtocolError(
                    "external upstream response projection failed"
                ) from exc
            validate_responses_body(projected)
            raw = json.dumps(
                projected,
                ensure_ascii=False,
                separators=(",", ":"),
            ).encode("utf-8")
        return response.status, content_type, raw


def forward_responses_stream(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    on_stream_event: Optional[Callable[[Mapping[str, Any]], None]] = None,
):
    payload = _responses_payload(provider, body, model)
    upstream = _bounded_stream_response(
        _request(provider, payload, incoming, True, context_check=context_check)
    )
    validated = _validated_responses_stream(upstream, terminal_callback, provider)
    if on_stream_event is not None:
        validated = _observing_stream(validated, on_stream_event)
    if classify_dialect(provider) == PORTABLE_RESPONSES:
        return _project_responses_stream(
            provider,
            validated,
            custom_names=custom_tool_names(body),
            preserve_reasoning_summary=model.get(
                "_emp_preserve_reasoning_summary"
            )
            is True,
            preserve_reasoning_state=model.get("_emp_preserve_reasoning_state")
            is True,
        )
    return validated


def _observing_stream(
    chunks: Iterable[bytes],
    on_event: Callable[[Mapping[str, Any]], None],
) -> Iterator[bytes]:
    """Forward SSE bytes unchanged while notifying one parsed-event callback.

    Parses SSE events from the raw byte stream without consuming it: each
    incoming chunk is yielded verbatim, and complete data events are parsed
    and passed to on_event. The stream is never buffered in full or
    re-serialized.
    """
    buffer = bytearray()
    data_lines: list = []
    try:
        for chunk in chunks:
            if not isinstance(chunk, (bytes, bytearray)):
                chunk = str(chunk).encode("utf-8")
            buffer.extend(chunk)
            while True:
                nl = buffer.find(b"\n")
                if nl < 0:
                    break
                raw_line = bytes(buffer[:nl])
                del buffer[: nl + 1]
                line = raw_line.rstrip(b"\r")
                if line.startswith(b"data:"):
                    data_lines.append(line[5:].lstrip())
                elif not line and data_lines:
                    raw = b"\n".join(data_lines)
                    data_lines = []
                    if raw != b"[DONE]":
                        try:
                            value = json.loads(raw.decode("utf-8"))
                            if isinstance(value, dict):
                                on_event(value)
                        except Exception:
                            pass
            yield bytes(chunk)
        if buffer:
            line = bytes(buffer).rstrip(b"\r")
            if line.startswith(b"data:"):
                data_lines.append(line[5:].lstrip())
            elif not line and data_lines:
                raw = b"\n".join(data_lines)
                data_lines = []
                if raw != b"[DONE]":
                    try:
                        value = json.loads(raw.decode("utf-8"))
                        if isinstance(value, dict):
                            on_event(value)
                    except Exception:
                        pass
    finally:
        closer = getattr(chunks, "close", None)
        if callable(closer):
            try:
                closer()
            except Exception:
                pass


def _project_responses_stream(
    provider: Dict[str, Any],
    chunks: Iterable[bytes],
    custom_names: set = None,
    preserve_reasoning_summary: bool = False,
    preserve_reasoning_state: bool = False,
) -> Iterator[bytes]:
    suppressed_item_ids = set()
    custom_state = {}
    try:
        for event in sse_json_events(chunks):
            projected = project_stream_event(
                provider,
                event,
                suppressed_item_ids,
                custom_names=custom_names,
                custom_state=custom_state,
                preserve_reasoning_summary=preserve_reasoning_summary,
                preserve_reasoning_state=preserve_reasoning_state,
            )
            if projected is not None:
                event_type = str(projected.get("type") or "message")
                yield _sse_frame(event_type, projected)
    except ProjectionError as exc:
        message = (
            "external upstream returned an unexpected compaction item"
            if exc.failure_class == "external_compaction"
            else "external upstream response projection failed"
        )
        raise ExternalProtocolError(message) from exc
    except TransportError as exc:
        raise StreamBoundaryError(
            "upstream Responses stream projection failed", "malformed_terminal"
        ) from exc


def forward_responses_compact(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
) -> Tuple[int, str, bytes]:
    payload = _responses_payload(provider, body, model)
    with _request(
        provider,
        payload,
        incoming,
        False,
        "responses_compact",
        context_check=context_check,
    ) as response:
        content_type = response.headers.get("Content-Type", "application/json")
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses compact response")
        _raise_if_context_response(provider, response.status, content_type, raw)
        return response.status, content_type, raw


def _body_with_supported_effort(
    provider: Mapping[str, Any], body: Mapping[str, Any], model: Mapping[str, Any]
) -> Dict[str, Any]:
    projected = dict(body)
    if provider.get("auth_mode") in {"account", "forward"}:
        return projected
    reasoning = projected.get("reasoning")
    if not isinstance(reasoning, Mapping) or "effort" not in reasoning:
        return projected
    levels = model.get("reasoning_levels")
    effort = reasoning.get("effort")
    if isinstance(levels, list) and effort in levels:
        return projected
    reasoning = dict(reasoning)
    reasoning.pop("effort", None)
    if reasoning:
        projected["reasoning"] = reasoning
    else:
        projected.pop("reasoning", None)
    return projected


def _reasoning_summary_policy(
    config: Mapping[str, Any], route: str
) -> str:
    presentations = config.get("catalog_presentations")
    value = presentations.get(route) if isinstance(presentations, Mapping) else None
    policy = value.get("reasoning_summary") if isinstance(value, Mapping) else "auto"
    return policy if policy in {"auto", "show", "hide"} else "auto"


def _prepare_reasoning_summary_route(
    config: Mapping[str, Any],
    provider: Mapping[str, Any],
    model: Dict[str, Any],
    route: str,
    body: Mapping[str, Any],
) -> Dict[str, Any]:
    """Apply a structured-summary policy without touching native requests."""

    if classify_dialect(provider) == CODEX_NATIVE:
        return dict(body)
    policy = _reasoning_summary_policy(config, route)
    supported = model.get("supports_reasoning_summaries") is True
    configured_protocol = provider.get("protocol")
    responses_capable = configured_protocol == "responses" or (
        configured_protocol == "auto"
        and _observed_protocol(provider, model) == "responses"
    )
    model["_emp_reasoning_summary_policy"] = policy
    model["_emp_preserve_reasoning_summary"] = bool(
        supported and responses_capable and policy != "hide"
    )
    # Opaque reasoning state is scoped to the upstream that created it.  It is
    # never copied across a model/provider switch; only Codex-visible history
    # reconstructed by HistoryContinuityEngine may cross that boundary.
    model["_emp_preserve_reasoning_state"] = False
    projected = dict(body)
    reasoning = projected.get("reasoning")
    reasoning = dict(reasoning) if isinstance(reasoning, Mapping) else {}
    if not responses_capable or not supported or policy == "hide":
        reasoning.pop("summary", None)
    elif policy == "show":
        reasoning["summary"] = "auto"
    if reasoning:
        projected["reasoning"] = reasoning
    else:
        projected.pop("reasoning", None)
    return projected


def _responses_payload(
    provider: Dict[str, Any], body: Dict[str, Any], model: Dict[str, Any]
) -> Dict[str, Any]:
    try:
        payload = project_request(
            provider,
            _body_with_supported_effort(provider, body, model),
            preserve_reasoning_state=model.get("_emp_preserve_reasoning_state")
            is True,
        )
    except ProjectionError as exc:
        if exc.failure_class in {"opaque_compaction", "invalid_compaction"}:
            raise HistoryReconstructionError("history_projection_incomplete") from exc
        raise RouterError(str(exc), 422) from exc
    payload["model"] = _upstream_model(provider, model, body["model"])
    return payload


def _preflight_history_projection(
    provider: Dict[str, Any], body: Dict[str, Any], model: Dict[str, Any]
) -> None:
    """Raise history failures before a lazy stream adapter starts."""

    try:
        if provider.get("protocol") == "responses":
            _responses_payload(provider, body, model)
        elif provider.get("protocol") == "chat_completions":
            responses_to_chat(body, _upstream_model(provider, model, body["model"]))
        elif provider.get("protocol") == "anthropic_messages":
            responses_to_anthropic(
                body, _upstream_model(provider, model, body["model"])
            )
    except HistoryReconstructionError:
        raise
    except RouterError:
        # Ordinary projection failures retain the existing stream error path.
        return


def prepare_native_websocket_request(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    on_context: Optional[Callable[..., Mapping[str, Any]]] = None,
    history_preparer: Optional[HistoryPreparationCallback] = None,
) -> Optional[NativeWebSocketPlan]:
    """Prepare one native Responses WebSocket request without sending it.

    Returning ``None`` means the selected route is not a native Responses
    destination and must use the ordinary protocol adapter.  Credential-bearing
    headers and request content remain transient on the returned plan.
    """

    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    provider = dict(provider)
    model = dict(model)
    if provider.get("protocol") != "responses" or classify_dialect(provider) != CODEX_NATIVE:
        return None
    prepared = _prepare_reasoning_summary_route(
        config, provider, model, model_id, body
    )
    prepared = _prepare_continuity_body(
        config,
        provider,
        model,
        model_id,
        prepared,
        incoming,
        history_preparer,
    )
    payload = _responses_payload(provider, prepared, model)
    context_observation: Dict[str, Any] = {}
    context_check = _bind_context_check(provider, model, on_context)
    if context_check is not None:
        try:
            observed = context_check(payload, True, "")
        except ContextGuardBlocked as exc:
            raise ContextLengthError(exc.assessment.to_safe_dict(), preflight=True) from exc
        if isinstance(observed, Mapping):
            context_observation = dict(observed)
    try:
        full_headers, identity_headers = _native_route_headers(provider, incoming)
        identity = _native_route_identity(
            provider, model, model_id, identity_headers
        )
    except (AccountError, NativeIdentityError, KeyError, TypeError):
        # HTTP forwarding remains the compatibility path when a stable native
        # connection identity cannot be proven.
        return None
    endpoint = _endpoint(provider)
    parsed = urlparse(endpoint)
    if parsed.scheme not in {"http", "https", "ws", "wss"} or not parsed.netloc:
        raise RouterError("native upstream websocket endpoint is invalid", 502)
    websocket_url = parsed._replace(
        scheme={"http": "ws", "https": "wss"}.get(parsed.scheme, parsed.scheme)
    ).geturl()
    headers = _headers(provider, incoming, True, full_headers)
    # _native_route_headers performs stricter identity validation; use the same
    # resolved credentials for the actual handshake.
    for key, value in full_headers.items():
        if key.lower() in {"authorization", "chatgpt-account-id"}:
            headers[key] = value
    payload["type"] = "response.create"
    connection_key = identity.connection_key
    return NativeWebSocketPlan(
        target=NativeWebSocketTarget(websocket_url, headers, connection_key),
        provider=provider,
        model=model,
        requested_slug=model_id,
        payload=payload,
        identity=identity,
        context_observation=context_observation,
    )


def chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    allow_retries: bool = True,
) -> Tuple[int, str, bytes]:
    body = _body_with_supported_effort(provider, body, model)
    payload = responses_to_chat(body, _upstream_model(provider, model, body["model"]))
    with _request(
        provider,
        payload,
        incoming,
        False,
        context_check=context_check,
        allow_retries=allow_retries,
    ) as response:
        content_type = response.headers.get("Content-Type", "application/json")
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Chat Completions 响应")
    _raise_if_context_response(provider, response.status, content_type, raw)
    try:
        value = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise RouterError("chat-completions upstream returned invalid JSON", 502)
    return 200, "application/json", json.dumps(
        _response_from_chat(
            value,
            body["model"],
            custom_names=custom_tool_names(body),
        )
    ).encode("utf-8")


def anthropic_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    allow_retries: bool = True,
) -> Tuple[int, str, bytes]:
    payload = responses_to_anthropic(body, _upstream_model(provider, model, body["model"]))
    with _request(
        provider,
        payload,
        incoming,
        False,
        context_check=context_check,
        allow_retries=allow_retries,
    ) as response:
        content_type = response.headers.get("Content-Type", "application/json")
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Anthropic 响应")
    _raise_if_context_response(provider, response.status, content_type, raw)
    try:
        value = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise RouterError("Anthropic upstream returned invalid JSON", 502)
    return 200, "application/json", json.dumps(
        _response_from_anthropic(
            value, body["model"], custom_names=custom_tool_names(body)
        )
    ).encode("utf-8")


def _response_text(raw: bytes) -> str:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise ExternalCompactionError("invalid_response") from exc
    text = value.get("output_text") if isinstance(value, dict) else None
    if isinstance(text, str) and text.strip():
        summary = text.strip()
    else:
        parts = []
        for item in value.get("output", []) if isinstance(value, dict) else []:
            if not isinstance(item, dict):
                continue
            for part in item.get("content", []) or []:
                if isinstance(part, dict) and part.get("type") in ("output_text", "text"):
                    parts.append(str(part.get("text", "")))
        summary = "\n".join(parts).strip()
    if not summary:
        raise ExternalCompactionError("summary_empty")
    if len(summary) > MAX_EXTERNAL_COMPACTION_SUMMARY_CHARS:
        raise ExternalCompactionError("summary_too_large")
    return summary


def _summary_body(body: Dict[str, Any]) -> Dict[str, Any]:
    source = body.get("input", [])
    if isinstance(source, str):
        source = [_message_item(source)]
    elif isinstance(source, dict):
        source = [source]
    elif not isinstance(source, list):
        source = []
    return {
        "model": body["model"],
        "input": list(_normalize_compaction_input(source, drop_trigger=True))
        + [_message_item(_COMPACTION_PROMPT)],
        "stream": False,
        "tools": [],
    }


def _summarize(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
) -> str:
    payload = _summary_body(body)
    if provider["protocol"] == "responses":
        _, _, raw = forward_responses(
            provider,
            payload,
            model,
            incoming,
            context_check=context_check,
            allow_retries=False,
        )
    elif provider["protocol"] == "chat_completions":
        _, _, raw = chat_completion(
            provider,
            payload,
            model,
            incoming,
            context_check=context_check,
            allow_retries=False,
        )
    else:
        _, _, raw = anthropic_completion(
            provider,
            payload,
            model,
            incoming,
            context_check=context_check,
            allow_retries=False,
        )
    return _response_text(raw)


def _external_compaction_response(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[
        Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]
    ] = None,
) -> Dict[str, Any]:
    """Ask the selected external model for one summary and wrap it for Codex."""

    summary = _summarize(provider, body, model, incoming, context_check)
    return _compaction_response(body["model"], summary)


def _retained_user_messages(source: Any, budget: int = 80_000) -> list:
    if not isinstance(source, list):
        return []
    messages = []
    for item in source:
        if not isinstance(item, dict) or item.get("type", "message") != "message":
            continue
        if item.get("role") != "user":
            continue
        text = _content_text(item.get("content", ""))
        if text.strip():
            messages.append(text)
    retained = []
    for text in reversed(messages):
        if budget <= 0:
            break
        retained.append(text[-budget:])
        budget -= len(retained[-1])
    return [_message_item(text) for text in reversed(retained)]


def _compaction_response(model: str, summary: str) -> Dict[str, Any]:
    item = {
        "id": "cmp_" + uuid.uuid4().hex,
        "type": "compaction",
        "encrypted_content": _encode_compaction(summary),
    }
    return {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "created_at": int(time.time()),
        "status": "completed",
        "model": model,
        "output": [item],
        "usage": None,
    }


def _is_compaction_trigger(body: Dict[str, Any]) -> bool:
    source = body.get("input")
    return bool(
        isinstance(source, list)
        and source
        and isinstance(source[-1], dict)
        and source[-1].get("type") == "compaction_trigger"
    )


def _stream_adapter_io() -> StreamAdapterIO:
    return StreamAdapterIO(
        request=_request,
        read_limited=_read_limited,
        raise_if_context_response=_raise_if_context_response,
        body_with_supported_effort=_body_with_supported_effort,
        upstream_model=_upstream_model,
    )


def _validated_responses_stream(
    response: Any,
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    provider: Optional[Dict[str, Any]] = None,
) -> Iterator[bytes]:
    return _owned_validated_responses_stream(
        _stream_adapter_io(), response, terminal_callback, provider
    )


def stream_chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[
        Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]
    ] = None,
) -> Iterable[bytes]:
    return _owned_stream_chat_completion(
        _stream_adapter_io(),
        provider,
        body,
        model,
        incoming,
        terminal_callback,
        context_check,
    )


def stream_anthropic_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[
        Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]
    ] = None,
) -> Iterable[bytes]:
    return _owned_stream_anthropic_completion(
        _stream_adapter_io(),
        provider,
        body,
        model,
        incoming,
        terminal_callback,
        context_check,
    )


def _observed_protocol(
    provider: Dict[str, Any], model: Dict[str, Any]
) -> Optional[str]:
    """Return a resolved protocol only when every identity component matches."""

    endpoint = endpoint_fingerprint(provider.get("base_url"))
    deployment = deployment_identity(provider, model)
    upstream = str(model.get("upstream_id") or "").strip()
    if not upstream:
        return None
    for source in (model, provider):
        protocol = source.get("resolved_protocol")
        observation = source.get("protocol_observation")
        if protocol not in _CONCRETE_PROTOCOLS or not isinstance(observation, dict):
            continue
        if observation.get("endpoint_fingerprint") != endpoint:
            continue
        if observation.get("deployment_identity") != deployment:
            continue
        if observation.get("upstream_model") != upstream:
            continue
        return protocol
    return None


def _auto_protocol_candidates(
    provider: Dict[str, Any], model: Optional[Dict[str, Any]] = None
) -> Tuple[str, ...]:
    if provider.get("auth_mode") == "anthropic_api_key":
        normal = ("anthropic_messages",)
    else:
        base = str(provider.get("base_url", "")).rstrip("/")
        normal = (
            ("responses", "chat_completions")
            if base.endswith("/responses")
            else ("chat_completions", "responses")
        )
    observed = _observed_protocol(provider, model or {})
    if observed in normal:
        return (observed,) + tuple(item for item in normal if item != observed)
    return normal


def _tag_route(
    metadata: Dict[str, Any],
    provider: Dict[str, Any],
    model: Optional[Dict[str, Any]] = None,
    decision: str = "explicit",
    fallback: bool = False,
    request: Optional[Mapping[str, Any]] = None,
) -> Dict[str, Any]:
    tagged = dict(metadata)
    tagged["provider_id"] = provider.get("id")
    tagged["model_id"] = (model or {}).get("id")
    tagged["resolved_protocol"] = provider.get("protocol")
    tagged["protocol_decision"] = decision
    tagged["protocol_fallback"] = bool(fallback)
    tagged["dialect"] = classify_dialect(provider)
    if isinstance(request, Mapping):
        tagged.update(request_shape(request))
    return tagged


def _route_protocol_observation(
    provider: Dict[str, Any], model: Dict[str, Any]
) -> Dict[str, Any]:
    return {
        "source": "observed",
        "confidence": 1.0,
        "observed_at": observed_at_now(),
        "endpoint_fingerprint": endpoint_fingerprint(provider.get("base_url")),
        "deployment_identity": deployment_identity(provider, model),
        "upstream_model": str(model.get("upstream_id") or ""),
    }


def _route_event(
    metadata: Dict[str, Any],
    provider: Dict[str, Any],
    model: Dict[str, Any],
    terminal: Optional[Dict[str, Any]] = None,
    response_bytes: int = 0,
) -> Dict[str, Any]:
    terminal = terminal or {}
    event = dict(metadata)
    event["response_bytes"] = max(0, int(response_bytes or 0))
    status = terminal.get("status", metadata.get("status"))
    event["status"] = status
    success = terminal.get(
        "success",
        isinstance(status, int) and 200 <= status < 300,
    )
    event["success"] = bool(success)
    event["error_class"] = (
        "none"
        if event["success"]
        else terminal.get("error_class") or _route_error_class(status)
    )
    event["endpoint_fingerprint"] = endpoint_fingerprint(provider.get("base_url"))
    event["deployment_identity"] = deployment_identity(provider, model)
    event["upstream_model"] = str(model.get("upstream_id") or "")
    context_observation = terminal.get("context_observation") or provider.get(
        "_context_observation"
    )
    if isinstance(context_observation, Mapping):
        event["context_observation"] = dict(context_observation)
    event.update(_safe_transport_metadata(provider.get("_transport_metadata")))
    for key in (
        "close_code",
        "output_emitted",
        "tool_activity",
        "terminal_event_observed",
        "recovery_succeeded",
        "recovery_mode",
    ):
        if key in terminal:
            event[key] = terminal[key]
    failure_reason = terminal.get("failure_reason")
    if isinstance(failure_reason, str) and failure_reason:
        event["failure_reason"] = failure_reason
    if event["success"]:
        event["protocol_observation"] = _route_protocol_observation(provider, model)
    return event


def _emit_observation(
    callback: Optional[Callable[[Dict[str, Any]], None]], event: Dict[str, Any]
) -> None:
    if callback is None:
        return
    try:
        callback(event)
    except Exception:
        pass


def _bind_context_check(
    provider: Dict[str, Any],
    model: Dict[str, Any],
    callback: Optional[Callable[..., Mapping[str, Any]]],
) -> Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]]:
    if callback is None:
        return None

    def check(payload: Dict[str, Any], stream: bool, operation: str) -> Mapping[str, Any]:
        return callback(provider, model, provider.get("protocol", "unknown"), payload, stream, operation)

    return check


def _stream_signal(chunk: Any) -> Optional[Dict[str, Any]]:
    raw = chunk.decode("utf-8", "replace") if isinstance(chunk, bytes) else str(chunk)
    for block in re.split(r"\r?\n\r?\n", raw):
        data = []
        for line in block.splitlines():
            if line.startswith("data:"):
                data.append(line[5:].lstrip())
        if not data or data == ["[DONE]"]:
            continue
        try:
            event = json.loads("\n".join(data))
        except ValueError:
            continue
        if not isinstance(event, Mapping):
            continue
        event_type = event.get("type")
        response = event.get("response")
        response = response if isinstance(response, Mapping) else {}
        if event_type == "response.completed":
            return {"success": True, "status": 200, "error_class": "none"}
        if event_type == "response.incomplete":
            details = response.get("incomplete_details")
            reason = details.get("reason") if isinstance(details, Mapping) else None
            error_class = {
                "max_output_tokens": "output_limit",
                "content_filter": "content_filter",
            }.get(reason, "stream_incomplete")
            return {"success": False, "status": 200, "error_class": error_class}
        if event_type == "response.failed":
            error = response.get("error")
            error = error if isinstance(error, Mapping) else {}
            status = error.get("status")
            if not isinstance(status, int) or isinstance(status, bool):
                status = 502
            return {
                "success": False,
                "status": status,
                "error_class": _route_error_class(status),
            }
    return None


def _observed_stream(
    result: Any,
    metadata: Dict[str, Any],
    provider: Dict[str, Any],
    model: Dict[str, Any],
    callback: Callable[[Dict[str, Any]], None],
    terminal: Dict[str, Any],
) -> Iterator[bytes]:
    response_bytes = 0
    saw_terminal = False
    natural_end = False
    reported = False

    def report(value: Dict[str, Any]) -> None:
        nonlocal reported
        if reported:
            return
        reported = True
        _emit_observation(
            callback,
            _route_event(metadata, provider, model, value, response_bytes),
        )

    try:
        for chunk in result:
            response_bytes += len(chunk if isinstance(chunk, bytes) else str(chunk).encode("utf-8"))
            signal = terminal if terminal else _stream_signal(chunk)
            if signal:
                terminal.update(signal)
                saw_terminal = True
            yield chunk
        natural_end = True
    except GeneratorExit:
        raise
    except Exception as exc:
        if not terminal or terminal.get("success") is not True:
            status, error_class = _stream_exception(exc)
            terminal.update(
                {"success": False, "status": status, "error_class": error_class}
            )
        raise
    finally:
        close = getattr(result, "close", None)
        if callable(close):
            try:
                close()
            except Exception:
                pass
        if not reported:
            if terminal:
                report(terminal)
            elif saw_terminal:
                report({"success": True, "status": 200, "error_class": "none"})
            elif natural_end:
                report({"success": False, "status": 502, "error_class": "stream_incomplete"})
            else:
                report({"success": False, "status": None, "error_class": "client_disconnect"})


def _auto_stream_result(
    candidates: Tuple[str, ...],
    provider: Dict[str, Any],
    model: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    decision: str,
    callback: Optional[Callable[[Dict[str, Any]], None]],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    on_stream_event: Optional[Callable[[Mapping[str, Any]], None]] = None,
) -> Iterator[bytes]:
    for index, protocol in enumerate(candidates):
        resolved = dict(provider)
        resolved["protocol"] = protocol
        candidate_decision = "fallback_rejection" if index else decision
        metadata = _tag_route(
            {"kind": "stream", "status": 200, "content_type": "text/event-stream"},
            resolved,
            model,
            candidate_decision,
            index > 0,
            request=body,
        )
        terminal: Dict[str, Any] = {}
        result = None
        emitted = False
        response_bytes = 0
        fallback = False
        reported = False
        closed_by_caller = False
        try:
            _, result = _proxy_resolved(
                resolved,
                model,
                body,
                incoming,
                candidate_decision,
                index > 0,
                terminal_callback=lambda value: terminal.update(value),
                context_check=_bind_context_check(resolved, model, context_check),
                on_stream_event=on_stream_event,
            )
            for chunk in result:
                response_bytes += len(chunk if isinstance(chunk, bytes) else str(chunk).encode("utf-8"))
                signal = terminal if terminal else _stream_signal(chunk)
                if signal:
                    terminal.update(signal)
                if (
                    not emitted
                    and not terminal.get("success", True)
                    and terminal.get("status") in _PROTOCOL_REJECTION_STATUSES
                    and index + 1 < len(candidates)
                ):
                    fallback = True
                    break
                emitted = True
                yield chunk
            if fallback:
                continue
            if not terminal:
                terminal.update(
                    {"success": False, "status": 502, "error_class": "stream_incomplete"}
                )
            if callback is not None:
                _emit_observation(
                    callback,
                    _route_event(metadata, resolved, model, terminal, response_bytes),
                )
                reported = True
            return
        except GeneratorExit:
            closed_by_caller = True
            raise
        except RouterError as exc:
            if index + 1 < len(candidates) and exc.status in _PROTOCOL_REJECTION_STATUSES:
                continue
            if callback is not None and not reported:
                _emit_observation(
                    callback,
                    _route_event(
                        metadata,
                        resolved,
                        model,
                        _terminal_exception(exc),
                        response_bytes,
                    ),
                )
                reported = True
            raise
        except Exception as exc:
            if callback is not None and not reported:
                status, error_class = _stream_exception(exc)
                _emit_observation(
                    callback,
                    _route_event(
                        metadata,
                        resolved,
                        model,
                        {
                            "success": False,
                            "status": status,
                            "error_class": error_class,
                        },
                        response_bytes,
                    ),
                )
                reported = True
            raise
        finally:
            close = getattr(result, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass
            if closed_by_caller and callback is not None and not reported:
                _emit_observation(
                    callback,
                    _route_event(
                        metadata,
                        resolved,
                        model,
                        {"success": False, "status": None, "error_class": "client_disconnect"},
                        response_bytes,
                    ),
                )


def _identity_headers(headers: Mapping[str, Any]) -> Dict[str, str]:
    if not isinstance(headers, Mapping):
        return {}
    for key, value in headers.items():
        if (
            isinstance(key, str)
            and key.lower() == "chatgpt-account-id"
            and isinstance(value, str)
        ):
            return {"chatgpt-account-id": value}
    return {}


def _native_route_headers(
    provider: Mapping[str, Any], incoming: Mapping[str, str]
) -> Tuple[Dict[str, str], Dict[str, str]]:
    auth_mode = provider.get("auth_mode")
    if auth_mode == "account":
        account = provider.get("account")
        if not isinstance(account, Mapping):
            raise NativeIdentityError("native route authentication is unavailable")
        selected = auth_headers(account)
        if not isinstance(selected, Mapping):
            raise NativeIdentityError("native route authentication is unavailable")
        full = {
            key: value
            for key, value in selected.items()
            if isinstance(key, str) and isinstance(value, str)
        }
    elif auth_mode == "forward":
        if provider.get("implicit_native") is True:
            try:
                selected = native_auth_headers(provider.get("_native_auth_path"))
            except AccountError:
                selected = incoming
        else:
            selected = incoming
        full = {
            key: value
            for key, value in selected.items()
            if isinstance(key, str) and isinstance(value, str)
        }
    else:
        raise NativeIdentityError("native route authentication is unavailable")
    return full, _identity_headers(full)


def _native_route_identity(
    provider: Mapping[str, Any],
    model: Mapping[str, Any],
    requested_slug: str,
    identity_headers: Mapping[str, str],
):
    return derive_native_route_identity(
        identity_headers,
        provider.get("base_url"),
        deployment_identity(provider, model),
        _upstream_model(provider, model, requested_slug),
    )


def _prepare_continuity_body(
    config: Dict[str, Any],
    provider: Dict[str, Any],
    model: Dict[str, Any],
    requested_slug: str,
    body: Dict[str, Any],
    incoming: Dict[str, str],
    history_preparer: Optional[HistoryPreparationCallback],
) -> Dict[str, Any]:
    if history_preparer is not None:
        try:
            prepared = history_preparer(
                config, provider, model, requested_slug, body, incoming
            )
        except HistoryReconstructionError:
            raise
        except Exception:
            raise HistoryReconstructionError("history_unavailable") from None
        if not isinstance(prepared, Mapping):
            raise HistoryReconstructionError("invalid_history_projection")
        return dict(prepared)

    source = body.get("input")
    items = [source] if isinstance(source, Mapping) else source
    if not isinstance(items, list):
        return dict(body)
    for item in items:
        if not isinstance(item, Mapping) or item.get("type") != "compaction":
            continue
        encoded = item.get("encrypted_content")
        if not isinstance(encoded, str) or not encoded.startswith(_COMPACTION_PREFIX):
            raise HistoryReconstructionError("history_reader_unavailable")
    return dict(body)


def _proxy_resolved(
    provider: Dict[str, Any],
    model: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    decision: str = "explicit",
    fallback: bool = False,
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
    on_stream_event: Optional[Callable[[Mapping[str, Any]], None]] = None,
) -> Tuple[Dict[str, Any], Any]:
    if _is_compaction_trigger(body) and classify_dialect(provider) != CODEX_NATIVE:
        response = _external_compaction_response(
            provider, body, model, incoming, context_check
        )
        if body.get("stream"):
            return _tag_route(
                {
                    "kind": "stream",
                    "status": 200,
                    "content_type": "text/event-stream",
                },
                provider,
                model,
                decision,
                fallback,
                request=body,
            ), _response_json_stream(response)
        return _tag_route(
            {
                "kind": "body",
                "status": 200,
                "content_type": "application/json",
            },
            provider,
            model,
            decision,
            fallback,
            request=body,
        ), json.dumps(response, ensure_ascii=False).encode("utf-8")
    if body.get("stream") and provider["protocol"] == "chat_completions":
        _preflight_history_projection(provider, body, model)
        return _tag_route(
            {"kind": "stream", "status": 200, "content_type": "text/event-stream"},
            provider,
            model,
            decision,
            fallback,
            request=body,
        ), _reliable_responses_stream(
            lambda: stream_chat_completion(
                provider, body, model, incoming, None, context_check
            ),
            terminal_callback,
        )
    if body.get("stream") and provider["protocol"] == "anthropic_messages":
        _preflight_history_projection(provider, body, model)
        return _tag_route(
            {"kind": "stream", "status": 200, "content_type": "text/event-stream"},
            provider,
            model,
            decision,
            fallback,
            request=body,
        ), _reliable_responses_stream(
            lambda: stream_anthropic_completion(
                provider, body, model, incoming, None, context_check
            ),
            terminal_callback,
        )
    if provider["protocol"] == "responses":
        if body.get("stream"):
            _preflight_history_projection(provider, body, model)
            upstream = _reliable_responses_stream(
                lambda: forward_responses_stream(
                    provider, body, model, incoming, None, context_check,
                    on_stream_event=on_stream_event,
                ),
                terminal_callback,
            )
            return _tag_route(
                {
                    "kind": "stream",
                    "status": 200,
                    "content_type": "text/event-stream",
                },
                provider,
                model,
                decision,
                fallback,
                request=body,
            ), upstream
        status, content_type, raw = forward_responses(
            provider, body, model, incoming, context_check
        )
    elif provider["protocol"] == "chat_completions":
        status, content_type, raw = chat_completion(
            provider, body, model, incoming, context_check
        )
    else:
        status, content_type, raw = anthropic_completion(
            provider, body, model, incoming, context_check
        )
    return _tag_route(
        {"kind": "body", "status": status, "content_type": content_type},
        provider,
        model,
        decision,
        fallback,
        request=body,
    ), raw


def _finish_nonstream(
    callback: Optional[Callable[[Dict[str, Any]], None]],
    metadata: Dict[str, Any],
    provider: Dict[str, Any],
    model: Dict[str, Any],
    result: Any,
) -> None:
    if callback is None:
        return
    response_bytes = len(result) if isinstance(result, (bytes, bytearray)) else 0
    terminal: Dict[str, Any] = {}
    if classify_dialect(provider) != CODEX_NATIVE and isinstance(
        result, (bytes, bytearray)
    ):
        try:
            terminal = responses_terminal_observation(
                json.loads(bytes(result).decode("utf-8", errors="strict"))
            )
        except (UnicodeDecodeError, ValueError, RouterError):
            terminal = {
                "status": 502,
                "success": False,
                "error_class": "stream_error",
            }
    _emit_observation(
        callback,
        _route_event(
            metadata,
            provider,
            model,
            terminal=terminal,
            response_bytes=response_bytes,
        ),
    )


def proxy(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    on_observation: Optional[Callable[[Dict[str, Any]], None]] = None,
    on_context: Optional[Callable[..., Mapping[str, Any]]] = None,
    history_preparer: Optional[HistoryPreparationCallback] = None,
    on_stream_event: Optional[Callable[[Mapping[str, Any]], None]] = None,
) -> Tuple[Dict[str, Any], Any]:
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    provider = dict(provider)
    model = dict(model)
    body = _prepare_reasoning_summary_route(
        config, provider, model, model_id, body
    )
    try:
        body = _prepare_continuity_body(
            config,
            provider,
            model,
            model_id,
            body,
            incoming,
            history_preparer,
        )
    except HistoryReconstructionError as exc:
        _emit_observation(
            on_observation,
            {
                "route": "responses",
                "transport": "unknown",
                "status": exc.status,
                "success": False,
                "error_class": exc.error_class,
                "failure_reason": exc.reason,
                "terminal_event_observed": True,
            },
        )
        raise
    except RouterError as exc:
        _emit_observation(
            on_observation,
            _route_event(
                _tag_route(
                    {"kind": "body", "status": exc.status},
                    provider,
                    model,
                    request=body,
                ),
                provider,
                model,
                _terminal_exception(exc),
            ),
        )
        raise
    if provider.get("protocol") != "auto":
        terminal: Dict[str, Any] = {}
        try:
            metadata, result = _proxy_resolved(
                provider,
                model,
                body,
                incoming,
                terminal_callback=terminal.update if on_observation and body.get("stream") else None,
                context_check=_bind_context_check(provider, model, on_context),
                on_stream_event=on_stream_event,
            )
        except RouterError as exc:
            _emit_observation(
                on_observation,
                _route_event(
                    _tag_route(
                        {"kind": "body", "status": exc.status},
                        provider,
                        model,
                        request=body,
                    ),
                    provider,
                    model,
                    _terminal_exception(exc),
                ),
            )
            raise
        if body.get("stream"):
            if on_observation:
                result = _observed_stream(
                    result,
                    metadata,
                    provider,
                    model,
                    on_observation,
                    terminal,
                )
                metadata["observation_attached"] = True
        else:
            _finish_nonstream(on_observation, metadata, provider, model, result)
        return metadata, result

    observed = _observed_protocol(provider, model)
    candidates = _auto_protocol_candidates(provider, model)
    decision = "observed_priority" if observed and candidates[0] == observed else "normal_order"
    if body.get("stream"):
        first = dict(provider)
        first["protocol"] = candidates[0]
        metadata = _tag_route(
            {"kind": "stream", "status": 200, "content_type": "text/event-stream"},
            first,
            model,
            decision,
            False,
            request=body,
        )
        result = _auto_stream_result(
            candidates,
            provider,
            model,
            body,
            incoming,
            decision,
            on_observation,
            on_context,
            on_stream_event,
        )
        metadata["observation_attached"] = bool(on_observation)
        return metadata, result

    for index, protocol in enumerate(candidates):
        resolved = dict(provider)
        resolved["protocol"] = protocol
        candidate_decision = "fallback_rejection" if index else decision
        try:
            metadata, result = _proxy_resolved(
                resolved,
                model,
                body,
                incoming,
                candidate_decision,
                index > 0,
                context_check=_bind_context_check(resolved, model, on_context),
            )
        except RouterError as exc:
            if index + 1 < len(candidates) and exc.status in _PROTOCOL_REJECTION_STATUSES:
                continue
            _emit_observation(
                on_observation,
                _route_event(
                    _tag_route(
                        {"kind": "body", "status": exc.status},
                        resolved,
                        model,
                        candidate_decision,
                        index > 0,
                        request=body,
                    ),
                    resolved,
                    model,
                        _terminal_exception(exc),
                ),
            )
            raise
        if (
            metadata.get("status") in _PROTOCOL_REJECTION_STATUSES
            and index + 1 < len(candidates)
        ):
            continue
        _finish_nonstream(on_observation, metadata, resolved, model, result)
        return metadata, result
    raise RouterError("unable to resolve provider protocol", 502)


def proxy_compact(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    on_observation: Optional[Callable[[Dict[str, Any]], None]] = None,
    on_context: Optional[Callable[..., Mapping[str, Any]]] = None,
    history_preparer: Optional[HistoryPreparationCallback] = None,
) -> Tuple[Dict[str, Any], bytes]:
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    provider = dict(provider)
    model = dict(model)
    try:
        body = _prepare_continuity_body(
            config,
            provider,
            model,
            model_id,
            body,
            incoming,
            history_preparer,
        )
    except RouterError as exc:
        _emit_observation(
            on_observation,
            _route_event(
                _tag_route(
                    {"kind": "body", "status": exc.status},
                    provider,
                    model,
                    request=body,
                ),
                provider,
                model,
                _terminal_exception(exc),
            ),
        )
        raise
    if provider.get("protocol") != "auto":
        try:
            metadata, raw = _proxy_compact_resolved(
                provider,
                model,
                body,
                incoming,
                context_check=_bind_context_check(provider, model, on_context),
            )
        except RouterError as exc:
            _emit_observation(
                on_observation,
                _route_event(
                    _tag_route(
                        {"kind": "body", "status": exc.status},
                        provider,
                        model,
                        request=body,
                    ),
                    provider,
                    model,
                    _terminal_exception(exc),
                ),
            )
            raise
        _finish_nonstream(on_observation, metadata, provider, model, raw)
        return metadata, raw
    observed = _observed_protocol(provider, model)
    candidates = _auto_protocol_candidates(provider, model)
    decision = "observed_priority" if observed and candidates[0] == observed else "normal_order"
    for index, protocol in enumerate(candidates):
        resolved = dict(provider)
        resolved["protocol"] = protocol
        candidate_decision = "fallback_rejection" if index else decision
        try:
            metadata, raw = _proxy_compact_resolved(
                resolved,
                model,
                body,
                incoming,
                candidate_decision,
                index > 0,
                context_check=_bind_context_check(resolved, model, on_context),
            )
        except RouterError as exc:
            if index + 1 < len(candidates) and exc.status in _PROTOCOL_REJECTION_STATUSES:
                continue
            _emit_observation(
                on_observation,
                _route_event(
                    _tag_route(
                        {"kind": "body", "status": exc.status},
                        resolved,
                        model,
                        candidate_decision,
                        index > 0,
                        request=body,
                    ),
                    resolved,
                    model,
                        _terminal_exception(exc),
                ),
            )
            raise
        if (
            metadata.get("status") in _PROTOCOL_REJECTION_STATUSES
            and index + 1 < len(candidates)
        ):
            continue
        _finish_nonstream(on_observation, metadata, resolved, model, raw)
        return metadata, raw
    raise RouterError("unable to resolve provider protocol", 502)


def _proxy_compact_resolved(
    provider: Dict[str, Any],
    model: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
    decision: str = "explicit",
    fallback: bool = False,
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
) -> Tuple[Dict[str, Any], bytes]:
    if classify_dialect(provider) == CODEX_NATIVE:
        status, content_type, raw = forward_responses_compact(
            provider, body, model, incoming, context_check
        )
    else:
        status, content_type = 200, "application/json"
        raw = json.dumps(
            _external_compaction_response(
                provider, body, model, incoming, context_check
            ),
            ensure_ascii=False,
        ).encode("utf-8")
    return _tag_route(
        {"kind": "body", "status": status, "content_type": content_type},
        provider,
        model,
        decision,
        fallback,
        request=body,
    ), raw
