"""Content-free transport failure taxonomy and stream lifecycle policy."""

from __future__ import annotations

from dataclasses import dataclass, field
import time
from typing import Any, Dict, Mapping, Optional, Set, Tuple
from urllib.error import URLError

from .router_errors import (
    ContextLengthError,
    RouterError,
    StreamBoundaryError,
    UpstreamHTTPError,
)


PHASE_CONNECT = "connect"
PHASE_FIRST_EVENT = "first_event"
PHASE_STREAMING = "streaming"
PHASE_TERMINAL = "terminal_validation"

CONNECT_TIMEOUT = "connect_timeout"
FIRST_EVENT_TIMEOUT = "first_event_timeout"
UPSTREAM_504 = "upstream_504"
IDLE_AFTER_OUTPUT = "idle_after_output"
LOCAL_DEADLINE = "local_deadline"
STREAM_INCOMPLETE = "stream_incomplete"

PROTOCOL_REJECTION_STATUSES = frozenset({404, 405, 415, 501})
_TOOL_TYPES = frozenset({"function_call", "custom_tool_call", "tool_call"})
_REPLAYABLE_TRANSPORT_CLASSES = frozenset(
    {CONNECT_TIMEOUT, FIRST_EVENT_TIMEOUT, "network", "proxy_reset"}
)
_KNOWN_ERROR_CLASSES = frozenset(
    {
        "none",
        "auth",
        "payment_required",
        "rate_limit",
        "protocol_rejection",
        "upstream_5xx",
        "timeout",
        UPSTREAM_504,
        CONNECT_TIMEOUT,
        FIRST_EVENT_TIMEOUT,
        IDLE_AFTER_OUTPUT,
        LOCAL_DEADLINE,
        "network",
        "router_error",
        "protocol_error",
        "stream_error",
        STREAM_INCOMPLETE,
        "client_disconnect",
        "malformed_terminal",
        "proxy_reset",
        "output_limit",
        "content_filter",
        "context_length_exceeded",
        "external_compaction_failed",
        "history_reconstruction_failed",
    }
)


def _safe_status(status: Any, fallback: int = 502) -> int:
    if isinstance(status, int) and not isinstance(status, bool):
        return status
    return fallback


def _safe_token(value: Any) -> Optional[str]:
    if not isinstance(value, str) or not value:
        return None
    value = value.strip().lower()
    value = "".join(
        character if character.isalnum() or character in {"_", "-"} else "_"
        for character in value
    )
    return value[:64] or None


def normalize_error_class(value: Any, fallback: str = "stream_error") -> str:
    value = _safe_token(value)
    return value if value in _KNOWN_ERROR_CLASSES else fallback


def status_error_class(status: Any) -> str:
    """Classify an HTTP boundary without treating status as protocol evidence."""

    status = _safe_status(status)
    if status in (401, 403):
        return "auth"
    if status == 402:
        return "payment_required"
    if status == 429:
        return "rate_limit"
    if status in PROTOCOL_REJECTION_STATUSES:
        return "protocol_rejection"
    if status == 504:
        return UPSTREAM_504
    if status == 408:
        return "timeout"
    if 500 <= status <= 599:
        return "upstream_5xx"
    return "router_error"


def is_protocol_rejection_status(status: Any) -> bool:
    return (
        isinstance(status, int)
        and not isinstance(status, bool)
        and status in PROTOCOL_REJECTION_STATUSES
    )


def protocol_fallback_allowed(
    status: Any,
    output_emitted: bool = False,
    terminal_event_observed: bool = False,
) -> bool:
    """Return whether auto protocol selection may try its next candidate."""

    return (
        is_protocol_rejection_status(status)
        and not output_emitted
        and not terminal_event_observed
    )


@dataclass(frozen=True)
class FailureSnapshot:
    """A bounded failure observation used by retry and diagnostic code."""

    error_class: str
    status: int = 502
    phase: str = PHASE_TERMINAL
    terminal_event: bool = False
    failure_reason: Optional[str] = None


class TransportFailure(RouterError):
    """A transport boundary error whose text contains no request/response data."""

    def __init__(
        self,
        error_class: str,
        status: Any = None,
        phase: Optional[str] = None,
        failure_reason: Optional[str] = None,
    ):
        error_class = normalize_error_class(error_class)
        if status is None:
            status = 504 if error_class in {
                CONNECT_TIMEOUT,
                FIRST_EVENT_TIMEOUT,
                UPSTREAM_504,
                IDLE_AFTER_OUTPUT,
                LOCAL_DEADLINE,
            } else 502
        status = _safe_status(status)
        if error_class == UPSTREAM_504:
            status = 504
        self.error_class = error_class
        self.phase = phase or {
            CONNECT_TIMEOUT: PHASE_CONNECT,
            FIRST_EVENT_TIMEOUT: PHASE_FIRST_EVENT,
            IDLE_AFTER_OUTPUT: PHASE_STREAMING,
            LOCAL_DEADLINE: PHASE_TERMINAL,
            STREAM_INCOMPLETE: PHASE_TERMINAL,
        }.get(error_class, PHASE_TERMINAL)
        self.failure_reason = _safe_token(failure_reason)
        super().__init__(
            "transport failure: class=%s" % self.error_class,
            status,
        )


