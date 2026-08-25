"""Read visible turns from an already reachable Codex App Server."""

from __future__ import annotations

import inspect
import json
from collections.abc import Mapping
from dataclasses import replace
from typing import Any, Callable, Optional

from .models import (
    CodexHistoryReader,
    HistoryAnchor,
    HistoryCursor,
    HistoryError,
    HistoryMismatchError,
    HistorySnapshot,
    HistoryUnavailableError,
    HistoryUnsupportedError,
    normalize_visible_item,
)


def _value(mapping: Mapping, *keys: str) -> Any:
    for key in keys:
        if key in mapping:
            return mapping.get(key)
    return None


def _string(mapping: Mapping, *keys: str) -> Optional[str]:
    value = _value(mapping, *keys)
    return value if isinstance(value, str) and value else None


def _integer(mapping: Mapping, *keys: str) -> Optional[int]:
    value = _value(mapping, *keys)
    if isinstance(value, bool):
        return None
    if isinstance(value, int) and value >= 0:
        return value
    if isinstance(value, str) and value.strip().isdigit():
        return int(value.strip())
    return None


def _model(mapping: Mapping) -> Optional[str]:
    value = _value(
        mapping,
        "model",
        "model_id",
        "modelId",
        "selected_model",
        "selectedModel",
    )
    return value.strip() if isinstance(value, str) and value.strip() else None


def _decode_response(value: Any) -> Mapping:
    if isinstance(value, bytes):
        try:
            value = value.decode("utf-8")
        except UnicodeDecodeError:
            raise HistoryUnsupportedError("invalid_rpc_response", source="app_server", fallback=True) from None
    if isinstance(value, str):
        try:
            value = json.loads(value)
        except (TypeError, ValueError):
            raise HistoryUnsupportedError("invalid_rpc_response", source="app_server", fallback=True) from None
    if not isinstance(value, Mapping):
        raise HistoryUnsupportedError("invalid_rpc_response", source="app_server", fallback=True)
    return value


def _invoke(caller: Any, method: str, params: Mapping, timeout: float) -> Any:
    if hasattr(caller, "call"):
        target = caller.call
    elif hasattr(caller, "request"):
        target = caller.request
    elif hasattr(caller, "send"):
        target = caller.send
    else:
        target = caller
    if not callable(target):
        raise TypeError("caller is not callable")
    try:
        signature = inspect.signature(target)
    except (TypeError, ValueError):
        return target(method, params)
    parameters = list(signature.parameters.values())
    accepts_kwargs = any(parameter.kind == parameter.VAR_KEYWORD for parameter in parameters)
    if accepts_kwargs or "timeout" in signature.parameters:
        return target(method, params, timeout=timeout)
    positional = [
        parameter
        for parameter in parameters
        if parameter.kind in (parameter.POSITIONAL_ONLY, parameter.POSITIONAL_OR_KEYWORD)
    ]
    if len(positional) >= 3:
        return target(method, params, timeout)
    return target(method, params)


def _fallback(reader: Any, anchor: HistoryAnchor, error: Exception) -> HistorySnapshot:
    if reader is None:
        raise error
    try:
        target = reader.read_visible_history if hasattr(reader, "read_visible_history") else reader
    except Exception:
        raise HistoryUnavailableError("fallback_unavailable", source="app_server") from None
    if not callable(target):
        raise HistoryUnsupportedError("invalid_fallback", source="app_server") from None
    try:
        signature = inspect.signature(target)
        parameters = list(signature.parameters.values())
        accepts_kwargs = any(parameter.kind == parameter.VAR_KEYWORD for parameter in parameters)
    except Exception:
        accepts_kwargs = False
    try:
        if accepts_kwargs:
            snapshot = target(anchor, fallback_from=error)
        else:
            snapshot = target(anchor)
    except HistoryError:
        raise
    except Exception:
        raise HistoryUnavailableError("fallback_unavailable", source="app_server") from None
    if not isinstance(snapshot, HistorySnapshot):
        raise HistoryUnsupportedError("invalid_fallback", source="app_server") from None
    if snapshot.thread_id != anchor.thread_id:
        raise HistoryMismatchError("thread_mismatch", source="app_server")
    try:
        return replace(snapshot, fallback=True)
    except HistoryError:
        raise
    except Exception:
        raise HistoryUnavailableError("fallback_unavailable", source="app_server") from None


