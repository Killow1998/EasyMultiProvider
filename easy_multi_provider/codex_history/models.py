"""Small, content-safe contracts for reading Codex-owned history."""

from __future__ import annotations

import copy
import json
import re
from collections.abc import Mapping
from dataclasses import dataclass, field
from typing import Any, Dict, List, Optional, Protocol, Tuple, runtime_checkable


_MAX_IDENTIFIER = 512
_MAX_MODEL = 512
_CODE = re.compile(r"^[a-z0-9][a-z0-9_.-]{0,63}$")
_HISTORY_MODES = frozenset(("legacy", "paginated"))


def _code(value: Any, fallback: str = "unknown") -> str:
    if isinstance(value, str):
        candidate = value.strip().lower()
        if _CODE.fullmatch(candidate):
            return candidate
    return fallback


def _identifier(value: Any, field: str, optional: bool = True) -> Optional[str]:
    if value is None and optional:
        return None
    if not isinstance(value, str) or not value.strip():
        raise HistoryInputError("invalid_%s" % _code(field, "field"))
    value = value.strip()
    if len(value) > _MAX_IDENTIFIER:
        raise HistoryInputError("oversized_%s" % _code(field, "field"))
    return value


def _model(value: Any) -> Optional[str]:
    if value is None:
        return None
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value or len(value) > _MAX_MODEL:
        return None
    return value


class HistoryError(Exception):
    """Base class for bounded, content-free history failures."""

    error_class = "history_error"
    default_reason = "unavailable"

    def __init__(
        self,
        reason: Optional[str] = None,
        *,
        source: str = "unknown",
        fallback: bool = False,
    ) -> None:
        self.reason = _code(reason or self.default_reason, self.default_reason)
        self.source = _code(source, "unknown")
        self.fallback = bool(fallback)
        self.retryable = False
        super().__init__("codex history %s: %s" % (self.error_class, self.reason))

    def __repr__(self) -> str:
        return "%s(reason=%r, source=%r, fallback=%r)" % (
            type(self).__name__,
            self.reason,
            self.source,
            self.fallback,
        )


class HistoryInputError(HistoryError):
    error_class = "invalid_input"
    default_reason = "invalid_anchor"


class HistoryUnavailableError(HistoryError):
    error_class = "unavailable"
    default_reason = "source_unavailable"


class HistoryUnsupportedError(HistoryError):
    error_class = "unsupported"
    default_reason = "unsupported_source"


class HistoryCorruptError(HistoryError):
    error_class = "corrupt"
    default_reason = "corrupt_source"


class HistoryMismatchError(HistoryError):
    error_class = "mismatch"
    default_reason = "thread_mismatch"


class HistoryAmbiguousError(HistoryError):
    error_class = "ambiguous"
    default_reason = "ambiguous_source"


@dataclass(frozen=True)
class HistoryAnchor:
    """Validated Codex thread/turn/window identity; never stores content."""

    thread_id: Optional[str] = None
    turn_id: Optional[str] = None
    window_id: Optional[str] = None

    def __post_init__(self) -> None:
        object.__setattr__(self, "thread_id", _identifier(self.thread_id, "thread_id"))
        object.__setattr__(self, "turn_id", _identifier(self.turn_id, "turn_id"))
        object.__setattr__(self, "window_id", _identifier(self.window_id, "window_id"))

    @classmethod
    def from_headers(cls, headers: Mapping) -> "HistoryAnchor":
        if not isinstance(headers, Mapping):
            raise HistoryInputError("invalid_headers")

        def header(*names: str) -> Optional[str]:
            values = []
            wanted = {name.lower() for name in names}
            for key, value in headers.items():
                if isinstance(key, str) and key.lower() in wanted:
                    if not isinstance(value, str):
                        raise HistoryInputError("invalid_header")
                    values.append(value)
            distinct = {value.strip() for value in values}
            if len(distinct) > 1:
                raise HistoryMismatchError("conflicting_thread_identity", source="anchor")
            return next(iter(distinct), None)

        raw_metadata = header("x-codex-turn-metadata")
        if raw_metadata:
            try:
                metadata = json.loads(raw_metadata)
            except (TypeError, ValueError):
                raise HistoryInputError("invalid_turn_metadata") from None
            if not isinstance(metadata, Mapping):
                raise HistoryInputError("invalid_turn_metadata")
        else:
            metadata = {}

        header_thread_id = header("thread-id")
        header_session_id = header("session-id")
        if header_thread_id and header_session_id and header_thread_id != header_session_id:
            raise HistoryMismatchError("conflicting_thread_identity", source="anchor")
        metadata_thread_id = metadata.get("thread_id", metadata.get("threadId"))
        if metadata_thread_id is not None:
            metadata_thread_id = _identifier(metadata_thread_id, "thread_id")
            explicit_thread_id = header_thread_id or header_session_id
            if explicit_thread_id is not None and metadata_thread_id != explicit_thread_id:
                raise HistoryMismatchError("conflicting_thread_identity", source="anchor")
        header_window_id = header("x-codex-window-id")
        metadata_window_id = metadata.get("window_id", metadata.get("windowId"))
        if metadata_window_id is not None:
            metadata_window_id = _identifier(metadata_window_id, "window_id")
            if header_window_id is not None and metadata_window_id != header_window_id:
                raise HistoryMismatchError("conflicting_window_identity", source="anchor")
        return cls(
            thread_id=header_thread_id or header_session_id or metadata_thread_id,
            turn_id=metadata.get("turn_id", metadata.get("turnId")),
            window_id=header_window_id or metadata_window_id,
        )

    def __repr__(self) -> str:
        return "HistoryAnchor(thread_id=%r, turn_id=%r, window_id=%r)" % (
            self.thread_id,
            self.turn_id,
            self.window_id,
        )


