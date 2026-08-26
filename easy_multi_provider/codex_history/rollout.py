"""Bounded, read-only normalization of Codex rollout JSONL files."""

from __future__ import annotations

import json
import os
import stat
from collections.abc import Mapping
from dataclasses import dataclass, replace
from pathlib import Path
from typing import Any, List, Optional, Tuple

from .models import (
    HistoryAmbiguousError,
    HistoryAnchor,
    HistoryCorruptError,
    HistoryCursor,
    HistoryError,
    HistoryMismatchError,
    HistorySnapshot,
    HistoryUnavailableError,
    HistoryUnsupportedError,
    normalize_visible_item,
)


_MAX_ROLLOUT_BYTES = 128 * 1024 * 1024
_ORDINAL_KEYS = ("ordinal", "rollout_ordinal", "rolloutOrdinal", "sequence")
_OFFSET_KEYS = ("projection_offset", "projectionOffset", "offset")


@dataclass(frozen=True)
class _ParsedRecord:
    value: Mapping
    start: int
    end: int
    line_index: int


def _token(value: Any) -> str:
    if not isinstance(value, str):
        return ""
    return value.replace("-", "_").replace(" ", "_").lower()


def _string(mapping: Mapping, *keys: str) -> Optional[str]:
    for key in keys:
        value = mapping.get(key)
        if isinstance(value, str) and value.strip():
            return value.strip()
    return None


def _model(value: Any) -> Optional[str]:
    if not isinstance(value, str):
        return None
    value = value.strip()
    if not value or len(value) > 512:
        return None
    return value


def _integer(mapping: Mapping, keys: Tuple[str, ...], *, present_is_error: bool = False) -> Optional[int]:
    for key in keys:
        if key not in mapping or mapping.get(key) is None:
            continue
        value = mapping.get(key)
        if isinstance(value, bool):
            if present_is_error:
                raise HistoryAmbiguousError("invalid_ordinal", source="rollout")
            return None
        if isinstance(value, int):
            if value >= 0:
                return value
            if present_is_error:
                raise HistoryAmbiguousError("invalid_ordinal", source="rollout")
            return None
        if isinstance(value, str) and value.strip().isdigit():
            return int(value.strip())
        if present_is_error:
            raise HistoryAmbiguousError("invalid_ordinal", source="rollout")
        return None
    return None


def _record_ordinal(record: Mapping, payload: Optional[Mapping]) -> Optional[int]:
    value = _integer(record, _ORDINAL_KEYS, present_is_error=True)
    if value is not None:
        return value
    if isinstance(payload, Mapping):
        return _integer(payload, _ORDINAL_KEYS, present_is_error=True)
    return None


def _record_offset(record: Mapping, payload: Optional[Mapping]) -> Optional[int]:
    value = _integer(record, _OFFSET_KEYS)
    if value is not None:
        return value
    if isinstance(payload, Mapping):
        return _integer(payload, _OFFSET_KEYS)
    return None


def _partial_json_line(raw: bytes) -> bool:
    stripped = raw.lstrip()
    return stripped.startswith(b"{") or stripped.startswith(b"[")


def _incomplete_json_line(raw: bytes) -> bool:
    try:
        text = raw.decode("utf-8").rstrip("\r")
    except UnicodeDecodeError:
        return False
    try:
        json.loads(text)
    except json.JSONDecodeError as error:
        # An end-of-container error or an unterminated string is an
        # active-writer prefix. A malformed token at EOF is corruption.
        if error.msg.startswith("Unterminated string"):
            return bool(text.strip())
        return bool(text.strip()) and error.pos >= max(0, len(text.strip()) - 1)
    except (TypeError, ValueError):
        return False
    return False


def _parse_line(raw: bytes) -> Mapping:
    try:
        text = raw.decode("utf-8")
    except UnicodeDecodeError:
        raise ValueError("invalid encoding") from None
    try:
        value = json.loads(text)
    except (TypeError, ValueError):
        raise ValueError("invalid json") from None
    if not isinstance(value, Mapping):
        raise ValueError("record is not an object")
    return value