class AppServerReader:
    """Primary reader using an injected JSON-RPC caller.

    The caller is deliberately a seam.  This class does not know how Codex's
    App Server is started, discovered, or owned, and it never starts one.
    """

    def __init__(
        self,
        caller: Optional[Callable[..., Any]] = None,
        *,
        rpc_caller: Optional[Callable[..., Any]] = None,
        rpc_call: Optional[Callable[..., Any]] = None,
        transport: Optional[Callable[..., Any]] = None,
        fallback: Optional[CodexHistoryReader] = None,
        fallback_reader: Optional[CodexHistoryReader] = None,
        timeout: float = 2.0,
    ) -> None:
        self.caller = caller or rpc_caller or rpc_call or transport
        self.fallback = fallback or fallback_reader
        try:
            timeout = float(timeout)
        except (TypeError, ValueError):
            timeout = 2.0
        self.timeout = max(0.01, min(timeout, 30.0))

    def read_visible_history(self, anchor: HistoryAnchor) -> HistorySnapshot:
        if not isinstance(anchor, HistoryAnchor):
            raise HistoryUnsupportedError("invalid_anchor", source="app_server", fallback=True)
        if not anchor.thread_id:
            error = HistoryUnavailableError(
                "thread_identity_missing", source="app_server", fallback=True
            )
            return _fallback(self.fallback, anchor, error)
        if self.caller is None:
            error = HistoryUnavailableError(
                "app_server_unavailable", source="app_server", fallback=True
            )
            return _fallback(self.fallback, anchor, error)

        params = {"threadId": anchor.thread_id, "includeTurns": True}
        try:
            response = _invoke(self.caller, "thread/read", params, self.timeout)
            snapshot = self._normalize(anchor, _decode_response(response))
            return snapshot
        except (HistoryMismatchError,) as error:
            raise error
        except (HistoryUnavailableError, HistoryUnsupportedError) as error:
            return _fallback(self.fallback, anchor, error)
        except Exception:
            error = HistoryUnavailableError(
                "app_server_unavailable", source="app_server", fallback=True
            )
            return _fallback(self.fallback, anchor, error)

    def read(self, anchor: HistoryAnchor) -> HistorySnapshot:
        return self.read_visible_history(anchor)

    @staticmethod
    def _normalize(anchor: HistoryAnchor, response: Mapping) -> HistorySnapshot:
        if response.get("error") is not None:
            raise HistoryUnavailableError("rpc_error", source="app_server", fallback=True)
        result = response.get("result", response)
        if not isinstance(result, Mapping):
            raise HistoryUnsupportedError("invalid_rpc_result", source="app_server", fallback=True)
        thread = result.get("thread")
        if thread is None:
            thread = result
        if not isinstance(thread, Mapping):
            raise HistoryUnsupportedError("thread_missing", source="app_server", fallback=True)

        observed_thread_id = _string(thread, "id", "thread_id", "threadId")
        if observed_thread_id is None:
            observed_thread_id = _string(result, "thread_id", "threadId")
        if observed_thread_id is None:
            raise HistoryUnsupportedError(
                "thread_identity_missing", source="app_server", fallback=True
            )
        if observed_thread_id != anchor.thread_id:
            raise HistoryMismatchError("thread_mismatch", source="app_server")

        turns = thread.get("turns")
        if turns is None:
            turns = result.get("turns")
        if not isinstance(turns, list):
            raise HistoryUnsupportedError("full_turns_missing", source="app_server", fallback=True)

        if anchor.turn_id:
            anchor_indexes = [
                index
                for index, turn in enumerate(turns)
                if isinstance(turn, Mapping)
                and _string(turn, "id", "turn_id", "turnId") == anchor.turn_id
            ]
            if len(anchor_indexes) != 1:
                raise HistoryUnsupportedError(
                    "turn_not_found", source="app_server", fallback=True
                )
            turns = turns[: anchor_indexes[0]]

        items = []
        turn_models = []
        highest_ordinal = _integer(
            thread,
            "highestOrdinal",
            "highest_ordinal",
            "rolloutOrdinal",
            "rollout_ordinal",
        )
        if highest_ordinal is None:
            highest_ordinal = _integer(
                result,
                "highestOrdinal",
                "highest_ordinal",
                "rolloutOrdinal",
                "rollout_ordinal",
            )
        projection_offset = _integer(
            thread, "projectionOffset", "projection_offset", "offset"
        )
        if projection_offset is None:
            projection_offset = _integer(
                result, "projectionOffset", "projection_offset", "offset"
            )
        for turn in turns:
            if not isinstance(turn, Mapping) or not isinstance(turn.get("items"), list):
                raise HistoryUnsupportedError(
                    "full_turn_items_missing", source="app_server", fallback=True
                )
            status = turn.get("status")
            if status not in ("completed", "interrupted", "failed", "inProgress"):
                raise HistoryUnsupportedError(
                    "turn_status_missing", source="app_server", fallback=True
                )
            if status != "completed":
                continue
            items_view = turn.get("itemsView", turn.get("items_view", "full"))
            if items_view != "full":
                raise HistoryUnsupportedError(
                    "full_turn_items_missing", source="app_server", fallback=True
                )
            turn_id = _string(turn, "id", "turn_id", "turnId")
            turn_model = _model(turn)
            if turn_model is None:
                for candidate in turn["items"]:
                    if not isinstance(candidate, Mapping):
                        continue
                    context = candidate.get("item")
                    context = context if isinstance(context, Mapping) else candidate
                    context_type = str(context.get("type") or "").lower().replace(
                        "-", "_"
                    )
                    if context_type not in ("turn_context", "turncontext"):
                        continue
                    payload = context.get("payload")
                    payload = payload if isinstance(payload, Mapping) else context
                    turn_model = _model(payload)
                    if turn_model is not None:
                        break
            turn_models.append((turn_id, turn_model))
            for raw_item in turn["items"]:
                if not isinstance(raw_item, Mapping):
                    raise HistoryUnsupportedError(
                        "invalid_turn_item", source="app_server", fallback=True
                    )
                item = raw_item
                if isinstance(raw_item.get("item"), Mapping):
                    item = raw_item["item"]
                if item.get("type") in ("response_item", "event_msg") and isinstance(
                    item.get("payload"), Mapping
                ):
                    item = item["payload"]
                ordinal = _integer(
                    raw_item,
                    "ordinal",
                    "rolloutOrdinal",
                    "rollout_ordinal",
                    "sequence",
                )
                if ordinal is None:
                    ordinal = _integer(item, "ordinal", "rolloutOrdinal", "rollout_ordinal")
                offset = _integer(
                    raw_item, "projectionOffset", "projection_offset", "offset"
                )
                normalized = normalize_visible_item(
                    item, turn_id=turn_id, ordinal=ordinal, offset=offset
                )
                if normalized is not None:
                    items.append(normalized)
                    if normalized.ordinal is not None:
                        highest_ordinal = max(
                            highest_ordinal or normalized.ordinal, normalized.ordinal
                        )
                    if normalized.offset is not None:
                        projection_offset = max(
                            projection_offset or normalized.offset, normalized.offset
                        )

        cursor = HistoryCursor(
            kind="paginated",
            thread_id=anchor.thread_id,
            highest_ordinal=highest_ordinal,
            projection_offset=projection_offset,
        )
        source_model = next(
            (model for _, model in reversed(turn_models) if model is not None),
            None,
        )
        return HistorySnapshot(
            anchor=anchor,
            items=tuple(items),
            cursor=cursor,
            source="app_server",
            source_model=source_model,
        )


__all__ = ["AppServerReader"]
