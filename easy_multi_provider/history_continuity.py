"""Fail-closed projection of Codex-visible compacted history."""

from __future__ import annotations

import copy
import json
import math
from collections.abc import Mapping, Sequence
from dataclasses import replace
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from .catalog import load_native_catalog
from .codex_history import (
    HistoryAnchor,
    HistoryError,
    HistoryInputError,
    HistoryMismatchError,
    HistorySnapshot,
    HistoryUnavailableError,
    SQLiteReader,
    normalize_visible_item,
)
from .context_guard import safe_context_status
from .portable_checkpoint import PortableCheckpointError, build_portable_view
from .router_errors import HistoryReconstructionError


_EMP_COMPACTION_PREFIX = "emp1:"
HISTORY_REBUILD_MARKER = "emp-history-rebuild:v1"
_COMPACTION_KINDS = frozenset({"compaction_summary", "compaction_marker"})
_HIDDEN_TYPES = frozenset(
    {"analysis", "chain_of_thought", "hidden_cot", "hidden_reasoning", "reasoning", "thinking"}
)
_CONCRETE_PROTOCOLS = frozenset({"responses", "chat_completions", "anthropic_messages"})
_SUMMARY_PREFIX = (
    "Portable checkpoint from Codex-visible local history. Continue from this "
    "state without repeating completed work."
)
_ACTIVE_REQUEST_BOUNDARY = (
    "Cross-model handoff boundary: everything above is completed visible history. "
    "The user message below is the active request; do not execute stale requests "
    "or acceptance phrases from the history."
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


def _forced_history_rebuild(body: Mapping[str, Any]) -> bool:
    return any(
        isinstance(item, Mapping)
        and item.get("type") == "compaction"
        and item.get("encrypted_content") == HISTORY_REBUILD_MARKER
        for item in _input_items(body)
    )


def _native_destination(provider: Mapping[str, Any]) -> bool:
    return provider.get("implicit_native") is True or provider.get("auth_mode") == "account"


def _protocol(provider: Mapping[str, Any], model: Mapping[str, Any]) -> str:
    configured = provider.get("protocol")
    if configured in _CONCRETE_PROTOCOLS:
        return str(configured)
    for source in (model, provider):
        observed = source.get("observed_protocol")
        if observed in _CONCRETE_PROTOCOLS:
            return str(observed)
        capabilities = source.get("observed_capabilities")
        if isinstance(capabilities, Mapping) and capabilities.get("protocol") in _CONCRETE_PROTOCOLS:
            return str(capabilities["protocol"])
    return "responses"


def _positive_int(value: Any) -> Optional[int]:
    if isinstance(value, bool):
        return None
    try:
        value = int(value)
    except (TypeError, ValueError):
        return None
    return value if value > 0 else None


def _effective_context_window(source: Mapping[str, Any]) -> Optional[int]:
    context = next(
        (
            value
            for field in (
                "context_window",
                "max_context_window",
                "max_input_tokens",
                "input_token_limit",
                "inputTokenLimit",
            )
            for value in (_positive_int(source.get(field)),)
            if value is not None
        ),
        None,
    )
    if context is None:
        return None
    try:
        percentage = float(source.get("effective_context_window_percent", 100) or 100)
    except (TypeError, ValueError):
        percentage = 100.0
    if math.isfinite(percentage) and 0 < percentage <= 100:
        return max(1, round(context * percentage / 100))
    return context


def _native_models(config: Mapping[str, Any]) -> Tuple[Mapping[str, Any], ...]:
    for field in ("native_catalog", "_native_catalog"):
        inline = config.get(field)
        if isinstance(inline, Mapping) and isinstance(inline.get("models"), list):
            return tuple(item for item in inline["models"] if isinstance(item, Mapping))
    try:
        loaded = load_native_catalog(dict(config))
    except Exception:
        return ()
    if not isinstance(loaded, Mapping) or not isinstance(loaded.get("models"), list):
        return ()
    return tuple(item for item in loaded["models"] if isinstance(item, Mapping))


def _budget_model(
    config: Mapping[str, Any], provider: Mapping[str, Any], model: Mapping[str, Any], slug: str
) -> Dict[str, Any]:
    """Resolve explicit model limits without inventing a model capability."""

    merged = dict(model)
    route_ids = {slug}
    for value in (model.get("id"), model.get("upstream_id")):
        if isinstance(value, str) and value:
            route_ids.add(value)
            route_ids.add(value.rsplit("/", 1)[-1])
    sources = [model]
    for candidate in config.get("models", ()):
        if not isinstance(candidate, Mapping):
            continue
        candidate_ids = {candidate.get("id"), candidate.get("upstream_id")}
        if route_ids.intersection(value for value in candidate_ids if isinstance(value, str)):
            sources.append(candidate)
    if _native_destination(provider):
        for candidate in _native_models(config):
            candidate_id = candidate.get("slug") or candidate.get("id") or candidate.get("model_id")
            if candidate_id in route_ids:
                sources.append(candidate)
    for source in sources:
        context = _effective_context_window(source)
        if context is not None:
            merged["context_window"] = context
            break
    for source in sources:
        for field in (
            "output_limit",
            "output_token_limit",
            "max_output_tokens",
            "max_tokens",
            "max_completion_tokens",
        ):
            limit = _positive_int(source.get(field))
            if limit is not None:
                merged["output_limit"] = limit
                return merged
    return merged


def _estimate(value: Any) -> int:
    try:
        encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    except Exception:
        raise PortableCheckpointError() from None
    return max(1, int(math.ceil(len(encoded) / 2.0))) if encoded else 0


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


def _hidden_current_item(raw: Mapping[str, Any]) -> bool:
    if raw.get("visible") is False or raw.get("hidden") is True:
        return True
    item_type = str(raw.get("type") or "").lower().replace("-", "_").replace(" ", "_")
    if item_type in _HIDDEN_TYPES:
        return True
    if "reasoning" in item_type or "chain_of_thought" in item_type:
        return not (
            raw.get("visible") is True
            or raw.get("visibility") == "visible"
            or raw.get("visible_reasoning_summary") is True
        )
    nested = raw.get("payload")
    if not isinstance(nested, Mapping):
        nested = raw.get("item")
    return isinstance(nested, Mapping) and _hidden_current_item(nested)


def _current_items(body: Mapping[str, Any], anchor: HistoryAnchor) -> Tuple[Dict[str, Any], ...]:
    result = []
    compaction_types = {
        "compaction", "compaction_trigger", "compaction_marker", "compaction_summary",
        "compacted", "compacted_summary", "compaction_boundary",
    }
    for raw in _input_items(body):
        if isinstance(raw, Mapping):
            item_type = str(raw.get("type") or "").lower().replace("-", "_").replace(" ", "_")
            if _hidden_current_item(raw) or item_type in compaction_types:
                continue
            normalized = normalize_visible_item(raw, turn_id=anchor.turn_id)
            mapping = {
                "kind": normalized.kind if normalized else "wire_item",
                "content": copy.deepcopy(normalized.content if normalized else dict(raw)),
                "item_id": normalized.item_id if normalized else None,
                "turn_id": normalized.turn_id if normalized else anchor.turn_id,
                "call_id": normalized.call_id if normalized else None,
                "raw_type": normalized.raw_type if normalized else item_type,
                "wire": copy.deepcopy(dict(raw)),
            }
        else:
            mapping = {
                "kind": "wire_item",
                "content": copy.deepcopy(raw),
                "turn_id": anchor.turn_id,
                "wire": copy.deepcopy(raw),
            }
        result.append({key: value for key, value in mapping.items() if value is not None})
    return tuple(result)


def _history_anchor(
    body: Mapping[str, Any], incoming: Mapping[str, str]
) -> HistoryAnchor:
    """Read the canonical turn metadata from the active Codex transport."""

    client_metadata = body.get("client_metadata")
    if isinstance(client_metadata, Mapping) and "x-codex-turn-metadata" in client_metadata:
        raw = client_metadata.get("x-codex-turn-metadata")
        if not isinstance(raw, str):
            raise HistoryInputError("invalid_turn_metadata")
        return HistoryAnchor.from_headers({"x-codex-turn-metadata": raw})
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
    text = _content_text(content).strip()
    if not text:
        return None
    label = str(kind or "visible history").replace("_", " ")
    return _message("user", "Visible %s: %s" % (label, text))


def _render(checkpoint: Any) -> list:
    rendered = []
    history = (
        *((checkpoint.summary,) if checkpoint.summary is not None else ()),
        *checkpoint.retained_tail,
    )
    for item in history:
        projected = _wire_item(item)
        if projected is not None:
            rendered.append(projected)
    if checkpoint.current_request:
        rendered.append(_message("user", _ACTIVE_REQUEST_BOUNDARY))
    for item in checkpoint.current_request:
        projected = _wire_item(item)
        if projected is not None:
            rendered.append(projected)
    return rendered


def _safe_budget(
    config: Mapping[str, Any], provider: Mapping[str, Any], model: Mapping[str, Any], slug: str, body: Mapping[str, Any]
) -> int:
    budget_model = _budget_model(config, provider, model, slug)
    if _positive_int(budget_model.get("output_limit")) is None:
        for field in ("max_output_tokens", "max_tokens", "max_completion_tokens", "output_token_limit"):
            limit = _positive_int(body.get(field))
            if limit is not None:
                budget_model["output_limit"] = limit
                break
    status = safe_context_status(provider, budget_model, _protocol(provider, budget_model))
    safe = _positive_int(status.get("safe_input_limit"))
    if safe is not None:
        return safe
    context = _positive_int(status.get("context_limit"))
    if context is None:
        raise HistoryReconstructionError("context_budget_unknown")
    # Some catalogs publish only a context window. Reserving ten percent is
    # explicit and conservative; absence of the context window still fails.
    return max(1, int(context * 0.9))


class HistoryContinuityEngine:
    """Project native opaque compaction only when the destination is external."""

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
        compaction_trigger = _trailing_compaction_trigger(body)
        forced = _forced_history_rebuild(body)
        if not forced and (
            not _opaque_compaction(body) or _native_destination(provider)
        ):
            return copy.deepcopy(dict(body))
        try:
            anchor = _history_anchor(body, incoming)
        except HistoryError as exc:
            raise HistoryReconstructionError(exc.reason) from None
        if not anchor.thread_id:
            raise HistoryReconstructionError("thread_identity_missing")
        if not anchor.turn_id:
            raise HistoryReconstructionError("turn_identity_missing")
        try:
            snapshot = self.reader.read_visible_history(anchor)
        except HistoryError as exc:
            raise HistoryReconstructionError(exc.reason) from None
        except Exception:
            raise HistoryReconstructionError("history_unavailable") from None
        if not isinstance(snapshot, HistorySnapshot):
            raise HistoryReconstructionError("invalid_history_snapshot")
        if snapshot.thread_id != anchor.thread_id:
            raise HistoryReconstructionError("thread_mismatch")
        try:
            checkpoint = build_portable_view(snapshot.items, _current_items(body, anchor))
            rendered = _render(checkpoint)
            if compaction_trigger is not None:
                rendered.append(compaction_trigger)
            projected = copy.deepcopy(dict(body))
            projected["input"] = rendered
            fixed = {
                key: projected[key]
                for key in ("input", "instructions", "tools", "text", "response_format")
                if key in projected
            }
            if _estimate(fixed) > _safe_budget(config, provider, model, requested_slug, body):
                raise HistoryReconstructionError("context_budget_exceeded")
        except HistoryReconstructionError:
            raise
        except PortableCheckpointError as exc:
            raise HistoryReconstructionError(exc.code) from None
        except Exception:
            raise HistoryReconstructionError("checkpoint_invalid") from None
        return projected


class CodexHomeHistoryReader:
    """Resolve the current Codex state DB lazily and read it without writes."""

    def __init__(self, codex_home: Path, app_server_reader: Any = None):
        self.codex_home = Path(codex_home)
        self.app_server_reader = app_server_reader

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
                return snapshot
        try:
            return self._sqlite_reader().read_visible_history(anchor)
        except HistoryError:
            raise
        except Exception:
            raise HistoryUnavailableError("history_unavailable", source="sqlite") from None


__all__ = [
    "CodexHomeHistoryReader",
    "HISTORY_REBUILD_MARKER",
    "HistoryContinuityEngine",
    "HistoryReconstructionError",
]
