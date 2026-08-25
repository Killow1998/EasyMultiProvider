"""Destination-model hierarchical compaction for request-local history.

Codex remains the source of history.  This module only receives a visible,
already assembled request and returns a smaller request for the selected
destination.  The only side effect it permits is a bounded process-memory
cache of successful checkpoint text; no file, database, rollout, or network
operation lives here.
"""

from __future__ import annotations

import copy
import dataclasses
import hashlib
import json
import threading
import time
from collections import OrderedDict
from collections.abc import Mapping as ABCMapping
from dataclasses import dataclass
from typing import Any, Dict, Iterable, List, Mapping, Optional, Protocol, Sequence, Tuple

from .context_guard import estimate_json_tokens


_CHECKPOINT_PREFIX = (
    "Portable checkpoint from Codex-visible local history. Continue from this "
    "state without repeating completed work."
)
_MAP_PROMPT = """Create a structured portable checkpoint from the visible history below.
Include only visible facts: objective, user constraints, decisions, completed work,
files and code changed, relevant tool results, current state, failures, and remaining
steps. Preserve important identifiers and facts. Do not include hidden reasoning or
invented details. Return only the checkpoint text."""
_REDUCE_PROMPT = """Merge the visible portable checkpoints and any visible history below
into one structured portable checkpoint. Preserve objective, user constraints,
decisions, completed work, files and code changed, relevant tool results, current
state, failures, and remaining steps. Do not include hidden reasoning or invented
details. Return only the checkpoint text."""
_USER_MESSAGE_TYPE = "message"
_TOOL_CALL_KINDS = frozenset(
    {"tool_call", "tool_use", "function_call", "custom_tool_call", "command_call"}
)
_TOOL_RESULT_KINDS = frozenset(
    {
        "tool_result",
        "tool_output",
        "tool_return",
        "function_call_output",
        "custom_tool_call_output",
        "function_result",
        "command_result",
    }
)
_HIDDEN_KINDS = frozenset(
    {
        "analysis",
        "chain_of_thought",
        "hidden_cot",
        "hidden_reasoning",
        "reasoning",
        "thinking",
    }
)

COMPACTION_NO_COMPACTION = "no_compaction"
COMPACTION_COMPACTED = "compacted"
COMPACTION_FAIL_CLOSED = "fail_closed"


class HistoryCompactionError(Exception):
    """Content-free, fail-closed compaction failure."""

    _REASONS = frozenset(
        {"history_compaction_failed", "compaction_unit_too_large"}
    )

    def __init__(self, reason: str = "history_compaction_failed") -> None:
        safe = str(reason or "history_compaction_failed").strip().lower()
        self.reason = safe if safe in self._REASONS else "history_compaction_failed"
        super().__init__(self.reason)


@dataclass(frozen=True)
class CompactionMetrics:
    """Content-free counters and timings safe to expose to diagnostics."""

    status: str
    reason: Optional[str]
    safe_input_budget: Optional[int]
    estimated_input_before: Optional[int]
    estimated_input_after: Optional[int]
    source_units: int
    mapped_units: int
    retained_units: int
    active_items: int
    map_calls: int
    reduce_calls: int
    cache_hit: bool
    elapsed_ms: float

    def to_safe_dict(self) -> Dict[str, Any]:
        return {
            "status": self.status,
            "reason": self.reason,
            "safe_input_budget": self.safe_input_budget,
            "estimated_input_before": self.estimated_input_before,
            "estimated_input_after": self.estimated_input_after,
            "source_units": self.source_units,
            "mapped_units": self.mapped_units,
            "retained_units": self.retained_units,
            "active_items": self.active_items,
            "map_calls": self.map_calls,
            "reduce_calls": self.reduce_calls,
            "cache_hit": self.cache_hit,
            "elapsed_ms": self.elapsed_ms,
        }


@dataclass(frozen=True)
class CompactionResult:
    """Transient compactor result; ``body`` is absent on fail-closed."""

    status: str
    body: Optional[Dict[str, Any]]
    reason: Optional[str]
    metrics: CompactionMetrics

    @property
    def payload(self) -> Optional[Dict[str, Any]]:
        return self.body

    def to_safe_dict(self) -> Dict[str, Any]:
        """Expose only the content-free diagnostic surface."""

        return self.metrics.to_safe_dict()

    def __repr__(self) -> str:
        return "CompactionResult(status=%r, reason=%r)" % (
            self.status,
            self.reason,
        )


