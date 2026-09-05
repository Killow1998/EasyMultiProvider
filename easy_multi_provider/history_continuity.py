"""Fail-closed projection of Codex-visible compacted history."""

from __future__ import annotations

import base64
import copy
import json
import uuid
from collections.abc import Mapping, Sequence
from dataclasses import replace
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from .codex_history import (
    HistoryAnchor,
    HistoryAmbiguousError,
    HistoryError,
    HistoryInputError,
    HistoryMismatchError,
    HistorySnapshot,
    HistoryUnavailableError,
    SQLiteReader,
    RolloutReader,
)
from .diagnostic_journal import NullJournal
from .dialects import CODEX_NATIVE, classify_dialect
from .portable_checkpoint import (
    PortableCheckpointError,
    build_compaction_replacement,
)
from .router_errors import HistoryReconstructionError


_EMP_COMPACTION_PREFIX = "emp1:"
ACTIVE_INPUT_START_KEY = "_emp_active_input_start"
_COMPACTION_KINDS = frozenset({"compaction_summary", "compaction_marker"})
_SUMMARY_PREFIX = (
    "Portable checkpoint from Codex-visible local history. Continue from this "
    "state without repeating completed work."
)


def _input_items(body: Mapping[str, Any]) -> Tuple[Any, ...]:
    value = body.get("input")
    if isinstance(value, list):
        return tuple(value)
    if isinstance(value, (Mapping, str)):
        return (value,)
    return ()