def _parse_complete_prefix(data: bytes) -> Tuple[List[_ParsedRecord], int]:
    records = []
    position = 0
    line_index = 0
    while position < len(data):
        newline = data.find(b"\n", position)
        if newline < 0:
            raw = data[position:]
            if not raw.strip():
                return records, len(data)
            try:
                value = _parse_line(raw.rstrip(b"\r"))
            except ValueError:
                if _partial_json_line(raw) and _incomplete_json_line(raw):
                    return records, position
                raise HistoryCorruptError("invalid_json", source="rollout") from None
            records.append(_ParsedRecord(value, position, len(data), line_index))
            return records, len(data)

        raw = data[position:newline].rstrip(b"\r")
        end = newline + 1
        if raw.strip():
            try:
                value = _parse_line(raw)
            except ValueError:
                raise HistoryCorruptError("invalid_json", source="rollout") from None
            records.append(_ParsedRecord(value, position, end, line_index))
        position = end
        line_index += 1
    return records, len(data)


def _payload(record: Mapping) -> Optional[Mapping]:
    value = record.get("payload")
    return value if isinstance(value, Mapping) else None


def _session_id(record: Mapping) -> Optional[str]:
    payload = _payload(record)
    for source in (payload, record):
        if not isinstance(source, Mapping):
            continue
        value = _string(source, "id", "thread_id", "threadId", "session_id", "sessionId")
        if value:
            return value
    return None


def _turn_id(record: Mapping, payload: Optional[Mapping]) -> Optional[str]:
    value = _string(record, "turn_id", "turnId")
    if value:
        return value
    if isinstance(payload, Mapping):
        return _string(payload, "turn_id", "turnId")
    return None


def _turn_context(record: Mapping) -> Optional[Tuple[Optional[str], str]]:
    if _token(record.get("type")) not in ("turn_context", "turncontext"):
        return None
    payload = _payload(record) or record
    model = None
    for field in ("model", "model_id", "modelId", "selected_model"):
        model = _model(payload.get(field))
        if model is not None:
            break
    if model is None:
        return None
    return _turn_id(record, payload), model


def _visible_payload(record: Mapping) -> Optional[Mapping]:
    record_type = _token(record.get("type"))
    payload = _payload(record)
    if record_type in (
        "session_meta",
        "sessionmeta",
        "turn_context",
        "turncontext",
        "event_msg",
        "response_item",
        ):
        if payload is None:
            return None
        if record_type == "event_msg":
            event_type = _token(payload.get("type"))
            if event_type in ("user_message", "usermessage"):
                return {
                    "type": "user_message",
                    "content": {
                        "text": payload.get("message"),
                        "images": payload.get("images"),
                    },
                    "id": payload.get("id"),
                }
            if event_type in (
                "assistant_message",
                "assistantmessage",
                "agent_message",
                "agentmessage",
            ):
                return {
                    "type": "assistant_message",
                    "message": payload.get("message"),
                    "id": payload.get("id"),
                }
            return payload if event_type else None
        return payload
    if record_type in (
        "compaction",
        "compaction_summary",
        "compacted",
        "compacted_summary",
        "compaction_marker",
        "compaction_boundary",
    ):
        if payload is None:
            return record
        return {
            "type": record_type,
            "message": payload.get("message"),
            "summary": payload.get("summary"),
            "content": payload.get("content"),
            "text": payload.get("text"),
            "marker": payload.get("marker"),
        }
    if isinstance(record.get("item"), Mapping):
        return record["item"]
    if isinstance(record.get("type"), str):
        return record
    return None


def _replacement_history(record: Mapping) -> Optional[List[Mapping]]:
    """Return a Codex 0.149 replacement base, or ``None`` for legacy compact."""

    record_type = _token(record.get("type"))
    if record_type not in (
        "compaction",
        "compaction_summary",
        "compacted",
        "compacted_summary",
    ):
        return None
    payload = _payload(record) or record
    if "replacement_history" not in payload or payload.get("replacement_history") is None:
        return None
    replacement = payload.get("replacement_history")
    if not isinstance(replacement, list) or any(
        not isinstance(item, Mapping) for item in replacement
    ):
        raise HistoryCorruptError("invalid_replacement_history", source="rollout")
    metadata = payload.get("replacement_history_metadata")
    if metadata is not None:
        if not isinstance(metadata, list) or len(metadata) != len(replacement):
            raise HistoryCorruptError(
                "invalid_replacement_history_metadata", source="rollout"
            )
    return [dict(item) for item in replacement]


