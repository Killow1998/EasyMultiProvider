"""Request-scoped Codex routing orchestration, separate from the HTTP server."""

from __future__ import annotations

import time
from typing import Any, Callable, Dict, Mapping, Optional, Tuple

from .codex_history import HistoryError
from .context_guard import ContextGuardBlocked
from .history_continuity import request_history_anchor
from .protocol_adapters import protocol_adapter
from .provider_replay import ProviderReplayScope
from .route_plan import ResolvedRoute, resolve_route
from .router import (
    RouterError,
    prepare_native_websocket_request,
    proxy,
    proxy_compact,
)
from .transport_failures import failure_from_exception


def provider_replay_scope(
    route: ResolvedRoute,
    body: Mapping[str, Any],
    incoming: Mapping[str, str],
) -> Optional[ProviderReplayScope]:
    """Resolve a fail-closed, content-free scope for opaque Provider state."""

    model_id = body.get("model")
    if model_id != route.requested_model:
        return None
    if not protocol_adapter(route.dialect).replay_safe:
        return None
    try:
        anchor = request_history_anchor(body, incoming)
    except HistoryError:
        return None
    if not anchor.thread_id:
        return None
    try:
        return ProviderReplayScope(
            provider_id=route.provider_id,
            endpoint_fingerprint=route.endpoint_fingerprint,
            deployment_identity=route.deployment_identity,
            model_id=model_id,
            upstream_model=route.upstream_model,
            thread_id=anchor.thread_id,
            window_id=anchor.window_id or "",
        )
    except ValueError:
        return None


class CodexRequestDispatcher:
    """Own route/history/context/replay orchestration for one application state."""

    def __init__(
        self,
        routing_snapshot: Callable[[], Dict[str, Any]],
        context_guard: Any,
        provider_replay: Any,
        history_continuity: Any,
        destination_context: Any,
        record_route_event: Callable[..., None],
        record_route_failure: Callable[..., None],
        diagnostic_stream: Callable[..., Any],
    ) -> None:
        self._routing_snapshot = routing_snapshot
        self.context_guard = context_guard
        self.provider_replay = provider_replay
        self.history_continuity = history_continuity
        self.destination_context = destination_context
        self._record_route_event = record_route_event
        self._record_route_failure = record_route_failure
        self._diagnostic_stream = diagnostic_stream

    def _context_check(self, completeness: str):
        def check(provider, model, protocol, payload, stream, operation):
            assessment = self.context_guard.assess(
                provider,
                model,
                protocol,
                payload,
                completeness,
            )
            observation = assessment.to_safe_dict()
            if assessment.decision == "block":
                raise ContextGuardBlocked(assessment)
            return observation

        return check

    @staticmethod
    def _route(snapshot: Dict[str, Any], body: Mapping[str, Any]) -> ResolvedRoute:
        model_id = body.get("model")
        if not isinstance(model_id, str) or not model_id:
            raise RouterError("request.model is required")
        return resolve_route(snapshot, model_id)

    def _record_failure(
        self,
        exc: BaseException,
        body: Dict[str, Any],
        started: float,
        transport: str,
        route: str,
    ) -> None:
        failure = failure_from_exception(exc)
        self._record_route_failure(
            body,
            started,
            transport,
            route,
            failure.status,
            getattr(exc, "context_observation", None),
            failure.error_class,
            failure_reason=failure.failure_reason,
        )

    def prepare_native_websocket(
        self,
        body: Dict[str, Any],
        incoming: Dict[str, str],
        context_completeness: str,
        *,
        transport_incremental: bool = False,
        transport_probe: bool = False,
    ):
        started = time.monotonic()
        try:
            snapshot = self._routing_snapshot()
            route = self._route(snapshot, body)
            plan = prepare_native_websocket_request(
                snapshot,
                body,
                incoming,
                on_context=(
                    None
                    if transport_probe
                    else self._context_check(context_completeness)
                ),
                history_preparer=(
                    None
                    if transport_incremental
                    else self.history_continuity.prepare
                ),
                destination_compactor=(
                    None if transport_probe else self.destination_context.compact
                ),
                transport_incremental=transport_incremental,
                resolved_route=route,
            )
        except RouterError as exc:
            self._record_failure(
                exc, body, started, "websocket", "responses"
            )
            raise
        prepare_ms = max(0, int(round((time.monotonic() - started) * 1000)))
        return plan, started, prepare_ms

    def route(
        self,
        body: Dict[str, Any],
        incoming: Dict[str, str],
        transport: Optional[str] = None,
        context_completeness: str = "high",
    ) -> Tuple[Dict[str, Any], Any]:
        snapshot = self._routing_snapshot()
        started = time.monotonic()
        selected_transport = transport or (
            "sse" if body.get("stream") else "http"
        )
        observed = False
        replay_scope = None

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            self._record_route_event(
                event, body, started, selected_transport, "responses"
            )

        try:
            route = self._route(snapshot, body)
            replay_scope = provider_replay_scope(route, body, incoming)
            body = self.provider_replay.prepare(body, replay_scope)
            metadata, result = proxy(
                snapshot,
                body,
                incoming,
                on_observation,
                self._context_check(context_completeness),
                history_preparer=self.history_continuity.prepare,
                destination_compactor=self.destination_context.compact,
                resolved_route=route,
            )
        except Exception as exc:
            if not observed:
                self._record_failure(
                    exc, body, started, selected_transport, "responses"
                )
            raise
        if metadata.get("kind") == "stream":
            if not metadata.get("observation_attached"):
                result = self._diagnostic_stream(
                    result,
                    metadata,
                    body,
                    started,
                    selected_transport,
                    "responses",
                )
            result = self.provider_replay.observe_stream(replay_scope, result)
        else:
            self.provider_replay.observe_bytes(replay_scope, result)
            if not observed:
                event = dict(metadata)
                event["response_bytes"] = (
                    len(result) if isinstance(result, (bytes, bytearray)) else 0
                )
                self._record_route_event(
                    event, body, started, selected_transport, "responses"
                )
        return metadata, result

    def route_compact(
        self,
        body: Dict[str, Any],
        incoming: Dict[str, str],
        transport: Optional[str] = None,
        context_completeness: str = "high",
    ) -> Tuple[Dict[str, Any], bytes]:
        started = time.monotonic()
        selected_transport = transport or "http"
        observed = False

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            self._record_route_event(
                event, body, started, selected_transport, "compact"
            )

        try:
            snapshot = self._routing_snapshot()
            route = self._route(snapshot, body)
            metadata, result = proxy_compact(
                snapshot,
                body,
                incoming,
                on_observation,
                self._context_check(context_completeness),
                history_preparer=self.history_continuity.prepare,
                destination_compactor=self.destination_context.compact,
                resolved_route=route,
            )
        except Exception as exc:
            if not observed:
                self._record_failure(
                    exc, body, started, selected_transport, "compact"
                )
            raise
        if not observed:
            event = dict(metadata)
            event["response_bytes"] = (
                len(result) if isinstance(result, (bytes, bytearray)) else 0
            )
            self._record_route_event(
                event, body, started, selected_transport, "compact"
            )
        return metadata, result
