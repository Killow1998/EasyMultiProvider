"""Content-free decisions for one Responses WebSocket transport chain.

This module deliberately knows nothing about Codex history, compaction, context
budgets, or request content.  It only decides whether an incremental request can
still use the live upstream chain that produced its ``previous_response_id``.
"""

from __future__ import annotations

from dataclasses import dataclass
from enum import Enum
from typing import Any, Mapping, Optional


PREVIOUS_RESPONSE_NOT_FOUND_CODE = "previous_response_not_found"
PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE = (
    "Previous response was not found. Retrying the full request."
)


class TransportContinuityDecision(str, Enum):
    CONTINUE_INCREMENTAL = "continue_incremental"
    FULL_REQUEST = "full_request"
    PREVIOUS_RESPONSE_NOT_FOUND = "previous_response_not_found"


@dataclass(frozen=True)
class TransportContinuityState:
    """Only live connection identity; never prompt or response content."""

    current_route_identity: Optional[str]
    live_route_identity: Optional[str]
    previous_response_id: Optional[str]
    live_previous_response_id: Optional[str]
    upstream_incremental_capable: bool
    live_connection: bool


class TransportContinuityAdapter:
    """Classify a downstream WS request without materializing history."""

    def decide(
        self,
        request: Mapping[str, Any],
        state: TransportContinuityState,
    ) -> TransportContinuityDecision:
        previous = request.get("previous_response_id")
        if previous is None:
            return TransportContinuityDecision.FULL_REQUEST
        if not isinstance(previous, str) or not previous:
            return TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        if previous != state.previous_response_id:
            return TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        if not state.upstream_incremental_capable or not state.live_connection:
            return TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        if not state.current_route_identity or (
            state.current_route_identity != state.live_route_identity
        ):
            return TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        if previous != state.live_previous_response_id:
            return TransportContinuityDecision.PREVIOUS_RESPONSE_NOT_FOUND
        return TransportContinuityDecision.CONTINUE_INCREMENTAL


__all__ = [
    "PREVIOUS_RESPONSE_NOT_FOUND_CODE",
    "PREVIOUS_RESPONSE_NOT_FOUND_MESSAGE",
    "TransportContinuityAdapter",
    "TransportContinuityDecision",
    "TransportContinuityState",
]