def _portable_base_entries(entries: List[Tuple[Any, int, bool]]) -> List[Tuple[Any, int, bool]]:
    """Materialize the latest visible base without forwarding an opaque item."""

    boundaries = [
        index
        for index, (item, _, _) in enumerate(entries)
        if item.kind in ("compaction_summary", "compaction_marker")
    ]
    if not boundaries:
        return list(entries)
    boundary = boundaries[-1]
    item = entries[boundary][0]
    content = item.content
    has_summary = bool(content.strip()) if isinstance(content, str) else bool(content)
    if has_summary:
        return list(entries[boundary:])
    return [*entries[:boundary], *entries[boundary + 1 :]]


def _replacement_entries(
    replacement: List[Mapping],
    previous: List[Tuple[Any, int, bool]],
    *,
    turn_id: Optional[str],
    ordinal: int,
    offset: int,
    line_index: int,
    explicit: bool,
) -> List[Tuple[Any, int, bool]]:
    """Normalize one replacement base and terminate it with a visible boundary."""

    normalized = []
    for raw in replacement:
        if _token(raw.get("type")) == "compaction":
            encoded = raw.get("encrypted_content")
            if isinstance(encoded, str) and encoded:
                portable = _portable_base_entries(previous)
                if not portable:
                    raise HistoryUnavailableError(
                        "opaque_replacement_unavailable", source="rollout"
                    )
                normalized.extend(portable)
                continue
        item = normalize_visible_item(
            raw,
            turn_id=turn_id,
            ordinal=ordinal,
            offset=offset,
        )
        if item is not None:
            normalized.append((item, line_index, explicit))

    boundary = normalize_visible_item(
        {"type": "compaction_marker", "marker": ""},
        turn_id=turn_id,
        ordinal=ordinal,
        offset=offset,
    )
    if boundary is None:
        raise HistoryCorruptError("invalid_compaction_boundary", source="rollout")
    normalized.append((boundary, line_index, explicit))
    return normalized


def _record_thread_ids(records: List[_ParsedRecord]) -> List[str]:
    values = []
    for record in records:
        if _token(record.value.get("type")) not in ("session_meta", "sessionmeta"):
            continue
        value = _session_id(record.value)
        if value:
            values.append(value)
    return values


def _canonical_history_mode(records: List[_ParsedRecord]) -> Optional[str]:
    """Return the mode from the first canonical SessionMeta record."""

    for record in records:
        if _token(record.value.get("type")) not in ("session_meta", "sessionmeta"):
            continue
        payload = _payload(record.value)
        value = payload.get("history_mode") if isinstance(payload, Mapping) else None
        if value is None:
            return None
        if value not in ("legacy", "paginated"):
            raise HistoryUnsupportedError("invalid_history_mode", source="rollout")
        return value
    return None


def _message_role(record: Mapping) -> Optional[str]:
    """Identify duplicate UI/model message projections without reading content."""

    payload = _payload(record)
    if not isinstance(payload, Mapping):
        return None
    record_type = _token(record.get("type"))
    if record_type == "response_item" and _token(payload.get("type")) == "message":
        role = payload.get("role")
        return role if role in ("user", "assistant") else None
    if record_type == "event_msg":
        event_type = _token(payload.get("type"))
        if event_type in ("user_message", "usermessage"):
            return "user"
        if event_type in (
            "assistant_message",
            "assistantmessage",
            "agent_message",
            "agentmessage",
        ):
            return "assistant"
    return None


def _record_turn_ids(records: List[_ParsedRecord]) -> List[Optional[str]]:
    """Associate records with the latest TurnContext in append order."""

    result = []
    active_turn_id = None
    for record in records:
        payload = _payload(record.value)
        explicit_turn_id = _turn_id(record.value, payload)
        if explicit_turn_id is not None:
            active_turn_id = explicit_turn_id
        context = _turn_context(record.value)
        if context is not None and context[0] is not None:
            active_turn_id = context[0]
        result.append(explicit_turn_id or active_turn_id)
    return result


