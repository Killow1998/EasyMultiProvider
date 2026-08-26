"""Destination compaction after protocol-independent history materialization."""

from __future__ import annotations

import copy
from collections.abc import Mapping, Sequence
from typing import Any, Dict, Optional

from .context_guard import ContextAssessment
from .history_compaction import (
    COMPACTION_COMPACTED,
    COMPACTION_NO_COMPACTION,
    DestinationSummarizer,
    HistoryCompactor,
    split_atomic_units,
)
from .history_continuity import ACTIVE_INPUT_START_KEY
from .router_errors import HistoryReconstructionError


_CONCRETE_PROTOCOLS = frozenset(
    {"responses", "chat_completions", "anthropic_messages"}
)


def _protocol(provider: Mapping[str, Any], model: Mapping[str, Any]) -> str:
    configured = provider.get("protocol")
    if configured in _CONCRETE_PROTOCOLS:
        return str(configured)
    for source in (model, provider):
        observed = source.get("observed_protocol")
        if observed in _CONCRETE_PROTOCOLS:
            return str(observed)
        capabilities = source.get("observed_capabilities")
        if isinstance(capabilities, Mapping):
            observed = capabilities.get("protocol")
            if observed in _CONCRETE_PROTOCOLS:
                return str(observed)
    return "responses"


def _items(body: Mapping[str, Any]) -> list[Any]:
    value = body.get("input")
    if isinstance(value, list):
        return copy.deepcopy(value)
    if isinstance(value, (Mapping, str)):
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


class DestinationContextCompactor:
    """Shrink one already materialized logical request to a Guard budget."""

    def __init__(self, summarizer: Optional[DestinationSummarizer] = None) -> None:
        self.compactor = HistoryCompactor(summarizer)

    def compact(
        self,
        provider: Mapping[str, Any],
        model: Mapping[str, Any],
        requested_slug: str,
        body: Mapping[str, Any],
        assessment: ContextAssessment,
    ) -> Dict[str, Any]:
        safe_budget = _positive_int(assessment.safe_input_limit)
        if safe_budget is None:
            raise HistoryReconstructionError("context_budget_unknown")

        projected = copy.deepcopy(dict(body))
        active_start = projected.pop(ACTIVE_INPUT_START_KEY, None)
        source = _items(projected)
        suffix: Sequence[Any] = ()
        if (
            source
            and isinstance(source[-1], Mapping)
            and source[-1].get("type") == "compaction_trigger"
        ):
            suffix = (source.pop(),)

        if (
            isinstance(active_start, int)
            and not isinstance(active_start, bool)
            and 0 <= active_start <= len(source)
        ):
            candidate = source[:active_start]
            active = source[active_start:]
        elif suffix:
            candidate = source
            active = []
        else:
            units = split_atomic_units(source)
            active = list(units[-1].items) if units else []
            candidate = [
                item for unit in units[:-1] for item in unit.items
            ]

        result = self.compactor.compact(
            provider=provider,
            model=model,
            protocol=_protocol(provider, model),
            requested_slug=requested_slug,
            body=projected,
            safe_budget=safe_budget,
            candidate_items=candidate,
            active_request=active,
            suffix_items=suffix,
            source_boundary={
                "kind": "destination_payload",
                "active_start": active_start,
                "source_items": len(source),
            },
        )
        if result.status in {COMPACTION_COMPACTED, COMPACTION_NO_COMPACTION}:
            if result.body is not None:
                result.body.pop(ACTIVE_INPUT_START_KEY, None)
                return result.body
        raise HistoryReconstructionError(
            result.reason or "history_compaction_failed"
        )


def strip_history_accounting(body: Mapping[str, Any]) -> Dict[str, Any]:
    """Remove request-local identity fields before protocol projection."""

    projected = copy.deepcopy(dict(body))
    projected.pop(ACTIVE_INPUT_START_KEY, None)
    return projected


__all__ = ["DestinationContextCompactor", "strip_history_accounting"]