def _trailing_compaction_trigger(body: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    items = _input_items(body)
    if not items or not isinstance(items[-1], Mapping):
        return None
    if items[-1].get("type") != "compaction_trigger":
        return None
    return copy.deepcopy(dict(items[-1]))


def _opaque_compaction(body: Mapping[str, Any]) -> bool:
    for item in _input_items(body):
        if not isinstance(item, Mapping) or item.get("type") != "compaction":
            continue
        value = item.get("encrypted_content")
        if not isinstance(value, str) or not value.startswith(_EMP_COMPACTION_PREFIX):
            return True
    return False


def _content_text(value: Any) -> str:
    if isinstance(value, str):
        return value
    if isinstance(value, Mapping):
        for key in ("text", "message", "content", "output", "result"):
            if key in value:
                text = _content_text(value[key])
                if text:
                    return text
        return json.dumps(value, ensure_ascii=False, sort_keys=True)
    if isinstance(value, Sequence) and not isinstance(value, (str, bytes)):
        return "\n".join(filter(None, (_content_text(item) for item in value)))
    return "" if value is None else str(value)


def request_history_anchor(
    body: Mapping[str, Any], incoming: Mapping[str, str]
) -> HistoryAnchor:
    """Read the canonical turn metadata from the active Codex transport."""

    client_metadata = body.get("client_metadata")
    if isinstance(client_metadata, Mapping) and "x-codex-turn-metadata" in client_metadata:
        raw = client_metadata.get("x-codex-turn-metadata")
        if not isinstance(raw, str):
            raise HistoryInputError("invalid_turn_metadata")
        request_anchor = HistoryAnchor.from_headers(
            {"x-codex-turn-metadata": raw}
        )
        merged = {
            key: value
            for key, value in incoming.items()
            if not (
                isinstance(key, str)
                and key.lower() == "x-codex-turn-metadata"
            )
        }
        if request_anchor.window_id is not None:
            merged = {
                key: value
                for key, value in merged.items()
                if not (
                    isinstance(key, str)
                    and key.lower() == "x-codex-window-id"
                )
            }
        merged["x-codex-turn-metadata"] = raw
        return HistoryAnchor.from_headers(merged)
    return HistoryAnchor.from_headers(incoming)


def _message(role: str, text: str) -> Dict[str, Any]:
    part_type = "output_text" if role == "assistant" else "input_text"
    return {"type": "message", "role": role, "content": [{"type": part_type, "text": text}]}


def _message_with_content(role: str, content: Any) -> Dict[str, Any]:
    if isinstance(content, list):
        return {"type": "message", "role": role, "content": copy.deepcopy(content)}
    if isinstance(content, Mapping):
        nested = content.get("content")
        if isinstance(nested, (str, list)):
            return _message_with_content(role, nested)
        parts = []
        text = content.get("text", content.get("message"))
        if isinstance(text, str) and text:
            parts.append({"type": "input_text" if role == "user" else "output_text", "text": text})
        images = content.get("images")
        if isinstance(images, list):
            for image in images:
                if isinstance(image, Mapping) and image.get("type") in {"input_image", "output_image"}:
                    parts.append(copy.deepcopy(dict(image)))
                else:
                    image_url = image.get("image_url", image.get("url")) if isinstance(image, Mapping) else image
                    if isinstance(image_url, str) and image_url:
                        parts.append({"type": "input_image", "image_url": image_url})
        if parts:
            return {"type": "message", "role": role, "content": parts}
    return _message(role, _content_text(content))


def _wire_item(item: Mapping[str, Any]) -> Any:
    wire = item.get("wire")
    if isinstance(wire, Mapping):
        return copy.deepcopy(dict(wire))
    if isinstance(wire, str):
        return wire
    kind = item.get("kind")
    content = copy.deepcopy(item.get("content"))
    if kind in {"user_message", "assistant_message"}:
        return _message_with_content("user" if kind == "user_message" else "assistant", content)
    if kind in _COMPACTION_KINDS:
        text = _content_text(content).strip()
        return _message("user", _SUMMARY_PREFIX + "\n\n" + text) if text else None
    if kind in {"tool_call", "tool_result"}:
        value = dict(content) if isinstance(content, Mapping) else {"output": content}
        value["type"] = item.get("raw_type") or (
            "function_call" if kind == "tool_call" else "function_call_output"
        )
        if item.get("call_id"):
            value["call_id"] = item["call_id"]
        if item.get("item_id"):
            value["id"] = item["item_id"]
        return value
    if kind == "standalone_tool_output":
        value = dict(content) if isinstance(content, Mapping) else {"output": content}
        value["type"] = "function_call_output"
        value.pop("call_id", None)
        return value
    text = _content_text(content).strip()
    if not text:
        return None
    label = str(kind or "visible history").replace("_", " ")
    return _message("user", "Visible %s: %s" % (label, text))


def _render(items: Sequence[Mapping[str, Any]]) -> list:
    rendered = []
    for item in items:
        projected = _wire_item(item)
        if projected is not None:
            rendered.append(projected)
    return rendered


def _opaque_compaction_index(body: Mapping[str, Any]) -> int:
    opaque = []
    for index, item in enumerate(_input_items(body)):
        if not isinstance(item, Mapping) or item.get("type") != "compaction":
            continue
        encoded = item.get("encrypted_content")
        if not isinstance(encoded, str) or not encoded.startswith(_EMP_COMPACTION_PREFIX):
            opaque.append(index)
    if len(opaque) != 1:
        raise HistoryReconstructionError(
            "compaction_boundary_missing" if not opaque else "multiple_compaction_boundaries"
        )
    return opaque[0]


def _decode_portable_compaction(item: Mapping[str, Any]) -> Dict[str, Any]:
    encoded = item.get("encrypted_content")
    if not isinstance(encoded, str) or not encoded.startswith(_EMP_COMPACTION_PREFIX):
        raise PortableCheckpointError()
    try:
        summary = base64.b64decode(
            encoded[len(_EMP_COMPACTION_PREFIX) :],
            altchars=b"-_",
            validate=True,
        ).decode("utf-8")
    except (UnicodeDecodeError, ValueError):
        raise PortableCheckpointError() from None
    if not summary.strip():
        raise PortableCheckpointError()
    return _message("user", _SUMMARY_PREFIX + "\n\n" + summary)


def _decode_portable_items(body: Mapping[str, Any]) -> Dict[str, Any]:
    projected = copy.deepcopy(dict(body))
    source = list(_input_items(body))
    latest_portable = None
    for index, item in enumerate(source):
        if not isinstance(item, Mapping) or item.get("type") != "compaction":
            continue
        encoded = item.get("encrypted_content")
        if isinstance(encoded, str) and encoded.startswith(_EMP_COMPACTION_PREFIX):
            source[index] = _decode_portable_compaction(item)
            latest_portable = index
    if isinstance(body.get("input"), list):
        projected["input"] = source
    elif latest_portable is not None:
        projected["input"] = source
    if latest_portable is not None:
        projected[ACTIVE_INPUT_START_KEY] = latest_portable + 1
    return projected


class HistoryContinuityEngine:
    """Materialize only source history that the selected destination cannot read."""

    def __init__(self, reader: Any):
        self.reader = reader

    def prepare(
        self,
        config: Mapping[str, Any],
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        requested_slug: str,
        body: Mapping[str, Any],
        incoming: Mapping[str, str],
    ) -> Dict[str, Any]:
        del config, model, requested_slug
        try:
            decoded = _decode_portable_items(body)
        except PortableCheckpointError as exc:
            raise HistoryReconstructionError(exc.code) from None
        if classify_dialect(provider) == CODEX_NATIVE:
            # Native can consume its own opaque compaction state.  EMP portable
            # checkpoints are visible text and therefore need only decoding.
            return decoded

        if not _opaque_compaction(decoded):
            return decoded

        try:
            anchor = request_history_anchor(decoded, incoming)
        except HistoryError as exc:
            raise HistoryReconstructionError(exc.reason) from None
        if not anchor.thread_id:
            raise HistoryReconstructionError("thread_identity_missing")
        if not anchor.turn_id:
            raise HistoryReconstructionError("turn_identity_missing")
        try:
            read_compaction = getattr(self.reader, "read_compaction_history", None)
            snapshot = (read_compaction(anchor, _input_items(decoded)[_opaque_compaction_index(decoded)])
                        if read_compaction is not None else self.reader.read_visible_history(anchor))
        except HistoryError as exc:
            raise HistoryReconstructionError(exc.reason) from None
        except Exception:
            raise HistoryReconstructionError("history_unavailable") from None
        if not isinstance(snapshot, HistorySnapshot):
            raise HistoryReconstructionError("invalid_history_snapshot")
        if snapshot.thread_id != anchor.thread_id:
            raise HistoryReconstructionError("thread_mismatch")
        try:
            source = list(_input_items(decoded))
            boundary = _opaque_compaction_index(decoded)
            history = build_compaction_replacement(snapshot.items)
            trigger = _trailing_compaction_trigger(decoded)
            tail = source[boundary + 1 :]
            if trigger is not None and tail and tail[-1] == trigger:
                tail = tail[:-1]
            portable_history = _render(history)
            rendered = [
                *copy.deepcopy(source[:boundary]),
                *portable_history,
                *copy.deepcopy(tail),
                *(copy.deepcopy([trigger]) if trigger is not None else []),
            ]
            projected = copy.deepcopy(dict(decoded))
            projected["input"] = rendered
            projected[ACTIVE_INPUT_START_KEY] = (
                len(source[:boundary]) + len(portable_history)
            )
            return projected
        except HistoryReconstructionError:
            raise
        except PortableCheckpointError as exc:
            raise HistoryReconstructionError(exc.code) from None
        except Exception:
            raise HistoryReconstructionError("checkpoint_invalid") from None


class CodexHomeHistoryReader:
    """Resolve the current Codex state DB lazily and read it without writes."""

    def __init__(self, codex_home: Path, app_server_reader: Any = None, *, journal=None):
        self.codex_home = Path(codex_home).resolve()
        self.app_server_reader = app_server_reader
        self.journal = journal if journal is not None else NullJournal()

    def _lookup_event(self, anchor, source, result, reason="", *, fallback=False):
        try:
            self.journal.event(
                "info" if result == "found" else "warning", "history_lookup",
                thread_ref=self.journal.pseudonym(anchor.thread_id or "missing"),
                turn_ref=self.journal.pseudonym(anchor.turn_id or "missing"),
                codex_home_ref=self.journal.pseudonym(str(self.codex_home)),
                source=source, result=result, reason=reason,
                anchor_found=result == "found", fallback=fallback,
            )
        except Exception:
            pass

    def read_compaction_history(self, anchor: HistoryAnchor, compaction: Mapping) -> HistorySnapshot:
        try:
            return self.read_visible_history(anchor)
        except HistoryUnavailableError as original:
            if original.reason != "thread_missing" or not anchor.forked_from_thread_id:
                raise
        # Codex's fork metadata names the parent. Restrict lookup to that UUID in
        # the configured home and require the exact inherited encrypted checkpoint.
        # No global scan, active app-server attachment, or parent-tail guessing.
        try:
            parent_id = str(uuid.UUID(anchor.forked_from_thread_id))
            if parent_id == anchor.thread_id:
                raise HistoryMismatchError("fork_parent_invalid", source="rollout")
            parent_anchor = HistoryAnchor(thread_id=parent_id)
            location = self._sqlite_reader().locate(parent_anchor)
            if not location.rollout_path.resolve().is_relative_to(self.codex_home):
                raise HistoryMismatchError("rollout_outside_codex_home", source="rollout")
            snapshot = RolloutReader(location.rollout_path, history_mode=location.history_mode,
                                     source_model=location.source_model,
                                     compaction_item=compaction).read_visible_history(parent_anchor)
            snapshot = replace(snapshot, anchor=anchor,
                               cursor=replace(snapshot.cursor, thread_id=anchor.thread_id))
            self._lookup_event(anchor, "fork_checkpoint", "found", fallback=True)
            return snapshot
        except (ValueError, AttributeError):
            raise HistoryMismatchError("fork_parent_invalid", source="rollout") from None
        except HistoryError as failure:
            self._lookup_event(anchor, "fork_checkpoint", "rejected", failure.reason, fallback=True)
            raise

    def _rollout_fallback(self, anchor: HistoryAnchor, original: HistoryError) -> HistorySnapshot:
        # SQLite is an index, not the conversation itself. Only inspect canonical
        # paths within this configured home; never guess a Side chat's parent or
        # scan another user's/home's history. RolloutReader verifies both the
        # session metadata and the exact turn/window before exposing any content.
        try:
            thread_id = str(uuid.UUID(anchor.thread_id or ""))
        except (ValueError, AttributeError):
            raise original
        candidates = set()
        patterns = (
            "sessions/*/*/*/rollout-*-%s.jsonl" % thread_id,
            "archived_sessions/rollout-*-%s.jsonl" % thread_id,
        )
        for pattern in patterns:
            for path in self.codex_home.glob(pattern):
                resolved = path.resolve()
                if not resolved.is_relative_to(self.codex_home):
                    raise HistoryMismatchError("rollout_outside_codex_home", source="rollout")
                if resolved.is_file():
                    candidates.add(resolved)
                if len(candidates) > 1:
                    raise HistoryAmbiguousError("multiple_rollout_sources", source="rollout")
        if not candidates:
            raise original
        return RolloutReader(next(iter(candidates))).read_visible_history(anchor)

    def _sqlite_reader(self) -> SQLiteReader:
        candidates = []
        for path in self.codex_home.glob("state_*.sqlite"):
            try:
                version = int(path.stem.rsplit("_", 1)[1])
            except (IndexError, ValueError):
                continue
            if path.is_file():
                candidates.append((version, path))
        if not candidates:
            raise HistoryUnavailableError("state_database_missing", source="sqlite")
        return SQLiteReader(max(candidates, key=lambda item: item[0])[1])

    def read_visible_history(self, anchor: HistoryAnchor) -> HistorySnapshot:
        if not isinstance(anchor, HistoryAnchor):
            raise HistoryInputError("invalid_anchor")
        if self.app_server_reader is not None:
            try:
                snapshot = self.app_server_reader.read_visible_history(anchor)
            except HistoryMismatchError:
                raise
            except Exception:
                snapshot = None
            if isinstance(snapshot, HistorySnapshot) and snapshot.thread_id != anchor.thread_id:
                raise HistoryMismatchError("thread_mismatch", source="app_server")
            if isinstance(snapshot, HistorySnapshot) and any(
                item.kind in _COMPACTION_KINDS and _content_text(item.content).strip()
                for item in snapshot.items
            ):
                if snapshot.source_model is None:
                    try:
                        metadata = self._sqlite_reader().read_visible_history(anchor)
                    except Exception:
                        metadata = None
                    if isinstance(metadata, HistorySnapshot) and metadata.source_model is not None:
                        snapshot = replace(snapshot, source_model=metadata.source_model)
                self._lookup_event(anchor, "app_server", "found")
                return snapshot
        try:
            snapshot = self._sqlite_reader().read_visible_history(anchor)
            self._lookup_event(anchor, "sqlite", "found")
            return snapshot
        except HistoryError as exc:
            self._lookup_event(anchor, "sqlite", "missing" if exc.reason == "thread_missing" else "rejected", exc.reason)
            if isinstance(exc, HistoryUnavailableError) and exc.reason in {
                "state_database_missing", "database_missing", "thread_missing",
                "rollout_path_missing", "source_missing",
            }:
                try:
                    snapshot = self._rollout_fallback(anchor, exc)
                except HistoryError as failure:
                    self._lookup_event(anchor, "rollout", "rejected", failure.reason, fallback=True)
                    raise
                self._lookup_event(anchor, "rollout", "found", fallback=True)
                return snapshot
            raise
        except Exception:
            raise HistoryUnavailableError("history_unavailable", source="sqlite") from None


__all__ = [
    "ACTIVE_INPUT_START_KEY",
    "CodexHomeHistoryReader",
    "HistoryContinuityEngine",
    "HistoryReconstructionError",
]
