"""Content-free performance measurements for canonical Responses streams."""

from __future__ import annotations

import json
import time
from typing import Any, Dict, Iterable, Iterator, Mapping, Optional

_MAX_EVENT_BYTES = 1024 * 1024
_MIN_TPS_WINDOW_MS = 500
PERFORMANCE_SCHEMA = 2


def _output_tokens(event: Mapping[str, Any]) -> Optional[int]:
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return None
    value = usage.get("output_tokens")
    if isinstance(value, bool):
        return None
    try:
        value = int(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return value if 0 <= value <= 10_000_000 else None


def _reasoning_tokens(event: Mapping[str, Any]) -> Optional[int]:
    response = event.get("response")
    response = response if isinstance(response, Mapping) else event
    usage = response.get("usage")
    if not isinstance(usage, Mapping):
        return None
    details = usage.get("output_tokens_details")
    if not isinstance(details, Mapping):
        return None
    value = details.get("reasoning_tokens")
    if isinstance(value, bool):
        return None
    try:
        value = int(value)
    except (TypeError, ValueError, OverflowError):
        return None
    return value if 0 <= value <= 10_000_000 else None


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
        self._pending = bytearray()
        self._data_lines = []
        self._data_bytes = 0
        self._valid_stream = True
        self._completed = False

    def mark_upstream_started(self, value: Optional[float] = None) -> None:
        self._upstream_started = self._clock() if value is None else float(value)

    def observe_event(self, event: Mapping[str, Any]) -> None:
        if not isinstance(event, Mapping):
            return
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
            value = _output_tokens(event)
            if value is not None:
                self._output_tokens = value
            reasoning = _reasoning_tokens(event)
            if reasoning is not None:
                self._reasoning_tokens = reasoning

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
        result: Dict[str, Any] = {"performance_schema": PERFORMANCE_SCHEMA}
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
