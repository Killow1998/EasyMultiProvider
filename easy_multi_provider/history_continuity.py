"""Fail-closed projection of Codex-visible compacted history."""

from __future__ import annotations

import base64
import copy
import json
import math
import threading
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
)
from .context_guard import estimate_json_tokens, safe_context_status
from .history_compaction import (
    COMPACTION_COMPACTED,
    COMPACTION_FAIL_CLOSED,
    COMPACTION_NO_COMPACTION,
    CompactionMetrics,
    DestinationSummarizer,
    HistoryCompactionError,
    HistoryCompactor,
    MemoryCheckpointCache,
    split_atomic_units,
)
from .portable_checkpoint import (
    PortableCheckpointError,
    build_compaction_replacement,
    build_visible_history,
)
from .router_errors import HistoryReconstructionError


_EMP_COMPACTION_PREFIX = "emp1:"
HISTORY_REBUILD_MARKER = "emp-history-rebuild:v1"
_COMPACTION_KINDS = frozenset({"compaction_summary", "compaction_marker"})
_CONCRETE_PROTOCOLS = frozenset({"responses", "chat_completions", "anthropic_messages"})
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
            # _effective_context_window already applied the catalog's usable
            # percentage. Context Guard must not apply it a second time.
            merged.pop("effective_context_window_percent", None)
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
    estimate = estimate_json_tokens(value)
    if estimate is None:
        raise PortableCheckpointError() from None
    return estimate


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


