"""Content-free performance measurements for canonical Responses streams."""

from __future__ import annotations

import json
import re
import time
from typing import Any, Dict, Iterable, Iterator, Mapping, Optional

_MAX_EVENT_BYTES = 1024 * 1024
_MIN_TPS_WINDOW_MS = 500
PERFORMANCE_SCHEMA = 2
_MODEL_ID = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._:/@+~-]{0,255}$")


def token_count(value: Any) -> Optional[int]:
    """Accept reported counts, not estimates, clamped values or missing fields."""
    return value if type(value) is int and 0 <= value <= 10_000_000 else None


def declared_response_model(event: Mapping[str, Any]) -> Optional[str]:
    """Return only a protocol-declared model identifier, never response content."""

    if not isinstance(event, Mapping):
        return None
    candidates = [event.get("model")]
    response = event.get("response")
    if isinstance(response, Mapping):
        candidates.append(response.get("model"))
    message = event.get("message")
    if isinstance(message, Mapping):
        candidates.append(message.get("model"))
    for value in candidates:
        if isinstance(value, str) and _MODEL_ID.fullmatch(value):
            return value
    return None


def input_cache_usage(event: Mapping[str, Any]) -> Dict[str, int]:
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return {}
    total = token_count(usage.get("input_tokens"))
    if total is None:
        return {}
    result = {"input_tokens": total}
    details = usage.get("input_tokens_details")
    cached = token_count(details.get("cached_tokens")) if isinstance(details, Mapping) else None
    if cached is not None and cached <= total:
        result["cached_input_tokens"] = cached
    return result


def _output_tokens(event: Mapping[str, Any]) -> Optional[int]:
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return None
    return token_count(usage.get("output_tokens"))


def _reasoning_tokens(event: Mapping[str, Any]) -> Optional[int]:
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return None
    details = usage.get("output_tokens_details")
    if not isinstance(details, Mapping):
        return None
    return token_count(details.get("reasoning_tokens"))


def reported_usage(event: Mapping[str, Any]) -> Dict[str, Any]:
    """Keep billing subsets separate, without adding them to token totals."""
    result = input_cache_usage(event)
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return result
    for name, value in (("output_tokens", token_count(usage.get("output_tokens"))),
                        ("reasoning_tokens", _reasoning_tokens(event))):
        if value is not None:
            result[name] = value
    details = usage.get("input_tokens_details")
    if isinstance(details, Mapping):
        for source, target in (("cache_creation_tokens", "cache_write_tokens"),
                               ("cache_creation_1h_tokens", "cache_write_1h_tokens")):
            if source in details:
                result[target] = token_count(details[source])
    response_id = response.get("id")
    if isinstance(response_id, str) and 0 < len(response_id) <= 256:
        result["usage_response_id"] = response_id
    tier = response.get("service_tier")
    if isinstance(tier, str) and 0 < len(tier) <= 32:
        result["service_tier"] = tier
    return result


def _measured_output_activity(event: Mapping[str, Any]) -> bool:
    """Return activity covered by non-reasoning output token usage."""

    return str(event.get("type") or "") in {
        "response.output_text.delta", "response.refusal.delta",
        "response.function_call_arguments.delta", "response.custom_tool_call_input.delta",
    } and isinstance(event.get("delta"), str) and bool(event["delta"])