def _anchor_position(records: List[_ParsedRecord], anchor: HistoryAnchor) -> int:
    """Return the first record owned by the incoming turn.

    Codex appends ``task_started`` before ``TurnContext``.  Cutting at the
    first matching record prevents a reader invoked after a failed request
    from treating that request (or a later retry) as persisted history.
    """

    if not anchor.turn_id:
        return len(records)
    for index, record in enumerate(records):
        payload = _payload(record.value)
        if _turn_id(record.value, payload) == anchor.turn_id:
            return index
    # The current request may intentionally be ephemeral or may not have
    # reached the append-only rollout yet.  A fully settled captured prefix is
    # still an exact boundary: completed turns are history and the current
    # request is supplied separately by the caller.  An unterminated turn at
    # the end remains ambiguous and therefore fails closed.
    explicit_turns = set()
    terminal_turns = set()
    for record in records:
        payload = _payload(record.value)
        turn_id = _turn_id(record.value, payload)
        if turn_id is not None:
            explicit_turns.add(turn_id)
        if (
            _token(record.value.get("type")) == "event_msg"
            and isinstance(payload, Mapping)
            and _token(payload.get("type")) == "task_complete"
            and turn_id is not None
        ):
            terminal_turns.add(turn_id)
    if explicit_turns.issubset(terminal_turns):
        return len(records)
    raise HistoryUnavailableError("turn_not_found", source="rollout")


def _successful_turn_ids(records: List[_ParsedRecord]) -> set:
    """Return turns whose terminal Codex event completed without an error."""

    outcomes = {}
    for record in records:
        if _token(record.value.get("type")) != "event_msg":
            continue
        payload = _payload(record.value)
        if not isinstance(payload, Mapping) or _token(payload.get("type")) != "task_complete":
            continue
        turn_id = _turn_id(record.value, payload)
        if turn_id is not None:
            outcomes[turn_id] = payload.get("error") is None
    return {turn_id for turn_id, successful in outcomes.items() if successful}


def _source_model(
    records: List[_ParsedRecord],
    anchor: HistoryAnchor,
    initial_model: Optional[str],
) -> Optional[str]:
    successful = _successful_turn_ids(records)
    models = []
    for record in records:
        context = _turn_context(record.value)
        if context is not None and context[0] in successful:
            models.append(context[1])
    if models:
        return models[-1]
    # SQLite's row-level model is only safe when there is no incoming turn to
    # distinguish from prior successful history.
    return initial_model if not anchor.turn_id else None