@dataclass
class _MetricState:
    source_units: int = 0
    mapped_units: int = 0
    retained_units: int = 0
    active_items: int = 0
    map_calls: int = 0
    reduce_calls: int = 0
    cache_hit: bool = False


@dataclass(frozen=True)
class HistoryUnit:
    """A complete turn-sized unit that is never split by map or tail packing."""

    items: Tuple[Any, ...]
    turn_id: Optional[str] = None

    def __repr__(self) -> str:
        return "HistoryUnit(item_count=%d, turn_id=%r)" % (
            len(self.items),
            self.turn_id,
        )


@dataclass(frozen=True)
class SummaryRequest:
    """Transient input passed to the main-thread destination summarizer.

    ``body`` is a Responses-shaped, protocol-neutral request view.  The
    injected adapter owns translation, credentials, transport, and terminal
    validation.  It must not call the history preparation callback again.
    """

    provider: Mapping[str, Any]
    model: Mapping[str, Any]
    protocol: str
    body: Mapping[str, Any]
    stage: str
    safe_input_budget: int
    output_limit: int
    source_fingerprint: str

    @property
    def payload(self) -> Mapping[str, Any]:
        """Compatibility alias for adapters that call request payloads that way."""

        return self.body

    def __repr__(self) -> str:
        return (
            "SummaryRequest(protocol=%r, stage=%r, safe_input_budget=%d, "
            "output_limit=%d)"
            % (self.protocol, self.stage, self.safe_input_budget, self.output_limit)
        )


class DestinationSummarizer(Protocol):
    """Small non-recursive callback implemented by the main request thread."""

    def __call__(self, request: SummaryRequest) -> Any:
        """Run one no-tools destination summary request and return its result."""


@dataclass(frozen=True)
class CheckpointCacheKey:
    """Content-free identity for one reusable checkpoint."""

    source_boundary_fingerprint: str
    visible_prefix_fingerprint: str
    destination_fingerprint: str
    safe_input_budget: int


def _json_default(value: Any) -> Any:
    if dataclasses.is_dataclass(value) and not isinstance(value, type):
        return dataclasses.asdict(value)
    if isinstance(value, (bytes, bytearray)):
        return {
            "byte_length": len(value),
            "sha256": hashlib.sha256(bytes(value)).hexdigest(),
        }
    if isinstance(value, (set, frozenset)):
        return sorted(value, key=repr)
    return repr(value)


def _fingerprint(value: Any) -> str:
    try:
        encoded = json.dumps(
            value,
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
            default=_json_default,
        ).encode("utf-8")
    except Exception:
        raise HistoryCompactionError() from None
    return "sha256:" + hashlib.sha256(encoded).hexdigest()


def source_fingerprint(source_boundary: Any, visible_prefix: Any) -> str:
    """Return a content-free fingerprint for a Codex boundary and prefix."""

    return _fingerprint(
        {"source_boundary": source_boundary, "visible_prefix": visible_prefix}
    )


def destination_fingerprint(
    provider: Mapping[str, Any],
    model: Mapping[str, Any],
    protocol: str,
    safe_input_budget: int,
) -> str:
    """Return a capability fingerprint without copying credentials into a key."""

    provider_fields = (
        "id",
        "base_url",
        "endpoint",
        "api_base",
        "deployment",
        "deployment_id",
        "auth_mode",
    )
    model_fields = (
        "id",
        "upstream_id",
        "model",
        "deployment",
        "deployment_id",
    )
    return _fingerprint(
        {
            "provider": {
                field: provider.get(field)
                for field in provider_fields
                if provider.get(field) is not None
            },
            "model": {
                field: model.get(field)
                for field in model_fields
                if model.get(field) is not None
            },
            "protocol": protocol,
            "safe_input_budget": safe_input_budget,
        }
    )