@dataclass(frozen=True)
class HistoryCursor:
    """Content-free position in a Codex-owned source."""

    kind: str = "legacy"
    thread_id: Optional[str] = None
    rollout_identity: Optional[str] = None
    file_identity: Optional[Tuple[int, int, int]] = None
    byte_boundary: Optional[int] = None
    captured_size: Optional[int] = None
    line_index: Optional[int] = None
    highest_ordinal: Optional[int] = None
    projection_offset: Optional[int] = None

    def __post_init__(self) -> None:
        if self.kind not in _HISTORY_MODES:
            raise HistoryInputError("invalid_history_mode")
        object.__setattr__(self, "thread_id", _identifier(self.thread_id, "thread_id"))
        for field_name in (
            "byte_boundary",
            "captured_size",
            "line_index",
            "highest_ordinal",
            "projection_offset",
        ):
            value = getattr(self, field_name)
            if value is not None and (isinstance(value, bool) or not isinstance(value, int) or value < 0):
                raise HistoryInputError("invalid_cursor_%s" % field_name)
        if self.byte_boundary is not None and self.captured_size is not None:
            if self.byte_boundary > self.captured_size:
                raise HistoryInputError("invalid_cursor_boundary")

    def __repr__(self) -> str:
        return (
            "HistoryCursor(kind=%r, byte_boundary=%r, line_index=%r, "
            "highest_ordinal=%r)"
            % (self.kind, self.byte_boundary, self.line_index, self.highest_ordinal)
        )


@dataclass(frozen=True)
class VisibleItem:
    """One explicitly visible item; content is intentionally omitted from repr."""

    kind: str
    content: Any = field(default=None, repr=False)
    item_id: Optional[str] = None
    turn_id: Optional[str] = None
    call_id: Optional[str] = None
    ordinal: Optional[int] = None
    offset: Optional[int] = None
    raw_type: Optional[str] = field(default=None, repr=False)

    def __post_init__(self) -> None:
        if not isinstance(self.kind, str) or not self.kind.strip():
            raise HistoryInputError("invalid_visible_item_type")
        object.__setattr__(self, "kind", self.kind.strip())
        object.__setattr__(self, "content", copy.deepcopy(self.content))
        object.__setattr__(self, "item_id", _identifier(self.item_id, "item_id"))
        object.__setattr__(self, "turn_id", _identifier(self.turn_id, "turn_id"))
        object.__setattr__(self, "call_id", _identifier(self.call_id, "call_id"))
        for field_name in ("ordinal", "offset"):
            value = getattr(self, field_name)
            if value is not None and (isinstance(value, bool) or not isinstance(value, int) or value < 0):
                raise HistoryInputError("invalid_visible_%s" % field_name)
        object.__setattr__(
            self, "raw_type", self.raw_type if isinstance(self.raw_type, str) else None
        )

    def __repr__(self) -> str:
        return (
            "VisibleItem(kind=%r, item_id=%r, turn_id=%r, call_id=%r, "
            "ordinal=%r)"
            % (self.kind, self.item_id, self.turn_id, self.call_id, self.ordinal)
        )