def _portable_compaction_index(body: Mapping[str, Any]) -> Optional[int]:
    for index, item in enumerate(_input_items(body)):
        if not isinstance(item, Mapping) or item.get("type") != "compaction":
            continue
        encoded = item.get("encrypted_content")
        if isinstance(encoded, str) and encoded.startswith(_EMP_COMPACTION_PREFIX):
            return index
    return None


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
    """Reconstruct Codex history and optionally compact it for one destination."""

    def __init__(
        self,
        reader: Any,
        destination_summarizer: Optional[DestinationSummarizer] = None,
        *,
        compaction_cache: Optional[MemoryCheckpointCache] = None,
        compactor: Optional[HistoryCompactor] = None,
    ):
        self.reader = reader
        self.compactor = compactor or HistoryCompactor(
            destination_summarizer,
            cache=compaction_cache,
        )
        self._request_state = threading.local()

    @property
    def last_compaction_metrics(self) -> Optional[CompactionMetrics]:
        """Return metrics for the current request thread only."""

        return getattr(self._request_state, "compaction_metrics", None)

    @last_compaction_metrics.setter
    def last_compaction_metrics(self, value: Optional[CompactionMetrics]) -> None:
        self._request_state.compaction_metrics = value

    def prepare(
        self,
        config: Mapping[str, Any],
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        requested_slug: str,
        body: Mapping[str, Any],
        incoming: Mapping[str, str],
    ) -> Dict[str, Any]:
        self.last_compaction_metrics = None
        forced = _forced_history_rebuild(body)
        if not forced and _native_destination(provider):
            self.last_compaction_metrics = self.compactor.no_compaction(
                body, None
            ).metrics
            return copy.deepcopy(dict(body))

        opaque = _opaque_compaction(body)
        if not forced and not opaque:
            # The ordinary path is intentionally reader-free.  A full visible
            # request only needs Codex history preparation after it crosses the
            # destination budget.
            try:
                budget = _safe_budget(
                    config, provider, model, requested_slug, body
                )
            except HistoryReconstructionError:
                return copy.deepcopy(dict(body))
            fixed = {
                key: body[key]
                for key in ("input", "instructions", "tools", "text", "response_format")
                if key in body
            }
            if _estimate(fixed) <= budget:
                result = self.compactor.no_compaction(body, budget, _estimate(fixed))
                self.last_compaction_metrics = result.metrics
                return copy.deepcopy(dict(body))
            try:
                source = list(_input_items(body))
                portable_index = _portable_compaction_index(body)
                if portable_index is not None:
                    source[portable_index] = _decode_portable_compaction(
                        source[portable_index]
                    )
                trigger = _trailing_compaction_trigger(body)
                if trigger is not None and source and source[-1] == trigger:
                    source.pop()
                if portable_index is not None:
                    candidate = source[: portable_index + 1]
                    active = source[portable_index + 1 :]
                elif trigger is not None:
                    # The trigger is the active control item.  The visible
                    # turns before it remain eligible history, not an
                    # accidentally protected final turn.
                    candidate = source
                    active = ()
                else:
                    units = split_atomic_units(source)
                    active = units[-1].items if units else ()
                    candidate = [
                        item
                        for unit in units[:-1]
                        for item in unit.items
                    ]
                return self._compact(
                    provider=provider,
                    model=model,
                    requested_slug=requested_slug,
                    body=body,
                    budget=budget,
                    candidate=candidate,
                    active=active,
                    suffix=(trigger,) if trigger is not None else (),
                    source_boundary={"kind": "full_visible_request"},
                )
            except HistoryCompactionError as exc:
                raise HistoryReconstructionError(exc.reason) from None
            except PortableCheckpointError:
                raise HistoryReconstructionError("history_compaction_failed") from None

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
            source = list(_input_items(body))
            boundary = _opaque_compaction_index(body)
            history = (
                build_visible_history(snapshot.items)
                if forced
                else build_compaction_replacement(snapshot.items)
            )
            trigger = _trailing_compaction_trigger(body)
            tail = source[boundary + 1 :]
            if trigger is not None and tail and tail[-1] == trigger:
                tail = tail[:-1]
            rendered = [
                *copy.deepcopy(source[:boundary]),
                *_render(history),
                *copy.deepcopy(tail),
                *(copy.deepcopy([trigger]) if trigger is not None else []),
            ]
            projected = copy.deepcopy(dict(body))
            projected["input"] = rendered
            fixed = {
                key: projected[key]
                for key in ("input", "instructions", "tools", "text", "response_format")
                if key in projected
            }
            budget = _safe_budget(config, provider, model, requested_slug, body)
            if _estimate(fixed) <= budget:
                result = self.compactor.no_compaction(projected, budget, _estimate(fixed))
                self.last_compaction_metrics = result.metrics
                return projected
            # Everything after Codex's opaque boundary belongs to the
            # request-local active tail.  Keep it byte-for-byte intact; only
            # the older Codex-visible replacement is eligible for compaction.
            active = tail
            candidate = _render(history)
            return self._compact(
                provider=provider,
                model=model,
                requested_slug=requested_slug,
                body=projected,
                budget=budget,
                candidate=candidate,
                active=active,
                prefix=source[:boundary],
                suffix=(trigger,) if trigger is not None else (),
                source_boundary={
                    "anchor": anchor,
                    "cursor": snapshot.cursor,
                    "source": snapshot.source,
                    "source_model": snapshot.source_model,
                },
            )
        except HistoryReconstructionError:
            raise
        except HistoryCompactionError as exc:
            raise HistoryReconstructionError(exc.reason) from None
        except PortableCheckpointError as exc:
            raise HistoryReconstructionError(exc.code) from None
        except Exception:
            raise HistoryReconstructionError("checkpoint_invalid") from None

    def _compact(
        self,
        *,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        requested_slug: str,
        body: Mapping[str, Any],
        budget: int,
        candidate: Sequence[Any],
        active: Sequence[Any],
        prefix: Sequence[Any] = (),
        suffix: Sequence[Any] = (),
        source_boundary: Any = None,
    ) -> Dict[str, Any]:
        result = self.compactor.compact(
            provider=provider,
            model=model,
            protocol=_protocol(provider, model),
            requested_slug=requested_slug,
            body=body,
            safe_budget=budget,
            candidate_items=candidate,
            active_request=active,
            prefix_items=prefix,
            suffix_items=suffix,
            source_boundary=source_boundary,
        )
        self.last_compaction_metrics = result.metrics
        if result.status == COMPACTION_COMPACTED and result.body is not None:
            return result.body
        if result.status == COMPACTION_NO_COMPACTION and result.body is not None:
            return result.body
        reason = result.reason or "history_compaction_failed"
        if self.compactor.summarizer is None:
            reason = "context_budget_exceeded"
        if result.status != COMPACTION_FAIL_CLOSED:
            reason = "history_compaction_failed"
        raise HistoryReconstructionError(reason)


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
