"""Build request-local history projections from Codex-visible rollout items.

Codex owns and persists conversation history. EMP only derives the visible
prefix needed to replace an opaque native compaction item for one request.
"""

from __future__ import annotations

import copy
import dataclasses
from collections.abc import Iterable, Mapping
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


def _tool_family(item: Mapping[str, Any]) -> str:
    raw_type = str(item.get("raw_type") or item.get("type") or "").lower()
    return "custom" if "custom_tool" in raw_type else "function"


def _aborted_output(call: Mapping[str, Any]) -> Dict[str, Any]:
    family = _tool_family(call)
    return {
        "kind": "tool_result",
        "content": {"output": "aborted"},
        "call_id": _call_id(call),
        "turn_id": call.get("turn_id"),
        "raw_type": (
            "custom_tool_call_output"
            if family == "custom"
            else "function_call_output"
        ),
    }


def _normalize_tool_pairs(items: Iterable[Mapping[str, Any]]) -> List[Dict[str, Any]]:
    """Mirror Codex prompt normalization for visible function/custom tools."""

    source = [copy.deepcopy(dict(item)) for item in items]
    calls = set()
    outputs = set()
    for item in source:
        role = _tool_role(item)
        if role is None:
            continue
        call_id = _call_id(item)
        if call_id is None:
            raise PortableCheckpointError()
        key = (_tool_family(item), call_id)
        (calls if role == "call" else outputs).add(key)

    normalized = []
    for item in source:
        role = _tool_role(item)
        if role == "result":
            key = (_tool_family(item), _call_id(item))
            if key not in calls:
                continue
        normalized.append(item)
        if role == "call":
            key = (_tool_family(item), _call_id(item))
            if key not in outputs:
                normalized.append(_aborted_output(item))
    return normalized


def _latest_compaction(items: List[Dict[str, Any]]) -> int:
    boundaries = [
        index for index, item in enumerate(items) if _kind(item) in _COMPACTION_KINDS
    ]
    if not boundaries:
        raise CompactionSummaryMissingError()
    return boundaries[-1]


def build_compaction_replacement(
    persisted_items: Iterable[Any],
) -> Tuple[Dict[str, Any], ...]:
    """Return only the history represented by one opaque compaction item.

    The incoming Codex request already contains the complete active tail. It
    must not be merged with the same post-compaction rollout items again.
    """

    persisted = _items(persisted_items)
    boundary = _latest_compaction(persisted)
    compacted = persisted[boundary]
    content = compacted.get("content")
    has_visible_summary = bool(content.strip()) if isinstance(content, str) else bool(content)
    replacement = [compacted] if has_visible_summary else persisted[:boundary]
    return tuple(_normalize_tool_pairs(replacement))


def build_visible_history(persisted_items: Iterable[Any]) -> Tuple[Dict[str, Any], ...]:
    """Return complete visible history when no active Codex tail is available."""

    persisted = _items(persisted_items)
    boundaries = [
        index for index, item in enumerate(persisted) if _kind(item) in _COMPACTION_KINDS
    ]
    if not boundaries:
        return tuple(_normalize_tool_pairs(persisted))
    boundary = boundaries[-1]
    compacted = persisted[boundary]
    content = compacted.get("content")
    has_visible_summary = bool(content.strip()) if isinstance(content, str) else bool(content)
    if has_visible_summary:
        visible = [compacted, *persisted[boundary + 1 :]]
    else:
        visible = [*persisted[:boundary], *persisted[boundary + 1 :]]
    return tuple(_normalize_tool_pairs(visible))


__all__ = [
    "CompactionSummaryMissingError",
    "PortableCheckpointError",
    "build_compaction_replacement",
    "build_visible_history",
]