class MemoryCheckpointCache:
    """Bounded, rebuildable process-memory cache of checkpoint text only."""

    def __init__(self, max_entries: int = 32) -> None:
        if isinstance(max_entries, bool) or not isinstance(max_entries, int):
            max_entries = 32
        self.max_entries = max(1, max_entries)
        self._entries = OrderedDict()  # type: OrderedDict[CheckpointCacheKey, str]
        self._lock = threading.RLock()

    def get(self, key: CheckpointCacheKey) -> Optional[str]:
        with self._lock:
            value = self._entries.pop(key, None)
            if value is None:
                return None
            self._entries[key] = value
            return value

    def put(self, key: CheckpointCacheKey, value: str) -> None:
        if not isinstance(value, str) or not value.strip():
            return
        with self._lock:
            self._entries.pop(key, None)
            self._entries[key] = value
            while len(self._entries) > self.max_entries:
                self._entries.popitem(last=False)

    def __len__(self) -> int:
        with self._lock:
            return len(self._entries)


def _kind(item: Any) -> str:
    if not isinstance(item, ABCMapping):
        return ""
    value = item.get("kind") or item.get("type") or ""
    return str(value).strip().lower().replace("-", "_").replace(" ", "_")


def _turn_id(item: Any) -> Optional[str]:
    if not isinstance(item, ABCMapping):
        return None
    value = item.get("turn_id", item.get("turnId"))
    return value.strip() if isinstance(value, str) and value.strip() else None


def _call_id(item: Any) -> Optional[str]:
    if not isinstance(item, ABCMapping):
        return None
    value = item.get("call_id", item.get("callId"))
    return value.strip() if isinstance(value, str) and value.strip() else None


def _tool_role(item: Any) -> Optional[str]:
    kind = _kind(item)
    if kind in _TOOL_CALL_KINDS or kind.endswith("_call"):
        return "call"
    if kind in _TOOL_RESULT_KINDS or kind.endswith("_result"):
        return "result"
    return None


def _is_user_item(item: Any) -> bool:
    if not isinstance(item, ABCMapping):
        return False
    if item.get("role") == "user":
        return True
    kind = _kind(item)
    if kind in {"user_message", "user_input"}:
        return True
    if kind == "message":
        return item.get("role") == "user"
    return False


def _visible_candidate(item: Any) -> bool:
    if not isinstance(item, ABCMapping):
        return True
    if item.get("visible") is False or item.get("hidden") is True:
        return False
    kind = _kind(item)
    if kind in _HIDDEN_KINDS:
        return False
    if "reasoning" in kind or "chain_of_thought" in kind:
        return item.get("visible") is True or item.get("visibility") == "visible"
    return True


def split_atomic_units(items: Iterable[Any]) -> Tuple[HistoryUnit, ...]:
    """Split visible items at complete turns while keeping tool pairs intact.

    Codex-normalized history normally supplies ``turn_id``.  The user-facing
    request body does not always expose it, so a user-message boundary is the
    conservative fallback.  An open tool call prevents a boundary until its
    matching result is seen.
    """

    current: List[Any] = []
    current_turn: Optional[str] = None
    open_calls = set()
    result: List[HistoryUnit] = []

    def flush() -> None:
        nonlocal current, current_turn, open_calls
        if current:
            result.append(HistoryUnit(tuple(current), current_turn))
        current = []
        current_turn = None
        open_calls = set()

    for raw in items:
        item = copy.deepcopy(raw)
        item_turn = _turn_id(item)
        boundary = False
        if current:
            if item_turn is not None and current_turn is not None:
                boundary = item_turn != current_turn
            elif _is_user_item(item) and any(_is_user_item(value) for value in current):
                boundary = True
            if boundary and open_calls:
                call_id = _call_id(item)
                boundary = not (
                    _tool_role(item) == "result"
                    and call_id is not None
                    and call_id in open_calls
                )
            if boundary:
                flush()
        if current_turn is None and item_turn is not None:
            current_turn = item_turn
        current.append(item)
        role = _tool_role(item)
        call_id = _call_id(item)
        if role == "call" and call_id is not None:
            open_calls.add(call_id)
        elif role == "result" and call_id is not None:
            open_calls.discard(call_id)
    flush()
    return tuple(result)


def _flatten(units: Iterable[HistoryUnit]) -> List[Any]:
    result: List[Any] = []
    for unit in units:
        result.extend(copy.deepcopy(list(unit.items)))
    return result


def _estimate(value: Any) -> int:
    estimate = estimate_json_tokens(value)
    if estimate is None:
        raise HistoryCompactionError() from None
    return estimate


