"""Request-scoped Codex routing orchestration, separate from the HTTP server."""

from __future__ import annotations

import time
from typing import Any, Callable, Dict, Mapping, Optional, Tuple

from .diagnostic_journal import request_source, exception_details, NullJournal
from .codex_history import HistoryError
from .context_guard import ContextGuardBlocked
from .history_continuity import request_history_anchor
from .protocol_adapters import protocol_adapter
from .provider_replay import ProviderReplayScope
from .performance import ResponsesPerformanceTracker
from .usage_ledger import usage_context
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
        journal=None,
    ) -> None:
        self._routing_snapshot = routing_snapshot
        self.context_guard = context_guard
        self.provider_replay = provider_replay
        self.history_continuity = history_continuity
        self.destination_context = destination_context
        self._record_route_event = record_route_event
        self._record_route_failure = record_route_failure
        self._diagnostic_stream = diagnostic_stream
        self.journal = journal if journal is not None else NullJournal()

    def _history_callbacks(self, body, incoming):
        source = request_source(body, incoming)
        source["model_id"] = body.get("model")
        try:
            anchor = request_history_anchor(body, incoming)
            if anchor.turn_id:
                source["turn_ref"] = self.journal.pseudonym(anchor.turn_id)
        except Exception:
            pass

        def emit(stage, result, **facts):
            try:
                self.journal.event("warning" if result == "failed" else "info",
                                   "history_phase", **source, stage=stage, result=result, **facts)
            except Exception:
                pass

        def failed(stage, started, exc):
            failure = failure_from_exception(exc)
            emit(stage, "failed", duration_ms=round((time.monotonic() - started) * 1000),
                 error_class=failure.error_class, reason=failure.failure_reason,
                 status=failure.status, exception_chain=exception_details(exc))

        def prepare(config, provider, model, slug, request, headers):
            started = time.monotonic()
            emit("prepare", "started")
            try:
                projected = self.history_continuity.prepare(config, provider, model, slug, request, headers,
                                                            on_diagnostic=emit)
            except Exception as exc:
                failed("prepare", started, exc)
                raise
            emit("prepare", "completed", duration_ms=round((time.monotonic() - started) * 1000),
                 input_items=len(request["input"]) if isinstance(request.get("input"), list) else 1,
                 projected_items=len(projected["input"]) if isinstance(projected.get("input"), list) else 1)
            return projected

        def compact(provider, model, slug, request, assessment):
            started = time.monotonic()
            emit("compact", "started", context=assessment.to_safe_dict())
            try:
                result = self.destination_context.compact(provider, model, slug, request, assessment,
                                                          on_diagnostic=emit)
            except Exception as exc:
                failed("compact", started, exc)
                raise
            emit("compact", "completed", duration_ms=round((time.monotonic() - started) * 1000))
            return result

        return prepare, compact

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
        source=None,
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
            source=source,
        )

    def _log_failure(self, exc, source, started):
        failure = failure_from_exception(exc)
        try:
            self.journal.event("warning", "request_failure", **source,
                               phase=failure.phase, status=failure.status,
                               error_class=failure.error_class, reason=failure.failure_reason,
                               duration_ms=round((time.monotonic() - started) * 1000),
                               exception_chain=exception_details(exc))
        except Exception:
            pass

    def _log_stream_failures(self, result, source, started):
        try:
            yield from result
        except Exception as exc:
            self._log_failure(exc, source, started)
            raise
        finally:
            close = getattr(result, "close", None)
            if callable(close):
                try:
                    close()
                except Exception:
                    pass

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
        history_prepare, history_compact = self._history_callbacks(body, incoming)
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
                    else history_prepare
                ),
                destination_compactor=(
                    None if transport_probe else history_compact
                ),
                transport_incremental=transport_incremental,
                resolved_route=route,
            )
        except RouterError as exc:
            self._log_failure(exc, request_source(body, incoming), started)
            self._record_failure(
                exc, body, started, "websocket", "responses", request_source(body, incoming)
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
        performance = ResponsesPerformanceTracker(started=started)
        source = {**request_source(body, incoming), **usage_context(body, incoming)}
        observed = False
        replay_scope = None
        history_prepare, history_compact = self._history_callbacks(body, incoming)

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            event = {**event, **performance.diagnostics(), **source}
            self._record_route_event(
                event, body, started, selected_transport, "responses"
            )

        try:
            route = self._route(snapshot, body)
            replay_scope = provider_replay_scope(route, body, incoming)
            body = self.provider_replay.prepare(body, replay_scope)
            performance.mark_upstream_started()
            metadata, result = proxy(
                snapshot,
                body,
                incoming,
                on_observation,
                self._context_check(context_completeness),
                history_preparer=history_prepare,
                destination_compactor=history_compact,
                resolved_route=route,
            )
        except Exception as exc:
            self._log_failure(exc, source, started)
            if not observed:
                self._record_failure(
                    exc, body, started, selected_transport, "responses", source
                )
            raise
        metadata.update(source)
        if metadata.get("kind") == "stream":
            if not metadata.get("observation_attached"):
                result = self._diagnostic_stream(
                    result,
                    metadata,
                    body,
                    started,
                    selected_transport,
                    "responses",
                    performance=performance,
                )
            result = self.provider_replay.observe_stream(replay_scope, result)
            result = performance.observe_stream(result)
            result = self._log_stream_failures(result, source, started)
        else:
            self.provider_replay.observe_bytes(replay_scope, result)
            performance.observe_bytes(result)
            if not observed:
                event = {**metadata, **performance.diagnostics()}
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
        source = {**request_source(body, incoming), **usage_context(body, incoming)}
        observed = False

        history_prepare, history_compact = self._history_callbacks(body, incoming)

        def on_observation(event: Dict[str, Any]) -> None:
            nonlocal observed
            observed = True
            self._record_route_event(
                {**event, **source}, body, started, selected_transport, "compact"
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
                history_preparer=history_prepare,
                destination_compactor=history_compact,
                resolved_route=route,
            )
        except Exception as exc:
            self._log_failure(exc, source, started)
            if not observed:
                self._record_failure(
                    exc, body, started, selected_transport, "compact", source
                )
            raise
        if not observed:
            event = dict(metadata)
            event["response_bytes"] = (
                len(result) if isinstance(result, (bytes, bytearray)) else 0
            )
            self._record_route_event(
                {**event, **source}, body, started, selected_transport, "compact"
            )
        return metadata, result
