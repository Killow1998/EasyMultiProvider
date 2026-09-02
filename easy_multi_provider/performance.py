"""Content-free performance measurements for canonical Responses streams."""

from __future__ import annotations

import json
import re
import time
from typing import Any, Dict, Iterable, Iterator, Mapping, Optional

from .transport_failures import event_activity


_OUTPUT_EVENT = re.compile(
    br'"type"\s*:\s*"response\.(?:output_text|reasoning(?:_summary)?_text|'
    br'function_call_arguments|custom_tool_call_input)\.(?:delta|done)"'
)
_TERMINAL_EVENT = re.compile(
    br'"type"\s*:\s*"response\.(?:completed|incomplete|failed)"'
)
_OUTPUT_TOKENS = re.compile(br'"output_tokens"\s*:\s*(\d+)')
_SCAN_TAIL_BYTES = 512


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
    except (TypeError, ValueError):
        return None
    return value if 0 <= value <= 10_000_000 else None


class ResponsesPerformanceTracker:
    """Measure timing and token counts without retaining request or output text."""

    def __init__(self, started: Optional[float] = None, clock=time.monotonic):
        self._clock = clock
        self._started = clock() if started is None else float(started)
        self._upstream_started: Optional[float] = None
        self._first_token_at: Optional[float] = None
        self._terminal_at: Optional[float] = None
        self._output_tokens: Optional[int] = None
        self._tail = b""

    def mark_upstream_started(self, value: Optional[float] = None) -> None:
        self._upstream_started = self._clock() if value is None else float(value)

    def observe_event(self, event: Mapping[str, Any]) -> None:
        if not isinstance(event, Mapping):
            return
        now = self._clock()
        visible, tool = event_activity(event)
        if (visible or tool) and self._first_token_at is None:
            self._first_token_at = now
        event_type = str(event.get("type") or "")
        if event_type in {
            "response.completed",
            "response.incomplete",
            "response.failed",
        }:
            self._terminal_at = now
            value = _output_tokens(event)
            if value is not None:
                self._output_tokens = value

    def observe_chunk(self, chunk: Any) -> None:
        raw = chunk if isinstance(chunk, bytes) else str(chunk).encode("utf-8")
        scan = self._tail + raw
        now = self._clock()
        if self._first_token_at is None and _OUTPUT_EVENT.search(scan):
            self._first_token_at = now
        if _TERMINAL_EVENT.search(scan):
            self._terminal_at = now
        matches = _OUTPUT_TOKENS.findall(scan)
        if matches:
            value = int(matches[-1])
            if value <= 10_000_000:
                self._output_tokens = value
        self._tail = scan[-_SCAN_TAIL_BYTES:]

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

    def observe_stream(self, result: Iterable[Any]) -> Iterator[Any]:
        try:
            for chunk in result:
                self.observe_chunk(chunk)
                yield chunk
        finally:
            close = getattr(result, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass

    def diagnostics(self) -> Dict[str, Any]:
        result: Dict[str, Any] = {}
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
        if self._first_token_at is not None and self._terminal_at is not None:
            generation_ms = max(
                0, int(round((self._terminal_at - self._first_token_at) * 1000))
            )
            result["generation_ms"] = generation_ms
            if generation_ms > 0 and self._output_tokens:
                result["tokens_per_second"] = round(
                    self._output_tokens * 1000.0 / generation_ms, 2
                )
        return result
