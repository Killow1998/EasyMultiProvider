"""Shared bounded errors for routing and protocol modules."""

from __future__ import annotations

from typing import Any, Dict, Optional

from .context_guard import format_context_error


class RouterError(Exception):
    def __init__(self, message: str, status: int = 400):
        super().__init__(message)
        self.status = status


class ExternalProtocolError(RouterError):
    """A bounded external response violation that must not enter history."""

    def __init__(self, message: str):
        self.error_class = "protocol_error"
        super().__init__(message, 502)


class ContextHandoffRequiredError(RouterError):
    """A content-free, recoverable failure for unprojectable opaque history."""

    def __init__(
        self,
        index: int,
        item_type: str = "compaction",
        reason: str = "binding_missing",
    ):
        self.error_class = "context_handoff_required"
        self.reason = str(reason or "binding_missing")[:64]
        self.item_index = max(0, int(index))
        self.item_type = str(item_type or "unknown")[:64]
        super().__init__(
            "context_handoff_required: reason=%s index=%d type=%s; "
            "switch back to the source model, continue or compact once while EMP "
            "is active, then retry"
            % (self.reason, self.item_index, self.item_type),
            409,
        )


class ContextLengthError(RouterError):
    """Normalized, non-retryable context-length failure."""

    def __init__(
        self,
        observation: Optional[Dict[str, Any]] = None,
        status: int = 413,
        preflight: bool = False,
    ):
        self.context_observation = dict(observation or {})
        self.preflight = bool(preflight)
        super().__init__(format_context_error(self.context_observation), status)


class StreamBoundaryError(RouterError):
    """A content-free stream lifecycle failure with a normalized class."""

    def __init__(self, message: str, error_class: str, status: int = 502):
        self.error_class = error_class
        super().__init__(message, status)