def _message(text: str) -> Dict[str, Any]:
    return {
        "type": _USER_MESSAGE_TYPE,
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def _checkpoint_message(summary: str) -> Dict[str, Any]:
    return _message(_CHECKPOINT_PREFIX + "\n\n" + summary)


def _input_view(body: Mapping[str, Any], items: Sequence[Any]) -> Dict[str, Any]:
    projected = copy.deepcopy(dict(body))
    projected["input"] = copy.deepcopy(list(items))
    return {
        key: projected[key]
        for key in ("input", "instructions", "tools", "text", "response_format")
        if key in projected
    }


def _body_items(body: Mapping[str, Any]) -> List[Any]:
    value = body.get("input")
    if isinstance(value, list):
        return copy.deepcopy(value)
    if isinstance(value, (ABCMapping, str)):
        return [copy.deepcopy(value)]
    return []


def _positive_int(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        value = int(value)
    except (TypeError, ValueError):
        return None
    return value if value > 0 else None


def _extract_summary(value: Any) -> str:
    """Extract text from common adapter result shapes without echoing content."""

    if isinstance(value, bytes):
        try:
            value = value.decode("utf-8")
        except UnicodeDecodeError:
            raise HistoryCompactionError() from None
    if isinstance(value, str):
        text = value.strip()
        if text:
            return text
        raise HistoryCompactionError()
    if isinstance(value, (list, tuple)):
        parts = []
        for item in value:
            try:
                part = _extract_summary(item)
            except HistoryCompactionError:
                continue
            if part:
                parts.append(part)
        if parts:
            return "\n".join(parts)
        raise HistoryCompactionError()
    if not isinstance(value, ABCMapping):
        raise HistoryCompactionError()
    status = value.get("status")
    if status in {"failed", "incomplete"}:
        raise HistoryCompactionError()
    for key in ("output_text", "text", "summary", "checkpoint", "content"):
        if key not in value:
            continue
        nested = value.get(key)
        if isinstance(nested, str) and nested.strip():
            return nested.strip()
        if isinstance(nested, ABCMapping):
            try:
                return _extract_summary(nested)
            except HistoryCompactionError:
                pass
    output = value.get("output")
    if isinstance(output, (list, tuple)):
        parts = []
        for item in output:
            try:
                part = _extract_summary(item)
            except HistoryCompactionError:
                continue
            if part:
                parts.append(part)
        if parts:
            return "\n".join(parts)
    if any(
        key in value
        for key in (
            "objective",
            "user_constraints",
            "decisions",
            "completed_work",
            "remaining_steps",
        )
    ):
        try:
            text = json.dumps(
                dict(value), ensure_ascii=False, sort_keys=True, separators=(",", ":")
            ).strip()
        except Exception:
            raise HistoryCompactionError() from None
        if text:
            return text
    raise HistoryCompactionError()


class HistoryCompactor:
    """Map/reduce visible history through an injected destination callback."""

    def __init__(
        self,
        summarizer: Optional[DestinationSummarizer] = None,
        *,
        cache: Optional[MemoryCheckpointCache] = None,
        output_limit: Optional[int] = None,
    ) -> None:
        self.summarizer = summarizer
        self.cache = cache if cache is not None else MemoryCheckpointCache()
        self.output_limit = _positive_int(output_limit)

    def _summary_output_limit(
        self, model: Mapping[str, Any], body: Mapping[str, Any], safe_budget: int
    ) -> int:
        configured = self.output_limit
        if configured is None:
            for source in (body, model):
                for field in (
                    "max_output_tokens",
                    "max_tokens",
                    "max_completion_tokens",
                    "output_limit",
                    "output_token_limit",
                ):
                    configured = _positive_int(source.get(field))
                    if configured is not None:
                        break
                if configured is not None:
                    break
        if configured is None:
            configured = 1024
        # Reserve this before chunk sizing.  A compact checkpoint should not
        # consume the entire target context just because the model advertises
        # a very large completion limit.
        return max(1, min(configured, max(1, safe_budget // 8)))

    def _summary_body(
        self,
        model: Mapping[str, Any],
        requested_slug: str,
        units: Sequence[HistoryUnit],
        prompt: str,
        output_limit: int,
    ) -> Dict[str, Any]:
        model_id = model.get("upstream_id") or model.get("id") or requested_slug
        return {
            "model": model_id,
            "input": _flatten(units) + [_message(prompt)],
            "stream": False,
            "tools": [],
            "max_output_tokens": output_limit,
        }

    def _invoke(
        self,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        protocol: str,
        requested_slug: str,
        units: Sequence[HistoryUnit],
        stage: str,
        prompt: str,
        safe_budget: int,
        output_limit: int,
        source_fp: str,
        metric_state: _MetricState,
    ) -> str:
        if self.summarizer is None:
            raise HistoryCompactionError()
        if stage == "map":
            metric_state.map_calls += 1
        else:
            metric_state.reduce_calls += 1
        request_body = self._summary_body(
            model, requested_slug, units, prompt, output_limit
        )
        if _estimate(request_body) > max(1, safe_budget - output_limit):
            raise HistoryCompactionError("compaction_unit_too_large")
        request = SummaryRequest(
            provider=copy.deepcopy(dict(provider)),
            model=copy.deepcopy(dict(model)),
            protocol=protocol,
            body=request_body,
            stage=stage,
            safe_input_budget=safe_budget,
            output_limit=output_limit,
            source_fingerprint=source_fp,
        )
        try:
            callback = getattr(self.summarizer, "summarize", None)
            raw = callback(request) if callable(callback) else self.summarizer(request)
        except HistoryCompactionError:
            raise
        except Exception:
            # The adapter's error details never cross the content-free history
            # boundary and the deterministic failure is not retried.
            raise HistoryCompactionError() from None
        return _extract_summary(raw)

    def _pack(
        self,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        requested_slug: str,
        units: Sequence[HistoryUnit],
        prompt: str,
        safe_budget: int,
        output_limit: int,
        oversize_reason: str = "compaction_unit_too_large",
    ) -> Tuple[Tuple[HistoryUnit, ...], ...]:
        input_budget = safe_budget - output_limit
        if input_budget <= 0:
            raise HistoryCompactionError(oversize_reason)
        chunks: List[Tuple[HistoryUnit, ...]] = []
        current: List[HistoryUnit] = []
        for unit in units:
            candidate = tuple(current + [unit])
            request_body = self._summary_body(
                model, requested_slug, candidate, prompt, output_limit
            )
            if _estimate(request_body) <= input_budget:
                current.append(unit)
                continue
            if current:
                chunks.append(tuple(current))
                current = [unit]
                single_body = self._summary_body(
                    model, requested_slug, tuple(current), prompt, output_limit
                )
                if _estimate(single_body) > input_budget:
                    raise HistoryCompactionError(oversize_reason)
                continue
            raise HistoryCompactionError(oversize_reason)
        if current:
            chunks.append(tuple(current))
        return tuple(chunks)

    def _map_reduce(
        self,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        protocol: str,
        requested_slug: str,
        units: Sequence[HistoryUnit],
        safe_budget: int,
        output_limit: int,
        source_fp: str,
        metric_state: _MetricState,
    ) -> str:
        map_chunks = self._pack(
            provider,
            model,
            requested_slug,
            units,
            _MAP_PROMPT,
            safe_budget,
            output_limit,
        )
        summaries = [
            self._invoke(
                provider,
                model,
                protocol,
                requested_slug,
                chunk,
                "map",
                _MAP_PROMPT,
                safe_budget,
                output_limit,
                source_fp,
                metric_state,
            )
            for chunk in map_chunks
        ]
        while len(summaries) > 1:
            reduce_units = tuple(
                HistoryUnit((_message(summary),)) for summary in summaries
            )
            reduce_chunks = self._pack(
                provider,
                model,
                requested_slug,
                reduce_units,
                _REDUCE_PROMPT,
                safe_budget,
                output_limit,
                "history_compaction_failed",
            )
            if len(reduce_chunks) >= len(summaries):
                raise HistoryCompactionError()
            summaries = [
                self._invoke(
                    provider,
                    model,
                    protocol,
                    requested_slug,
                    chunk,
                    "reduce",
                    _REDUCE_PROMPT,
                    safe_budget,
                    output_limit,
                    source_fp,
                    metric_state,
                )
                for chunk in reduce_chunks
            ]
        if not summaries:
            raise HistoryCompactionError()
        return summaries[0]

    def _final_body(
        self,
        body: Mapping[str, Any],
        prefix_items: Sequence[Any],
        summary: Optional[str],
        tail_units: Sequence[HistoryUnit],
        active_request: Sequence[Any],
        suffix_items: Sequence[Any],
    ) -> Dict[str, Any]:
        items = list(copy.deepcopy(list(prefix_items)))
        if summary is not None:
            items.append(_checkpoint_message(summary))
        items.extend(_flatten(tail_units))
        items.extend(copy.deepcopy(list(active_request)))
        items.extend(copy.deepcopy(list(suffix_items)))
        projected = copy.deepcopy(dict(body))
        projected["input"] = items
        return projected

    def _compact_payload(
        self,
        *,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        protocol: str,
        requested_slug: str,
        body: Mapping[str, Any],
        safe_budget: int,
        candidate_items: Sequence[Any],
        active_request: Sequence[Any],
        prefix_items: Sequence[Any] = (),
        suffix_items: Sequence[Any] = (),
        source_boundary: Any = None,
        metric_state: _MetricState,
    ) -> Dict[str, Any]:
        """Compact older candidate units while retaining the active request verbatim."""

        if _positive_int(safe_budget) is None:
            raise HistoryCompactionError()
        visible_candidates = [
            copy.deepcopy(item) for item in candidate_items if _visible_candidate(item)
        ]
        candidate_units = split_atomic_units(visible_candidates)
        active = tuple(copy.deepcopy(list(active_request)))
        metric_state.source_units = len(candidate_units)
        metric_state.active_items = len(active)
        output_limit = self._summary_output_limit(model, body, safe_budget)
        source_fp = source_fingerprint(
            source_boundary,
            {"candidate": visible_candidates, "active_request": list(active)},
        )
        cache_key = CheckpointCacheKey(
            source_boundary_fingerprint=_fingerprint(source_boundary),
            visible_prefix_fingerprint=_fingerprint(
                {"candidate": visible_candidates, "active_request": list(active)}
            ),
            destination_fingerprint=destination_fingerprint(
                provider, model, protocol, safe_budget
            ),
            safe_input_budget=safe_budget,
        )

        active_only = self._final_body(
            body,
            prefix_items,
            None,
            (),
            active,
            suffix_items,
        )
        if _estimate(_input_view(active_only, active_only.get("input", []))) > safe_budget:
            raise HistoryCompactionError("compaction_unit_too_large")

        placeholder = "x" * max(1, output_limit * 2)
        tail_units: List[HistoryUnit] = []
        for unit in reversed(candidate_units):
            tentative = [unit] + tail_units
            projected = self._final_body(
                body,
                prefix_items,
                placeholder,
                tentative,
                active,
                suffix_items,
            )
            if _estimate(_input_view(projected, projected.get("input", []))) <= safe_budget:
                tail_units.insert(0, unit)
                continue
            break

        map_units = candidate_units[: len(candidate_units) - len(tail_units)]
        metric_state.mapped_units = len(map_units)
        metric_state.retained_units = len(tail_units)
        summary = self.cache.get(cache_key) if map_units else None
        if summary is not None:
            metric_state.cache_hit = True
        if map_units and summary is None:
            summary = self._map_reduce(
                provider,
                model,
                protocol,
                requested_slug,
                map_units,
                safe_budget,
                output_limit,
                source_fp,
                metric_state,
            )

        projected = self._final_body(
            body,
            prefix_items,
            summary,
            tail_units,
            active,
            suffix_items,
        )
        if _estimate(_input_view(projected, projected.get("input", []))) > safe_budget:
            raise HistoryCompactionError()
        if map_units and summary is not None:
            self.cache.put(cache_key, summary)
        return projected

    @staticmethod
    def _metrics(
        *,
        status: str,
        reason: Optional[str],
        safe_budget: Optional[int],
        before: Optional[int],
        after: Optional[int],
        state: _MetricState,
        started: float,
    ) -> CompactionMetrics:
        return CompactionMetrics(
            status=status,
            reason=reason,
            safe_input_budget=safe_budget,
            estimated_input_before=before,
            estimated_input_after=after,
            source_units=state.source_units,
            mapped_units=state.mapped_units,
            retained_units=state.retained_units,
            active_items=state.active_items,
            map_calls=state.map_calls,
            reduce_calls=state.reduce_calls,
            cache_hit=state.cache_hit,
            elapsed_ms=round(max(0.0, (time.monotonic() - started) * 1000.0), 3),
        )

    def no_compaction(
        self,
        body: Mapping[str, Any],
        safe_budget: Optional[int],
        estimated_input: Optional[int] = None,
    ) -> CompactionResult:
        """Return an explicit no-op result for the ordinary fast path."""

        started = time.monotonic()
        state = _MetricState()
        if estimated_input is None:
            try:
                estimated_input = _estimate(_input_view(body, _body_items(body)))
            except HistoryCompactionError:
                metrics = self._metrics(
                    status=COMPACTION_FAIL_CLOSED,
                    reason="history_compaction_failed",
                    safe_budget=safe_budget,
                    before=None,
                    after=None,
                    state=state,
                    started=started,
                )
                return CompactionResult(
                    COMPACTION_FAIL_CLOSED,
                    None,
                    "history_compaction_failed",
                    metrics,
                )
        metrics = self._metrics(
            status=COMPACTION_NO_COMPACTION,
            reason=None,
            safe_budget=safe_budget,
            before=estimated_input,
            after=estimated_input,
            state=state,
            started=started,
        )
        return CompactionResult(
            COMPACTION_NO_COMPACTION,
            copy.deepcopy(dict(body)),
            None,
            metrics,
        )

    def compact(
        self,
        *,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        protocol: str,
        requested_slug: str,
        body: Mapping[str, Any],
        safe_budget: int,
        candidate_items: Sequence[Any],
        active_request: Sequence[Any],
        prefix_items: Sequence[Any] = (),
        suffix_items: Sequence[Any] = (),
        source_boundary: Any = None,
    ) -> CompactionResult:
        """Return a classified result without performing transport itself."""

        started = time.monotonic()
        state = _MetricState()
        before: Optional[int] = None
        try:
            before = _estimate(_input_view(body, _body_items(body)))
            if _positive_int(safe_budget) is not None and before <= safe_budget:
                metrics = self._metrics(
                    status=COMPACTION_NO_COMPACTION,
                    reason=None,
                    safe_budget=safe_budget,
                    before=before,
                    after=before,
                    state=state,
                    started=started,
                )
                return CompactionResult(
                    COMPACTION_NO_COMPACTION,
                    copy.deepcopy(dict(body)),
                    None,
                    metrics,
                )
            projected = self._compact_payload(
                provider=provider,
                model=model,
                protocol=protocol,
                requested_slug=requested_slug,
                body=body,
                safe_budget=safe_budget,
                candidate_items=candidate_items,
                active_request=active_request,
                prefix_items=prefix_items,
                suffix_items=suffix_items,
                source_boundary=source_boundary,
                metric_state=state,
            )
            after = _estimate(_input_view(projected, _body_items(projected)))
            if after > safe_budget:
                raise HistoryCompactionError()
            metrics = self._metrics(
                status=COMPACTION_COMPACTED,
                reason=None,
                safe_budget=safe_budget,
                before=before,
                after=after,
                state=state,
                started=started,
            )
            return CompactionResult(COMPACTION_COMPACTED, projected, None, metrics)
        except HistoryCompactionError as exc:
            metrics = self._metrics(
                status=COMPACTION_FAIL_CLOSED,
                reason=exc.reason,
                safe_budget=safe_budget,
                before=before,
                after=None,
                state=state,
                started=started,
            )
            return CompactionResult(
                COMPACTION_FAIL_CLOSED,
                None,
                exc.reason,
                metrics,
            )
        except Exception:
            metrics = self._metrics(
                status=COMPACTION_FAIL_CLOSED,
                reason="history_compaction_failed",
                safe_budget=safe_budget,
                before=before,
                after=None,
                state=state,
                started=started,
            )
            return CompactionResult(
                COMPACTION_FAIL_CLOSED,
                None,
                "history_compaction_failed",
                metrics,
            )


__all__ = [
    "CheckpointCacheKey",
    "COMPACTION_COMPACTED",
    "COMPACTION_FAIL_CLOSED",
    "COMPACTION_NO_COMPACTION",
    "CompactionMetrics",
    "CompactionResult",
    "DestinationSummarizer",
    "HistoryCompactionError",
    "HistoryCompactor",
    "HistoryUnit",
    "MemoryCheckpointCache",
    "SummaryRequest",
    "destination_fingerprint",
    "source_fingerprint",
    "split_atomic_units",
]