@dataclass(frozen=True)
class HistorySnapshot:
    """Normalized visible history plus a source cursor and route metadata."""

    anchor: HistoryAnchor = field(default_factory=HistoryAnchor)
    items: Tuple[VisibleItem, ...] = ()
    cursor: HistoryCursor = field(default_factory=HistoryCursor)
    source: str = "unknown"
    source_model: Optional[str] = None
    fallback: bool = False

    def __post_init__(self) -> None:
        if not isinstance(self.anchor, HistoryAnchor):
            raise HistoryInputError("invalid_snapshot_anchor")
        if not isinstance(self.cursor, HistoryCursor):
            raise HistoryInputError("invalid_snapshot_cursor")
        if (
            self.anchor.thread_id
            and self.cursor.thread_id
            and self.anchor.thread_id != self.cursor.thread_id
        ):
            raise HistoryMismatchError("thread_mismatch", source="snapshot")
        normalized_items = tuple(self.items)
        if any(not isinstance(item, VisibleItem) for item in normalized_items):
            raise HistoryInputError("invalid_snapshot_items")
        object.__setattr__(self, "items", normalized_items)
        source = _code(self.source, "unknown")
        object.__setattr__(self, "source", source)
        object.__setattr__(self, "source_model", _model(self.source_model))

    @property
    def history_mode(self) -> str:
        return self.cursor.kind

    @property
    def thread_id(self) -> Optional[str]:
        return self.anchor.thread_id or self.cursor.thread_id

    def __repr__(self) -> str:
        return (
            "HistorySnapshot(source=%r, mode=%r, item_count=%d, source_model=%r, "
            "fallback=%r)"
            % (self.source, self.cursor.kind, len(self.items), self.source_model, self.fallback)
        )


@runtime_checkable
class CodexHistoryReader(Protocol):
    def read_visible_history(self, anchor: HistoryAnchor) -> HistorySnapshot:
        """Read normalized visible items without mutating Codex state."""


def _string(raw: Mapping, *keys: str) -> Optional[str]:
    for key in keys:
        value = raw.get(key)
        if isinstance(value, str) and value:
            return value
    return None


def _integer(raw: Mapping, *keys: str) -> Optional[int]:
    for key in keys:
        if key not in raw or raw.get(key) is None:
            continue
        value = raw.get(key)
        if isinstance(value, bool):
            return None
        if isinstance(value, int):
            return value if value >= 0 else None
        if isinstance(value, str) and value.strip().isdigit():
            return int(value.strip())
        return None
    return None


def _content(raw: Mapping, *keys: str) -> Any:
    for key in keys:
        if key in raw:
            return copy.deepcopy(raw.get(key))
    return None


def _content_map(raw: Mapping, keys: Tuple[str, ...]) -> Dict[str, Any]:
    result = {}
    for key in keys:
        if key in raw:
            result[key] = copy.deepcopy(raw.get(key))
    return result


_HIDDEN_TYPES = frozenset(
    (
        "analysis",
        "chain_of_thought",
        "hidden_reasoning",
        "hidden_cot",
        "reasoning",
        "thinking",
    )
)


def _type_token(value: str) -> str:
    """Normalize Codex protocol item names across snake/camel case versions."""

    value = re.sub(r"(?<=[a-z0-9])(?=[A-Z])", "_", value)
    return value.replace("-", "_").replace(" ", "_").lower()


