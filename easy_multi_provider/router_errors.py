"""Shared bounded errors for routing and protocol modules."""

from __future__ import annotations

from typing import Any, Dict, Optional

from .context_guard import format_context_error


class RouterError(Exception):
    def __init__(self, message: str, status: int = 400):
        super().__init__(message)
        self.status = status


class UpstreamHTTPError(RouterError):
    """An upstream rejection with a content-free diagnostic category."""

    _REASONS = frozenset(
        {
            "auth_rejected",
            "context_length_exceeded",
            "quota_exhausted",
            "rate_limited",
            "request_too_large",
            "upstream_capacity",
            "upstream_rejected",
            "upstream_unavailable",
        }
    )

    def __init__(self, message: str, status: int, failure_reason: str):
        self.failure_reason = (
            failure_reason
            if failure_reason in self._REASONS
            else "upstream_rejected"
        )
        super().__init__(message, status)


class ExternalProtocolError(RouterError):
    """A bounded external response violation that must not enter history."""

    def __init__(self, message: str):
        self.error_class = "protocol_error"
        super().__init__(message, 502)


class ExternalCompactionError(RouterError):
    """A content-free failure of external model-owned summarization."""

    _REASONS = frozenset(
        {"invalid_response", "summary_empty", "summary_too_large"}
    )

    def __init__(self, reason: str):
        self.error_class = "external_compaction_failed"
        self.failure_reason = reason if reason in self._REASONS else "invalid_response"
        super().__init__(
            "external_compaction_failed: reason=%s" % self.failure_reason,
            502,
        )


class HistoryReconstructionError(RouterError):
    """A deterministic, content-free visible-history reconstruction failure."""

    def __init__(self, reason: str = "history_unavailable"):
        safe = str(reason or "history_unavailable").strip().lower()
        safe = "".join(character if character.isalnum() or character == "_" else "_" for character in safe)
        self.reason = (safe or "history_unavailable")[:64]
        self.error_class = "history_reconstruction_failed"
        super().__init__(
            "history_reconstruction_failed: reason=%s; Codex history was not modified"
            % self.reason,
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
