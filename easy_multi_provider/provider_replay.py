"""Bounded in-memory replay for provider-owned opaque protocol metadata."""

from __future__ import annotations

from collections import OrderedDict
import json
import threading
import time
from typing import Any, Dict, Iterable, Iterator, Mapping, Optional, Tuple


_TOOL_CALL_TYPES = frozenset({"function_call", "custom_tool_call"})
_MAX_EVENT_BYTES = 1024 * 1024
_MAX_IDENTIFIER_BYTES = 512
_MAX_SIGNATURE_BYTES = 128 * 1024


def _bounded_identifier(value: Any) -> str:
    if not isinstance(value, str) or not value:
        return ""
    return value if len(value.encode("utf-8")) <= _MAX_IDENTIFIER_BYTES else ""


def _thought_signature(item: Mapping[str, Any]) -> str:
    extra_content = item.get("extra_content")
    google = extra_content.get("google") if isinstance(extra_content, Mapping) else None
    signature = google.get("thought_signature") if isinstance(google, Mapping) else None
    if not isinstance(signature, str) or not signature:
        return ""
    if len(signature.encode("utf-8")) > _MAX_SIGNATURE_BYTES:
        return ""
    return signature


def _response_items(value: Mapping[str, Any]) -> Iterator[Mapping[str, Any]]:
    item = value.get("item")
    if isinstance(item, Mapping):
        yield item
    output = value.get("output")
    if isinstance(output, list):
        yield from (entry for entry in output if isinstance(entry, Mapping))
    response = value.get("response")
    if isinstance(response, Mapping):
        output = response.get("output")
        if isinstance(output, list):
            yield from (entry for entry in output if isinstance(entry, Mapping))


class ProviderReplayCache:
    """Replay only opaque provider metadata that Codex cannot retain itself.

    This is accounting/protocol state, not conversation history: prompts, model
    output, tool results, and complete response items are never retained.
    """

    def __init__(
        self,
        capacity: int = 512,
        ttl_seconds: float = 7200.0,
        clock=time.monotonic,
    ) -> None:
        self.capacity = max(1, int(capacity))
        self.ttl_seconds = max(1.0, float(ttl_seconds))
        self.clock = clock
        self.lock = threading.RLock()
        self._values: "OrderedDict[Tuple[str, str], Tuple[float, str]]" = OrderedDict()

    def _purge(self, now: float) -> None:
        expired = [key for key, (deadline, _) in self._values.items() if deadline <= now]
        for key in expired:
            self._values.pop(key, None)

    def _remember(self, model_id: Any, call_id: Any, signature: Any) -> None:
        model_id = _bounded_identifier(model_id)
        call_id = _bounded_identifier(call_id)
        if not model_id or not call_id or not isinstance(signature, str) or not signature:
            return
        if len(signature.encode("utf-8")) > _MAX_SIGNATURE_BYTES:
            return
        now = self.clock()
        key = (model_id, call_id)
        with self.lock:
            self._purge(now)
            self._values.pop(key, None)
            self._values[key] = (now + self.ttl_seconds, signature)
            while len(self._values) > self.capacity:
                self._values.popitem(last=False)

    def _lookup(self, model_id: Any, call_id: Any) -> str:
        model_id = _bounded_identifier(model_id)
        call_id = _bounded_identifier(call_id)
        if not model_id or not call_id:
            return ""
        now = self.clock()
        key = (model_id, call_id)
        with self.lock:
            self._purge(now)
            value = self._values.get(key)
            if value is None:
                return ""
            self._values.move_to_end(key)
            return value[1]

    def observe_value(self, model_id: Any, value: Any) -> None:
        if not isinstance(value, Mapping):
            return
        for item in _response_items(value):
            if item.get("type") not in _TOOL_CALL_TYPES:
                continue
            call_id = item.get("call_id") or item.get("id")
            signature = _thought_signature(item)
            if signature:
                self._remember(model_id, call_id, signature)

    def observe_bytes(self, model_id: Any, payload: Any) -> None:
        if not isinstance(payload, (bytes, bytearray)) or len(payload) > _MAX_EVENT_BYTES:
            return
        try:
            value = json.loads(bytes(payload).decode("utf-8"))
        except (UnicodeDecodeError, ValueError):
            return
        self.observe_value(model_id, value)

    def prepare(self, body: Dict[str, Any]) -> Dict[str, Any]:
        """Inject cached provider metadata into matching Codex tool-call items."""

        model_id = body.get("model")
        source = body.get("input")
        if not _bounded_identifier(model_id) or not isinstance(source, list):
            return body
        prepared: Optional[list] = None
        for index, item in enumerate(source):
            if not isinstance(item, Mapping) or item.get("type") not in _TOOL_CALL_TYPES:
                continue
            if _thought_signature(item):
                continue
            call_id = item.get("call_id") or item.get("id")
            signature = self._lookup(model_id, call_id)
            if not signature:
                continue
            if prepared is None:
                prepared = list(source)
            enriched = dict(item)
            extra_content = (
                dict(item["extra_content"])
                if isinstance(item.get("extra_content"), Mapping)
                else {}
            )
            google = (
                dict(extra_content["google"])
                if isinstance(extra_content.get("google"), Mapping)
                else {}
            )
            google["thought_signature"] = signature
            extra_content["google"] = google
            enriched["extra_content"] = extra_content
            prepared[index] = enriched
        if prepared is None:
            return body
        result = dict(body)
        result["input"] = prepared
        return result

    def observe_stream(self, model_id: Any, chunks: Iterable[bytes]) -> Iterator[bytes]:
        """Observe SSE JSON while yielding the original byte stream unchanged."""

        pending = bytearray()
        data_lines = []

        def finish_event() -> None:
            if not data_lines:
                return
            raw = b"\n".join(data_lines)
            data_lines.clear()
            if raw == b"[DONE]" or len(raw) > _MAX_EVENT_BYTES:
                return
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, ValueError):
                return
            self.observe_value(model_id, value)

        try:
            for chunk in chunks:
                if isinstance(chunk, (bytes, bytearray)):
                    pending.extend(chunk)
                    if len(pending) > _MAX_EVENT_BYTES:
                        pending.clear()
                        data_lines.clear()
                    while b"\n" in pending:
                        raw_line, _, remainder = pending.partition(b"\n")
                        pending = bytearray(remainder)
                        line = raw_line.rstrip(b"\r")
                        if not line:
                            finish_event()
                        elif line.startswith(b"data:"):
                            data_lines.append(line[5:].lstrip())
                yield chunk
        finally:
            close = getattr(chunks, "close", None)
            if callable(close):
                close()