class RolloutReader:
    """Read a captured complete prefix without writing or loading SQLite data."""

    def __init__(
        self,
        path: Optional[Path] = None,
        *,
        rollout_path: Optional[Path] = None,
        history_mode: Optional[str] = None,
        source_model: Optional[str] = None,
        model: Optional[str] = None,
        projection_ordinal: Optional[int] = None,
        projection_offset: Optional[int] = None,
        max_bytes: int = _MAX_ROLLOUT_BYTES,
    ) -> None:
        if path is not None and rollout_path is not None and Path(path) != Path(rollout_path):
            raise HistoryUnavailableError("conflicting_rollout_source", source="rollout")
        self.path = Path(path if path is not None else rollout_path) if (path is not None or rollout_path is not None) else None
        self.history_mode = history_mode
        self.source_model = _model(source_model) or _model(model)
        self.projection_ordinal = projection_ordinal
        self.projection_offset = projection_offset
        try:
            self.max_bytes = max(1, min(int(max_bytes), _MAX_ROLLOUT_BYTES))
        except (TypeError, ValueError):
            self.max_bytes = _MAX_ROLLOUT_BYTES

    def _read_visible_history(self, anchor: HistoryAnchor, location: Any = None) -> HistorySnapshot:
        if not isinstance(anchor, HistoryAnchor):
            raise HistoryUnavailableError("invalid_anchor", source="rollout")
        if not anchor.thread_id:
            raise HistoryUnavailableError("thread_identity_missing", source="rollout")
        path = self.path
        history_mode = self.history_mode
        source_model = self.source_model
        projection_ordinal = self.projection_ordinal
        projection_offset = self.projection_offset
        try:
            if location is not None:
                if path is None:
                    rollout_path = getattr(location, "rollout_path", None)
                    if rollout_path is not None:
                        path = Path(rollout_path)
                if history_mode is None:
                    history_mode = getattr(location, "history_mode", None)
                if source_model is None:
                    source_model = _model(getattr(location, "source_model", None))
                if projection_ordinal is None:
                    projection_ordinal = getattr(location, "projection_ordinal", None)
                if projection_offset is None:
                    projection_offset = getattr(location, "projection_offset", None)
        except Exception:
            raise HistoryUnavailableError("invalid_location", source="rollout") from None
        if path is None:
            raise HistoryUnavailableError("source_missing", source="rollout")
        try:
            before = os.stat(str(path))
        except (FileNotFoundError, OSError):
            raise HistoryUnavailableError("source_missing", source="rollout") from None
        if not stat.S_ISREG(before.st_mode):
            raise HistoryUnavailableError("source_missing", source="rollout")
        if before.st_size > self.max_bytes:
            raise HistoryUnsupportedError("source_too_large", source="rollout")

        try:
            with path.open("rb") as handle:
                opened = os.fstat(handle.fileno())
                if (opened.st_dev, opened.st_ino) != (before.st_dev, before.st_ino):
                    raise HistoryAmbiguousError("source_changed", source="rollout")
                captured_size = before.st_size
                if opened.st_size < captured_size:
                    raise HistoryAmbiguousError("source_changed", source="rollout")
                if captured_size > self.max_bytes:
                    raise HistoryUnsupportedError("source_too_large", source="rollout")
                data = handle.read(captured_size)
        except (HistoryAmbiguousError, HistoryUnsupportedError):
            raise
        except (FileNotFoundError, OSError):
            raise HistoryUnavailableError("source_unavailable", source="rollout") from None
        if len(data) != captured_size:
            raise HistoryAmbiguousError("source_changed", source="rollout")

        records, complete_boundary = _parse_complete_prefix(data)
        session_ids = _record_thread_ids(records)
        if not session_ids:
            raise HistoryMismatchError("session_meta_missing", source="rollout")
        if any(value != anchor.thread_id for value in session_ids):
            raise HistoryMismatchError("thread_mismatch", source="rollout")
        if len(set(session_ids)) != 1:
            raise HistoryMismatchError("thread_identity_conflict", source="rollout")

        anchor_position = _anchor_position(records, anchor)
        history_records = records[:anchor_position]
        history_boundary = (
            records[anchor_position].start
            if anchor_position < len(records)
            else complete_boundary
        )
        successful_turns = _successful_turn_ids(history_records)

        explicit_seen = False
        last_explicit = None
        explicit_max = None
        normalized_entries = []
        visible_offsets = []
        record_turn_ids = _record_turn_ids(history_records)
        response_message_roles = {
            (record_turn_ids[index], role)
            for index, record in enumerate(history_records)
            if _token(record.value.get("type")) == "response_item"
            for role in (_message_role(record.value),)
            if role is not None
        }
        for index, record in enumerate(history_records):
            payload = _payload(record.value)
            record_ordinal = _record_ordinal(record.value, payload)
            record_offset = _record_offset(record.value, payload)
            raw_item = _visible_payload(record.value)
            replacement = _replacement_history(record.value)
            item_turn_id = record_turn_ids[index]
            if item_turn_id is not None and item_turn_id not in successful_turns:
                raw_item = None
                replacement = None
            if (
                _token(record.value.get("type")) == "event_msg"
                and (item_turn_id, _message_role(record.value)) in response_message_roles
            ):
                # Legacy rollouts store both a model-visible ResponseItem and a
                # UI EventMsg for the same user/assistant message.  ResponseItem
                # is authoritative; carrying both would duplicate conversation
                # turns in the portable checkpoint.
                raw_item = None
            item_ordinal = record_ordinal
            if raw_item is not None and item_ordinal is None:
                item_ordinal = _integer(raw_item, _ORDINAL_KEYS)
            if item_ordinal is not None:
                explicit_seen = True
                if last_explicit is not None and item_ordinal < last_explicit:
                    raise HistoryAmbiguousError("ordinal_not_monotonic", source="rollout")
                last_explicit = item_ordinal
                explicit_max = max(explicit_max or item_ordinal, item_ordinal)
            if replacement is not None:
                explicit_item_ordinal = item_ordinal is not None
                if item_ordinal is None:
                    item_ordinal = record.line_index
                item_offset = record_offset if record_offset is not None else record.start
                normalized_entries = _replacement_entries(
                    replacement,
                    normalized_entries,
                    turn_id=item_turn_id,
                    ordinal=item_ordinal,
                    offset=item_offset,
                    line_index=record.line_index,
                    explicit=explicit_item_ordinal,
                )
                visible_offsets = [item_offset]
                continue
            if raw_item is None:
                continue
            explicit_item_ordinal = item_ordinal is not None
            if item_ordinal is None:
                item_ordinal = record.line_index
            item_offset = record_offset if record_offset is not None else record.start
            item = normalize_visible_item(
                raw_item,
                turn_id=item_turn_id,
                ordinal=item_ordinal,
                offset=item_offset,
            )
            if item is None:
                continue
            normalized_entries.append(
                (item, record.line_index, explicit_item_ordinal)
            )
            if item.offset is not None:
                visible_offsets.append(item.offset)

        requested_mode = history_mode
        if requested_mode is not None and requested_mode not in ("legacy", "paginated"):
            raise HistoryUnsupportedError("invalid_history_mode", source="rollout")
        canonical_mode = _canonical_history_mode(history_records)
        if requested_mode is not None and canonical_mode is not None:
            if requested_mode != canonical_mode:
                raise HistoryAmbiguousError("history_mode_mismatch", source="rollout")
        mode = requested_mode or canonical_mode or ("paginated" if explicit_seen else "legacy")
        if mode == "paginated" and not explicit_seen:
            raise HistoryUnsupportedError("ordinal_missing", source="rollout")
        if mode == "paginated" and any(
            not explicit for _, _, explicit in normalized_entries
        ):
            raise HistoryUnsupportedError("ordinal_missing", source="rollout")
        if mode == "legacy":
            normalized = [
                replace(item, ordinal=line_index)
                for item, line_index, _ in normalized_entries
            ]
        else:
            normalized = [item for item, _, _ in normalized_entries]

        highest_ordinal = explicit_max if mode == "paginated" else None
        if highest_ordinal is None and history_records:
            highest_ordinal = history_records[-1].line_index
        if projection_ordinal is not None and mode == "paginated":
            if isinstance(projection_ordinal, bool) or not isinstance(projection_ordinal, int) or projection_ordinal < 0:
                raise HistoryAmbiguousError("invalid_projection", source="rollout")
            highest_ordinal = max(highest_ordinal or projection_ordinal, projection_ordinal)
        if projection_offset is not None:
            if isinstance(projection_offset, bool) or not isinstance(projection_offset, int) or projection_offset < 0:
                raise HistoryAmbiguousError("invalid_projection", source="rollout")
            if projection_offset > captured_size:
                raise HistoryAmbiguousError("projection_mismatch", source="rollout")
        elif record_offsets := visible_offsets:
            projection_offset = max(record_offsets)

        file_identity = (int(before.st_dev), int(before.st_ino), int(before.st_mtime_ns))
        rollout_identity = "%d:%d:%d" % file_identity
        cursor = HistoryCursor(
            kind=mode,
            thread_id=anchor.thread_id,
            rollout_identity=rollout_identity,
            file_identity=file_identity,
            byte_boundary=history_boundary,
            captured_size=captured_size,
            line_index=history_records[-1].line_index if history_records else None,
            highest_ordinal=highest_ordinal,
            projection_offset=projection_offset,
        )
        return HistorySnapshot(
            anchor=anchor,
            items=tuple(normalized),
            cursor=cursor,
            source="rollout",
            source_model=_source_model(history_records, anchor, source_model),
        )

    def read_visible_history(self, anchor: HistoryAnchor, location: Any = None) -> HistorySnapshot:
        try:
            return self._read_visible_history(anchor, location=location)
        except HistoryError:
            raise
        except Exception:
            raise HistoryUnavailableError("rollout_unavailable", source="rollout") from None

    def read(self, anchor: HistoryAnchor, location: Any = None) -> HistorySnapshot:
        return self.read_visible_history(anchor, location=location)

__all__ = ["RolloutReader"]
