"""Build one request-local view from Codex's latest visible compaction.

Codex owns history. This module neither summarizes nor persists it: it keeps
the latest Codex compaction summary, the completed visible tail after that
summary, and the current request in that order.
"""

from __future__ import annotations

import copy
import dataclasses
from collections.abc import Iterable, Mapping
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Tuple


_COMPACTION_KINDS = frozenset({"compaction_summary", "compaction_marker"})
_HIDDEN_KINDS = frozenset(
    {"analysis", "chain_of_thought", "hidden_cot", "hidden_reasoning", "reasoning", "thinking"}
)
_CALL_KINDS = frozenset({"tool_call", "function_call", "command_call"})
_RESULT_KINDS = frozenset({"tool_result", "function_result", "command_result"})


class PortableCheckpointError(Exception):
    """Content-free failure at the history projection boundary."""

    code = "portable_checkpoint_invalid"

    def __init__(self, *_: Any, **__: Any) -> None:
        super().__init__()

    def __str__(self) -> str:
        return self.code


class CompactionSummaryMissingError(PortableCheckpointError):
    code = "compaction_summary_missing"


class IncompleteToolPairError(PortableCheckpointError):
    code = "checkpoint_tool_pair_incomplete"


@dataclass(frozen=True)
class PortableCheckpoint:
    summary: Optional[Dict[str, Any]]
    retained_tail: Tuple[Dict[str, Any], ...]
    current_request: Tuple[Dict[str, Any], ...]


def _mapping(item: Any) -> Dict[str, Any]:
    if isinstance(item, Mapping):
        return copy.deepcopy(dict(item))
    if dataclasses.is_dataclass(item) and not isinstance(item, type):
        return {
            field.name: copy.deepcopy(getattr(item, field.name))
            for field in dataclasses.fields(item)
        }
    raise PortableCheckpointError()


def _kind(item: Mapping[str, Any]) -> str:
    return str(item.get("kind") or item.get("type") or "").strip().lower().replace("-", "_")


def _visible(item: Mapping[str, Any]) -> bool:
    if item.get("visible") is False or item.get("hidden") is True:
        return False
    kind = _kind(item)
    if kind in _HIDDEN_KINDS:
        return False
    if "reasoning" in kind or "chain_of_thought" in kind:
        return item.get("visible") is True or item.get("visibility") == "visible"
    return True


def _items(source: Iterable[Any]) -> List[Dict[str, Any]]:
    try:
        values = [_mapping(item) for item in source]
    except PortableCheckpointError:
        raise
    except Exception:
        raise PortableCheckpointError() from None
    return [item for item in values if _visible(item)]


def _tool_role(item: Mapping[str, Any]) -> Optional[str]:
    kind = _kind(item)
    if kind in _CALL_KINDS or kind.endswith("_call"):
        return "call"
    if kind in _RESULT_KINDS or kind.endswith("_result"):
        return "result"
    return None


def _call_id(item: Mapping[str, Any]) -> Optional[str]:
    value = item.get("call_id")
    return value.strip() if isinstance(value, str) and value.strip() else None


def _validate_tool_pairs(items: Iterable[Mapping[str, Any]]) -> None:
    open_calls = set()
    completed = set()
    for item in items:
        role = _tool_role(item)
        if role is None:
            continue
        call_id = _call_id(item)
        if call_id is None:
            raise IncompleteToolPairError()
        if role == "call":
            if call_id in open_calls or call_id in completed:
                raise IncompleteToolPairError()
            open_calls.add(call_id)
        else:
            if call_id not in open_calls:
                raise IncompleteToolPairError()
            open_calls.remove(call_id)
            completed.add(call_id)
    if open_calls:
        raise IncompleteToolPairError()


def build_portable_view(
    persisted_items: Iterable[Any], current_request_items: Iterable[Any]
) -> PortableCheckpoint:
    """Return ``Codex summary + completed tail + exact current request``.

    No second summary, clipping, retry, or history cache is permitted here.
    If Codex's visible state cannot be represented exactly, the caller fails
    closed instead of sending a partial context.
    """

    persisted = _items(persisted_items)
    current = _items(current_request_items)
    compacted = [
        index for index, item in enumerate(persisted) if _kind(item) in _COMPACTION_KINDS
    ]
    if not compacted:
        raise CompactionSummaryMissingError()
    boundary = compacted[-1]
    compacted = persisted[boundary]
    content = compacted.get("content")
    has_visible_summary = bool(content.strip()) if isinstance(content, str) else bool(content)
    if has_visible_summary:
        summary = compacted
        tail = persisted[boundary + 1 :]
    else:
        # OpenAI remote compaction may keep only encrypted_content in the live
        # context while the append-only rollout retains the complete visible
        # history. Replaying that visible history is lossless; fabricating a
        # clipped local "summary" would not be.
        summary = None
        tail = persisted[:boundary] + persisted[boundary + 1 :]
    _validate_tool_pairs([*tail, *current])
    return PortableCheckpoint(
        summary=summary,
        retained_tail=tuple(tail),
        current_request=tuple(current),
    )


__all__ = [
    "CompactionSummaryMissingError",
    "IncompleteToolPairError",
    "PortableCheckpoint",
    "PortableCheckpointError",
    "build_portable_view",
]