class ResponsesPerformanceTracker:
    """Measure timing and token counts without retaining request or output text."""

    def __init__(self, started: Optional[float] = None, clock=time.monotonic):
        self._clock = clock
        self._started = clock() if started is None else float(started)
        self._upstream_started: Optional[float] = None
        self._first_token_at: Optional[float] = None
        self._last_token_at: Optional[float] = None
        self._terminal_at: Optional[float] = None
        self._output_tokens: Optional[int] = None
        self._reasoning_tokens: Optional[int] = None
        self._input_usage: Dict[str, int] = {}
        self._pending = bytearray()
        self._data_lines = []
        self._data_bytes = 0
        self._valid_stream = True
        self._completed = False
        self._response_model: Optional[str] = None

    def mark_upstream_started(self, value: Optional[float] = None) -> None:
        self._upstream_started = self._clock() if value is None else float(value)

    def observe_event(self, event: Mapping[str, Any]) -> None:
        if not isinstance(event, Mapping):
            return
        self.observe_upstream_event(event)
        now = self._clock()
        if self._terminal_at is not None:
            return
        if _measured_output_activity(event):
            if self._first_token_at is None:
                self._first_token_at = now
            self._last_token_at = now
        event_type = str(event.get("type") or "")
        if event_type in {
            "response.completed",
            "response.incomplete",
            "response.failed",
        }:
            self._terminal_at = now
            self._completed = event_type == "response.completed"
            self._input_usage = reported_usage(event)
            value = _output_tokens(event)
            if value is not None:
                self._output_tokens = value
            reasoning = _reasoning_tokens(event)
            if reasoning is not None:
                self._reasoning_tokens = reasoning

    def observe_upstream_event(self, event: Mapping[str, Any]) -> None:
        """Capture the first model declared by the upstream protocol."""

        if self._response_model is None:
            self._response_model = declared_response_model(event)

    def observe_chunk(self, chunk: Any) -> None:
        if not self._valid_stream:
            return
        raw = chunk if isinstance(chunk, bytes) else str(chunk).encode("utf-8")
        # Observe complete SSE events, never regex-match fields inside output.
        # Bound transient JSON buffering; measurement failure must not stop relay.
        for fragment in raw.splitlines(keepends=True):
            if len(self._pending) + self._data_bytes + len(fragment) > _MAX_EVENT_BYTES:
                self._valid_stream = False
                break
            self._pending.extend(fragment)
            if not self._pending.endswith(b"\n"):
                continue
            line = bytes(self._pending).rstrip(b"\r\n")
            self._pending.clear()
            if line.startswith(b"data:"):
                value = line[5:].lstrip(b" ")
                self._data_lines.append(value)
                self._data_bytes += len(value) + 1
            elif not line and self._data_lines:
                data = b"\n".join(self._data_lines)
                self._data_lines.clear()
                self._data_bytes = 0
                if data == b"[DONE]":
                    continue
                try:
                    self.observe_event(json.loads(data))
                except (ValueError, UnicodeDecodeError, RecursionError):
                    self._valid_stream = False
                    break
        if not self._valid_stream:
            self._pending.clear()
            self._data_lines.clear()
            self._data_bytes = 0

    def observe_bytes(self, value: Any) -> None:
        raw = value if isinstance(value, bytes) else bytes(value or b"")
        try:
            payload = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError, TypeError):
            self.observe_chunk(raw)
            return
        if isinstance(payload, Mapping):
            self.observe_upstream_event(payload)
            self._input_usage = reported_usage(payload)
            count = _output_tokens(payload)
            if count is not None:
                self._output_tokens = count
            reasoning = _reasoning_tokens(payload)
            if reasoning is not None:
                self._reasoning_tokens = reasoning

    def observe_stream(self, result: Iterable[Any]) -> Iterator[Any]:
        try:
            for chunk in result:
                self.observe_chunk(chunk)
                yield chunk
        finally:
            self._pending.clear()
            self._data_lines.clear()
            self._data_bytes = 0
            close = getattr(result, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass

    def diagnostics(self) -> Dict[str, Any]:
        result: Dict[str, Any] = {"performance_schema": PERFORMANCE_SCHEMA, **self._input_usage}
        if self._response_model is not None:
            result["response_model"] = self._response_model
            result["response_model_source"] = "upstream_response"
        if self._first_token_at is not None:
            result["ttft_ms"] = max(
                0, int(round((self._first_token_at - self._started) * 1000))
            )
            if self._upstream_started is not None:
                result["upstream_first_token_ms"] = max(
                    0,
                    int(
                        round(
                            (self._first_token_at - self._upstream_started) * 1000
                        )
                    ),
                )
        if self._output_tokens is not None:
            result["output_tokens"] = self._output_tokens
        if self._first_token_at is not None and self._last_token_at is not None:
            generation_ms = max(
                0, int(round((self._last_token_at - self._first_token_at) * 1000))
            )
            result["generation_ms"] = generation_ms
            measured_tokens = None
            if (
                self._output_tokens is not None
                and self._reasoning_tokens is not None
            ):
                measured_tokens = self._output_tokens - self._reasoning_tokens - 1
            # A short answer can arrive in one buffered burst immediately before
            # the terminal event. Dividing by that sub-second delivery gap
            # reports transport batching as model generation speed. Keep TTFT,
            # but only publish TPS when the observed output window is long
            # enough to form a useful rate.
            if (self._valid_stream and self._completed
                    and generation_ms >= _MIN_TPS_WINDOW_MS
                    and measured_tokens is not None and measured_tokens > 0):
                result["tokens_per_second"] = round(
                    measured_tokens * 1000.0 / generation_ms, 2
                )
        return result