def failure_from_exception(
    exc: BaseException,
    phase: Optional[str] = None,
    output_emitted: bool = False,
) -> FailureSnapshot:
    """Normalize exceptions at the phase where they crossed the boundary."""

    phase = phase or PHASE_TERMINAL
    if isinstance(exc, TransportFailure):
        return FailureSnapshot(
            exc.error_class,
            _safe_status(exc.status),
            exc.phase,
            False,
            exc.failure_reason,
        )
    if isinstance(exc, ContextLengthError):
        return FailureSnapshot(
            "context_length_exceeded", _safe_status(exc.status, 413), phase, False
        )
    if isinstance(exc, UpstreamHTTPError):
        reason = _safe_token(getattr(exc, "failure_reason", None))
        return FailureSnapshot(
            status_error_class(exc.status), _safe_status(exc.status), phase, False, reason
        )
    if isinstance(exc, StreamBoundaryError):
        error_class = normalize_error_class(
            getattr(exc, "error_class", None), STREAM_INCOMPLETE
        )
        return FailureSnapshot(
            error_class,
            _safe_status(exc.status),
            phase,
            False,
            _safe_token(getattr(exc, "failure_reason", None)),
        )
    if isinstance(exc, TimeoutError) or (
        isinstance(exc, URLError) and isinstance(getattr(exc, "reason", None), TimeoutError)
    ):
        if output_emitted:
            error_class = IDLE_AFTER_OUTPUT
            failure_phase = PHASE_STREAMING
        elif phase == PHASE_CONNECT:
            error_class = CONNECT_TIMEOUT
            failure_phase = PHASE_CONNECT
        else:
            error_class = FIRST_EVENT_TIMEOUT
            failure_phase = PHASE_FIRST_EVENT
        return FailureSnapshot(error_class, 504, failure_phase)
    if isinstance(exc, (OSError, URLError)):
        return FailureSnapshot("network", 502, phase)
    if isinstance(exc, RouterError):
        if (
            _safe_status(exc.status) == 504
            and not getattr(exc, "error_class", None)
            and str(exc) == "upstream request timed out"
        ):
            return FailureSnapshot(LOCAL_DEADLINE, 504, phase)
        error_class = normalize_error_class(
            getattr(exc, "error_class", None), status_error_class(exc.status)
        )
        return FailureSnapshot(
            error_class,
            _safe_status(exc.status),
            phase,
            False,
            _safe_token(getattr(exc, "failure_reason", None)),
        )
    return FailureSnapshot("stream_error", 502, phase)


def retry_allowed(
    failure: FailureSnapshot,
    attempt: int,
    replayable: bool,
    output_emitted: bool,
) -> bool:
    """Allow exactly one pre-output retry for a pure transport failure."""

    return (
        attempt == 0
        and replayable
        and not output_emitted
        and not failure.terminal_event
        and failure.error_class in _REPLAYABLE_TRANSPORT_CLASSES
    )


def _event_tool_key(event: Mapping[str, Any], item: Optional[Mapping[str, Any]]) -> Tuple[str, str]:
    item = item if isinstance(item, Mapping) else {}
    for source, key in ((item, "id"), (item, "call_id"), (event, "item_id"), (event, "call_id")):
        value = source.get(key)
        if isinstance(value, (str, int)) and not isinstance(value, bool) and str(value):
            return key, str(value)
    value = event.get("output_index")
    if isinstance(value, int) and not isinstance(value, bool):
        return "output_index", str(value)
    return "anonymous", "0"


def event_activity(event: Mapping[str, Any]) -> Tuple[bool, bool]:
    """Return visible-output and tool-activity flags without retaining content."""

    event_type = str(event.get("type") or "")
    item = event.get("item")
    item_type = str(item.get("type") or "") if isinstance(item, Mapping) else ""
    tool_activity = item_type in _TOOL_TYPES or any(
        marker in event_type for marker in ("function_call", "tool_call")
    )
    output_emitted = tool_activity
    if event_type.endswith((".delta", ".done")):
        output_emitted = output_emitted or any(
            marker in event_type
            for marker in ("output_text", "output_image", "image_generation", "reasoning")
        )
    elif any(marker in event_type for marker in ("output_image", "image_generation")):
        output_emitted = True
    part = event.get("part")
    if isinstance(part, Mapping) and part.get("type") in {
        "output_text",
        "output_image",
        "reasoning_text",
        "summary_text",
    }:
        output_emitted = True
    if isinstance(item, Mapping):
        content = item.get("content")
        if isinstance(content, list) and any(
            isinstance(part, Mapping)
            and part.get("type")
            in {"output_text", "output_image", "image", "image_url", "reasoning_text"}
            for part in content
        ):
            output_emitted = True
    return output_emitted, tool_activity