def normalize_visible_item(
    raw: Any,
    *,
    turn_id: Optional[str] = None,
    ordinal: Optional[int] = None,
    offset: Optional[int] = None,
) -> Optional[VisibleItem]:
    """Normalize the small allowlist of visible Codex item classes.

    Unknown items and hidden reasoning are intentionally ignored.  The
    function never copies an arbitrary raw item into the normalized payload.
    """

    if not isinstance(raw, Mapping):
        return None
    raw_type_value = raw.get("type")
    if not isinstance(raw_type_value, str):
        return None
    if raw.get("visible") is False or raw.get("hidden") is True:
        return None
    raw_type = raw_type_value
    token = _type_token(raw_type)
    if token in _HIDDEN_TYPES:
        return None
    if token == "reasoning_summary":
        if raw.get("visible") is not True and raw.get("visibility") != "visible":
            return None
        token = "assistant_message"
        raw = {"content": _content(raw, "summary", "text", "content"), **dict(raw)}

    if token == "message":
        role = raw.get("role")
        if role not in ("user", "assistant"):
            return None
        kind = "%s_message" % role
        content = _content(raw, "content", "text", "message")
    elif token in ("user_message", "usermessage"):
        kind = "user_message"
        content = _content(raw, "content", "message", "text", "images")
    elif token in ("assistant_message", "assistantmessage", "agent_message", "agentmessage"):
        kind = "assistant_message"
        content = _content(raw, "content", "message", "text")
    elif token in ("function_call", "custom_tool_call", "tool_call"):
        kind = "tool_call"
        content = _content_map(raw, ("name", "arguments", "input", "tool", "status"))
    elif token in (
        "function_call_output",
        "custom_tool_call_output",
        "tool_result",
        "tool_output",
        "mcp_tool_result",
    ):
        standalone = (
            token == "function_call_output"
            and _string(raw, "call_id", "callId") is None
            and _string(raw, "name") is not None
        )
        kind = "standalone_tool_output" if standalone else "tool_result"
        content = _content_map(
            raw,
            (
                "name",
                "namespace",
                "output",
                "result",
                "content",
                "status",
            ),
        )
    elif token in ("mcp_tool_call", "dynamic_tool_call", "collab_agent_tool_call"):
        # App Server exposes these as one display item rather than a Responses
        # call/output pair.  Keep the visible activity without inventing an
        # invalid or unpaired upstream tool call.
        kind = "tool_activity"
        content = _content_map(
            raw,
            (
                "server",
                "tool",
                "arguments",
                "result",
                "error",
                "status",
                "namespace",
                "prompt",
                "receiverThreadIds",
            ),
        )
    elif token in ("command_execution", "command", "shell_command"):
        kind = "command_execution"
        content = _content_map(
            raw,
            (
                "command",
                "cwd",
                "output",
                "result",
                "aggregatedOutput",
                "exit_code",
                "exitCode",
                "status",
            ),
        )
    elif token in (
        "command_execution_output",
        "command_execution_result",
        "command_result",
    ):
        kind = "command_execution_result"
        content = _content_map(raw, ("command", "output", "result", "exit_code", "status"))
    elif token in ("file_operation", "file_change", "file_edit", "file_write"):
        kind = "file_operation"
        content = _content_map(
            raw,
            ("operation", "path", "diff", "changes", "content", "result", "status"),
        )
    elif token in ("file_operation_result", "file_change_result", "file_edit_result"):
        kind = "file_operation_result"
        content = _content_map(raw, ("operation", "path", "result", "output", "status"))
    elif token in ("plan", "plan_update", "update_plan"):
        kind = "plan"
        content = _content(raw, "content", "text", "summary", "plan", "items")
    elif token in ("user_constraint", "constraint"):
        kind = "user_constraint"
        content = _content(raw, "content", "text", "constraint")
    elif token == "decision":
        kind = "decision"
        content = _content(raw, "content", "text", "decision")
    elif token == "progress":
        kind = "progress"
        content = _content(raw, "content", "text", "summary", "progress")
    elif token == "error":
        kind = "error"
        content = _content(raw, "content", "text", "message", "error", "details")
    elif token == "blocker":
        kind = "blocker"
        content = _content(raw, "content", "text", "message", "blocker", "details")
    elif token in (
        "compaction",
        "compaction_summary",
        "compacted",
        "compacted_summary",
    ):
        kind = "compaction_summary"
        content = _content(raw, "message", "summary", "content", "text", "marker")
    elif token in ("compaction_marker", "compaction_boundary", "context_compaction"):
        kind = "compaction_marker"
        content = _content(raw, "marker", "summary", "content", "text")
    elif token in ("image", "image_reference", "input_image", "output_image"):
        kind = "image_reference"
        content = _content_map(
            raw, ("image_url", "url", "file_id", "detail", "media_type", "alt_text")
        )
    else:
        return None

    item_id = _string(raw, "id", "item_id", "itemId")
    resolved_turn_id = turn_id or _string(raw, "turn_id", "turnId")
    resolved_call_id = _string(raw, "call_id", "callId")
    resolved_ordinal = ordinal if ordinal is not None else _integer(
        raw, "ordinal", "rollout_ordinal", "rolloutOrdinal", "sequence"
    )
    resolved_offset = offset if offset is not None else _integer(
        raw, "offset", "projection_offset", "projectionOffset"
    )
    return VisibleItem(
        kind=kind,
        content=content,
        item_id=item_id,
        turn_id=resolved_turn_id,
        call_id=resolved_call_id,
        ordinal=resolved_ordinal,
        offset=resolved_offset,
        raw_type=raw_type,
    )


__all__ = [
    "CodexHistoryReader",
    "HistoryAmbiguousError",
    "HistoryAnchor",
    "HistoryCorruptError",
    "HistoryCursor",
    "HistoryError",
    "HistoryInputError",
    "HistoryMismatchError",
    "HistorySnapshot",
    "HistoryUnavailableError",
    "HistoryUnsupportedError",
    "VisibleItem",
    "normalize_visible_item",
]