@dataclass
class StreamLifecycle:
    """Small content-free state machine shared by SSE and WebSocket relays."""

    replayable: bool = False
    phase: str = PHASE_CONNECT
    output_emitted: bool = False
    tool_activity: bool = False
    upstream_event_observed: bool = False
    terminal_event_observed: bool = False
    retry_count: int = 0
    _attempt_started_at: float = field(default_factory=time.monotonic, repr=False)
    _first_event_at: Optional[float] = field(default=None, repr=False)
    _open_tool_calls: Set[Tuple[str, str]] = field(default_factory=set, repr=False)
    _anonymous_tool_count: int = field(default=0, repr=False)

    def reset_attempt(self) -> None:
        self.phase = PHASE_CONNECT
        self.output_emitted = False
        self.tool_activity = False
        self.upstream_event_observed = False
        self.terminal_event_observed = False
        self._attempt_started_at = time.monotonic()
        self._first_event_at = None
        self._open_tool_calls.clear()
        self._anonymous_tool_count = 0

    def mark_iterator_created(self) -> None:
        self.phase = PHASE_FIRST_EVENT

    def observe_event(self, event: Mapping[str, Any]) -> None:
        self.upstream_event_observed = True
        if self._first_event_at is None:
            self._first_event_at = time.monotonic()
        visible, tool = event_activity(event)
        self.output_emitted = self.output_emitted or visible
        self.tool_activity = self.tool_activity or tool
        event_type = str(event.get("type") or "")
        item = event.get("item")
        item_type = str(item.get("type") or "") if isinstance(item, Mapping) else ""
        if visible or tool:
            self.phase = PHASE_STREAMING
        # Argument delta/done events prove tool activity but do not open or
        # close a second tool item. Only the Responses item lifecycle owns
        # that state.
        if event_type not in {
            "response.output_item.added",
            "response.output_item.done",
        } or item_type not in _TOOL_TYPES:
            return
        is_done = event_type == "response.output_item.done"
        if is_done:
            key = _event_tool_key(event, item)
            if key in self._open_tool_calls:
                self._open_tool_calls.remove(key)
        else:
            key = _event_tool_key(event, item)
            if key == ("anonymous", "0"):
                self._anonymous_tool_count += 1
                key = ("anonymous", str(self._anonymous_tool_count))
            self._open_tool_calls.add(key)

    def observe_terminal(
        self, event: Mapping[str, Any], terminal: Mapping[str, Any]
    ) -> Dict[str, Any]:
        self.phase = PHASE_TERMINAL
        self.terminal_event_observed = True
        result = dict(terminal)
        if result.get("success") is True and self.has_unfinished_tool(event):
            result.update(
                {
                    "success": False,
                    "status": 502,
                    "error_class": STREAM_INCOMPLETE,
                    "failure_reason": "unfinished_tool_call",
                }
            )
        return result

    def has_unfinished_tool(self, event: Optional[Mapping[str, Any]] = None) -> bool:
        if self._open_tool_calls:
            return True
        if not isinstance(event, Mapping):
            return False
        response = event.get("response")
        response = response if isinstance(response, Mapping) else {}
        output = response.get("output")
        if not isinstance(output, list):
            return False
        for item in output:
            if not isinstance(item, Mapping) or item.get("type") not in _TOOL_TYPES:
                continue
            if item.get("status") not in {"completed", "done"}:
                return True
        return False

    def incomplete(self) -> FailureSnapshot:
        self.phase = PHASE_TERMINAL
        return FailureSnapshot(STREAM_INCOMPLETE, 502, PHASE_TERMINAL, False)

    def diagnostics(self, terminal: Mapping[str, Any]) -> Dict[str, Any]:
        duration_ms = max(
            0, int(round((time.monotonic() - self._attempt_started_at) * 1000))
        )
        first_event_ms = None
        if self._first_event_at is not None:
            first_event_ms = max(
                0, int(round((self._first_event_at - self._attempt_started_at) * 1000))
            )
        return {
            "phase": self.phase,
            "duration_ms": duration_ms,
            "upstream_first_event_ms": first_event_ms,
            "retry_count": self.retry_count,
            "output_emitted": self.output_emitted,
            "tool_activity": self.tool_activity,
            "terminal_event_observed": self.terminal_event_observed,
        }
