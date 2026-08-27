"""Bounded Responses stream lifecycle and external stream adapters."""

from __future__ import annotations

from dataclasses import dataclass
import json
import re
import uuid
from typing import Any, Callable, Dict, Iterable, Iterator, Mapping, Optional, Tuple
from urllib.error import URLError

from .context_guard import is_explicit_context_error, mark_explicit_failure
from .dialects import (
    custom_tool_ids,
    custom_tool_input,
    custom_tool_names,
)
from .protocol_projection import (
    _advance_textual_protocol_probe,
    _anthropic_incomplete_reason,
    _anthropic_tool_arguments,
    _chat_incomplete_reason,
    _content_text,
    _upstream_tool_arguments,
    _validate_textual_protocol,
    responses_terminal_observation,
    responses_to_anthropic,
    responses_to_chat,
    validate_responses_body,
)
from .router_errors import (
    ContextLengthError,
    ExternalProtocolError,
    RouterError,
    StreamBoundaryError,
)
from .transport import TransportError, sse_json_events
from .transport_failures import (
    CONNECT_TIMEOUT,
    PHASE_CONNECT,
    PROTOCOL_REJECTION_STATUSES,
    STREAM_INCOMPLETE,
    FailureSnapshot,
    StreamLifecycle,
    TransportFailure,
    event_activity,
    failure_from_exception,
    normalize_error_class,
    retry_allowed,
    status_error_class,
)


MAX_UPSTREAM_BODY_BYTES = 16 * 1024 * 1024
MAX_SSE_FRAME_BYTES = 1024 * 1024
MAX_PRE_OUTPUT_BUFFER_BYTES = 1024 * 1024
MAX_PRE_OUTPUT_BUFFER_EVENTS = 256
MAX_STREAM_TEXT_BYTES = 16 * 1024 * 1024
_PROTOCOL_REJECTION_STATUSES = PROTOCOL_REJECTION_STATUSES


@dataclass(frozen=True)
class StreamAdapterIO:
    request: Callable[..., Any]
    read_limited: Callable[..., bytes]
    raise_if_context_response: Callable[..., None]
    body_with_supported_effort: Callable[..., Dict[str, Any]]
    upstream_model: Callable[..., str]


def _route_error_class(status: Any) -> str:
    if status is None:
        return "network"
    if status == 402:
        return "payment_required"
    return status_error_class(status)


def _stream_exception(
    exc: BaseException,
    phase: Optional[str] = None,
    output_emitted: bool = False,
) -> Tuple[Optional[int], str]:
    failure = failure_from_exception(exc, phase, output_emitted)
    return failure.status, failure.error_class


def _terminal_exception(
    exc: BaseException,
    phase: Optional[str] = None,
    output_emitted: bool = False,
) -> Dict[str, Any]:
    if isinstance(exc, ContextLengthError):
        return {
            "success": False,
            "status": exc.status,
            "error_class": "context_length_exceeded",
            "context_observation": dict(exc.context_observation),
        }
    failure = failure_from_exception(exc, phase, output_emitted)
    terminal = {
        "success": False,
        "status": failure.status,
        "error_class": failure.error_class,
    }
    if failure.failure_reason:
        terminal["failure_reason"] = failure.failure_reason
    if isinstance(getattr(exc, "phase", None), str):
        terminal["phase"] = exc.phase
    return terminal


def _notify_terminal(
    callback: Optional[Callable[[Dict[str, Any]], None]],
    status: Any,
    error_class: str = "none",
    context_observation: Optional[Mapping[str, Any]] = None,
    diagnostics: Optional[Mapping[str, Any]] = None,
) -> None:
    if callback is None:
        return
    try:
        value = {
            "success": (
                error_class == "none"
                and isinstance(status, int)
                and 200 <= status < 300
            ),
            "status": status,
            "error_class": error_class,
        }
        if isinstance(context_observation, Mapping):
            value["context_observation"] = dict(context_observation)
        if isinstance(diagnostics, Mapping):
            for key in (
                "phase",
                "duration_ms",
                "upstream_first_event_ms",
                "retry_count",
                "output_emitted",
                "tool_activity",
                "terminal_event_observed",
            ):
                if key in diagnostics:
                    value[key] = diagnostics[key]
        callback(value)
    except Exception:
        pass


def _sse_frame(event: str, value: Dict[str, Any]) -> bytes:
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(value, ensure_ascii=False))).encode("utf-8")


def _content_free_failure_message(value: Any) -> str:
    """Keep stable category wording while dropping arbitrary upstream text."""

    text = str(value or "")
    if "textual reasoning/tool-call markup" in text:
        return "upstream returned textual reasoning/tool-call markup"
    if "empty Chat Completions response" in text:
        return "upstream returned an empty Chat Completions response"
    return "upstream stream failed"


def _response_failure_frame(
    message: str,
    status: int = 502,
    error_class: Optional[str] = None,
    failure_reason: Optional[str] = None,
    transport_failure: bool = False,
) -> bytes:
    failure_class = normalize_error_class(error_class or _route_error_class(status))
    safe_message = _content_free_failure_message(message)
    display_message = "HTTP %d: %s" % (status, safe_message)
    error_code = {
        "context_length_exceeded": "context_length_exceeded",
        "payment_required": "payment_required",
        "rate_limit": "rate_limit_exceeded",
    }.get(failure_class, "upstream_error")
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "failed",
        "error": {
            "code": error_code,
            "message": display_message,
            "status": status,
            "error_class": failure_class,
        },
    }
    if isinstance(failure_reason, str) and failure_reason:
        safe_reason = "".join(
            character
            if character.isalnum() or character in {"_", "-"}
            else "_"
            for character in failure_reason.lower()
        )[:64]
        if safe_reason:
            response["error"]["failure_reason"] = safe_reason
    if transport_failure:
        response["error"]["transport_failure"] = True
    return _sse_frame("response.failed", {"type": "response.failed", "response": response})


_TERMINAL_EVENT_TYPES = frozenset(
    {"response.completed", "response.incomplete", "response.failed", "error"}
)


def _stream_event_activity(event: Mapping[str, Any]) -> Tuple[bool, bool]:
    """Return visible-output and tool-activity flags without retaining content."""
    return event_activity(event)


def _stream_terminal(event: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    event_type = str(event.get("type") or "")
    if event_type not in _TERMINAL_EVENT_TYPES:
        return None

    def reason_value(value: Any) -> Optional[str]:
        if not isinstance(value, str) or not value:
            return None
        value = "".join(
            character if character.isalnum() or character in {"_", "-"} else "_"
            for character in value.lower()
        )
        return value[:64] or None

    def error_class(error: Mapping[str, Any], status: Any) -> str:
        candidate = error.get("error_class") or error.get("code")
        candidate = {
            "context_length_exceeded": "context_length_exceeded",
            "rate_limit_exceeded": "rate_limit",
            "payment_required": "payment_required",
            "protocol_rejection": "protocol_rejection",
            "incomplete": STREAM_INCOMPLETE,
        }.get(candidate, candidate)
        return normalize_error_class(candidate, _route_error_class(status))

    response = event.get("response")
    response = response if isinstance(response, Mapping) else {}
    if event_type == "response.completed":
        nested_status = response.get("status")
        nested_error = response.get("error")
        if nested_error not in (None, {}):
            error = nested_error if isinstance(nested_error, Mapping) else {}
            status = error.get("status")
            if isinstance(status, bool) or not isinstance(status, int):
                status = 502
            return {
                "success": False,
                "status": status,
                "error_class": error_class(error, status),
                **(
                    {"failure_reason": reason_value(error.get("failure_reason"))}
                    if reason_value(error.get("failure_reason"))
                    else {}
                ),
            }
        if nested_status == "incomplete":
            details = response.get("incomplete_details")
            reason = (
                str(details.get("reason") or "")
                if isinstance(details, Mapping)
                else ""
            )
            terminal = {
                "success": False,
                "status": 200,
                "error_class": {
                    "max_output_tokens": "output_limit",
                    "content_filter": "content_filter",
                }.get(reason, "stream_incomplete"),
            }
            if reason_value(reason):
                terminal["failure_reason"] = reason_value(reason)
            return terminal
        if nested_status == "failed":
            return {
                "success": False,
                "status": 502,
                "error_class": "stream_error",
            }
        if nested_status not in (None, "", "completed"):
            return {
                "success": False,
                "status": 502,
                "error_class": "malformed_terminal",
            }
        return {"success": True, "status": 200, "error_class": "none"}
    if event_type == "response.incomplete":
        details = response.get("incomplete_details")
        reason = str(details.get("reason") or "") if isinstance(details, Mapping) else ""
        terminal = {
            "max_output_tokens": "output_limit",
            "content_filter": "content_filter",
        }.get(reason, "stream_incomplete")
        result = {"success": False, "status": 200, "error_class": terminal}
        if reason_value(reason):
            result["failure_reason"] = reason_value(reason)
        return result
    error = response.get("error")
    if not isinstance(error, Mapping):
        error = event.get("error")
    error = error if isinstance(error, Mapping) else {}
    status = error.get("status")
    if not isinstance(status, int):
        message = str(error.get("message") or event.get("message") or "")
        match = re.search(r"HTTP\s+(\d{3})", message)
        status = int(match.group(1)) if match else 502
    terminal_class = error_class(error, status)
    close_code = error.get("close_code")
    terminal = {
        "success": False,
        "status": status,
        "error_class": terminal_class,
    }
    failure_reason = reason_value(error.get("failure_reason"))
    if failure_reason:
        terminal["failure_reason"] = failure_reason
    if isinstance(close_code, int) and not isinstance(close_code, bool):
        terminal["close_code"] = close_code
    return terminal


def _validate_terminal_payload(event: Mapping[str, Any]) -> None:
    """Validate complete terminal output, including structured tool JSON."""

    if str(event.get("type") or "") not in {
        "response.completed",
        "response.incomplete",
    }:
        return
    response = event.get("response")
    if not isinstance(response, Mapping) or "output" not in response:
        return
    validate_responses_body(response)


def _reliable_responses_stream(
    factory: Callable[[], Iterable[bytes]],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    replay_safe: bool = False,
) -> Iterator[bytes]:
    """Enforce terminal truth and one explicitly safe pre-output retry."""

    lifecycle = StreamLifecycle(replayable=replay_safe)
    recovery_attempted = False
    reported = False

    def report(terminal: Mapping[str, Any], terminal_observed: bool) -> None:
        nonlocal reported
        if reported:
            return
        reported = True
        value = dict(terminal)
        diagnostics = lifecycle.diagnostics(value)
        value.update(
            {
                **diagnostics,
                "terminal_event_observed": bool(
                    diagnostics["terminal_event_observed"] or terminal_observed
                ),
                "recovery_succeeded": bool(
                    recovery_attempted and terminal.get("success") is True
                ),
                "recovery_mode": "pre_output_retry" if recovery_attempted else "none",
            }
        )
        if terminal_callback is not None:
            try:
                terminal_callback(value)
            except Exception:
                pass

    try:
        for attempt in range(2 if replay_safe else 1):
            lifecycle.reset_attempt()
            pending = []
            pending_bytes = 0
            iterator = None
            try:
                iterator = iter(factory())
                lifecycle.mark_iterator_created()
                for event in sse_json_events(iterator):
                    lifecycle.observe_event(event)
                    terminal = _stream_terminal(event)
                    if terminal is not None:
                        terminal = lifecycle.observe_terminal(event, terminal)
                        _validate_terminal_payload(event)
                        report(terminal, True)
                        if terminal.get("success") is True:
                            for buffered in pending:
                                yield buffered
                            yield _sse_frame("response.completed", dict(event))
                        elif str(event.get("type") or "") == "response.incomplete":
                            for buffered in pending:
                                yield buffered
                            yield _sse_frame("response.incomplete", dict(event))
                        else:
                            yield _response_failure_frame(
                                "upstream stream failed",
                                int(terminal.get("status") or 502),
                                str(terminal.get("error_class") or "stream_error"),
                                terminal.get("failure_reason"),
                            )
                        return
                    if lifecycle.output_emitted or lifecycle.tool_activity:
                        for buffered in pending:
                            yield buffered
                        pending = []
                        pending_bytes = 0
                        yield _sse_frame(str(event.get("type") or "message"), dict(event))
                    else:
                        buffered = _sse_frame(
                            str(event.get("type") or "message"), dict(event)
                        )
                        if (
                            len(pending) >= MAX_PRE_OUTPUT_BUFFER_EVENTS
                            or pending_bytes + len(buffered) > MAX_PRE_OUTPUT_BUFFER_BYTES
                        ):
                            raise TransportError(
                                "upstream pre-output SSE buffer is too large"
                            )
                        pending.append(buffered)
                        pending_bytes += len(buffered)
                failure = lifecycle.incomplete()
                terminal = {
                    "success": False,
                    "status": failure.status,
                    "error_class": failure.error_class,
                }
                report(terminal, False)
                yield _response_failure_frame(
                    "upstream stream ended without a terminal event",
                    failure.status,
                    failure.error_class,
                )
                return
            except GeneratorExit:
                raise
            except Exception as exc:
                failure = failure_from_exception(
                    exc,
                    lifecycle.phase,
                    lifecycle.output_emitted,
                )
                if isinstance(exc, TransportError):
                    failure = FailureSnapshot(
                        "malformed_terminal", 502, lifecycle.phase
                    )
                if retry_allowed(
                    failure,
                    attempt,
                    replay_safe,
                    lifecycle.output_emitted,
                ):
                    recovery_attempted = True
                    lifecycle.retry_count += 1
                    continue
                lifecycle.phase = failure.phase
                terminal = {
                    "success": False,
                    "status": failure.status,
                    "error_class": failure.error_class,
                }
                if failure.failure_reason:
                    terminal["failure_reason"] = failure.failure_reason
                report(terminal, False)
                yield _response_failure_frame(
                    "upstream stream failed",
                    failure.status,
                    failure.error_class,
                    failure.failure_reason,
                    transport_failure=True,
                )
                return
            finally:
                close = getattr(iterator, "close", None)
                if callable(close):
                    try:
                        close()
                    except Exception:
                        pass
    except GeneratorExit:
        report(
            {
                "success": False,
                "status": None,
                "error_class": "client_disconnect",
            },
            False,
        )
        raise


def _sse_data(response: Any) -> Iterator[Tuple[str, bool]]:
    """Yield decoded data with whether it came from an SSE ``data:`` frame."""
    pending = []
    pending_bytes = 0
    stream_bytes = 0
    raw_body = bytearray()
    saw_sse_data = False
    try:
        for raw in response:
            stream_bytes += len(raw)
            if stream_bytes > MAX_UPSTREAM_BODY_BYTES:
                raise RouterError("upstream SSE stream is too large", 502)
            if len(raw) > MAX_SSE_FRAME_BYTES:
                raise RouterError("upstream SSE frame is too large", 502)
            if not saw_sse_data:
                raw_body.extend(raw)
            try:
                line = raw.decode("utf-8", "strict").rstrip("\r\n")
            except UnicodeDecodeError as exc:
                raise ExternalProtocolError(
                    "upstream SSE stream is not valid UTF-8"
                ) from exc
            if line.startswith("data:"):
                if not saw_sse_data:
                    raw_body.clear()
                saw_sse_data = True
                value = line[5:].lstrip()
                pending_bytes += len(value.encode("utf-8"))
                if pending_bytes > MAX_SSE_FRAME_BYTES:
                    raise RouterError("upstream SSE frame is too large", 502)
                pending.append(value)
            elif not line and pending:
                yield "\n".join(pending), True
                pending = []
                pending_bytes = 0
        if pending:
            yield "\n".join(pending), True
        if not saw_sse_data and raw_body:
            try:
                body = bytes(raw_body).decode("utf-8", "strict").strip()
            except UnicodeDecodeError as exc:
                raise ExternalProtocolError(
                    "upstream response is not valid UTF-8"
                ) from exc
            try:
                json.loads(body)
            except ValueError:
                raise RouterError("upstream returned neither SSE nor JSON data", 502)
            yield body, False
    finally:
        response.close()


def _request_stream(
    io: StreamAdapterIO,
    provider: Dict[str, Any],
    payload: Dict[str, Any],
    incoming: Dict[str, str],
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]],
) -> Any:
    """Mark failures that happen while establishing the upstream response."""

    try:
        return io.request(
            provider,
            payload,
            incoming,
            True,
            context_check=context_check,
            # Streaming disables transport replay inside Router. Keep the one
            # pre-header compatibility retry that changes an explicitly
            # rejected reasoning_effort payload before any output exists.
            allow_retries=True,
        )
    except TransportFailure:
        raise
    except TimeoutError as exc:
        raise TransportFailure(
            CONNECT_TIMEOUT,
            504,
            PHASE_CONNECT,
        ) from exc
    except URLError as exc:
        if isinstance(getattr(exc, "reason", None), TimeoutError):
            raise TransportFailure(
                CONNECT_TIMEOUT,
                504,
                PHASE_CONNECT,
            ) from exc
        raise


def _response_json_stream(
    value: Dict[str, Any], validate_output_items: bool = True
) -> Iterator[bytes]:
    """Convert one complete Responses JSON body to a finite SSE stream."""
    response = validate_responses_body(value, validate_output_items)
    output = response["output"]
    response.setdefault("id", "resp_" + uuid.uuid4().hex)
    response.setdefault("object", "response")
    final_status = response["status"]
    response["status"] = final_status
    output = [dict(item) for item in output]
    if not output and response.get("output_text"):
        output = [{
            "id": "msg_" + uuid.uuid4().hex,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": str(response["output_text"]), "annotations": []}],
        }]
    response["output"] = output
    base = dict(response)
    base["status"] = "in_progress"
    base["output"] = []
    yield _sse_frame("response.created", {"type": "response.created", "response": base})
    for index, source_item in enumerate(output):
        item = dict(source_item)
        item.setdefault("id", "item_" + uuid.uuid4().hex)
        item_type = item.get("type", "message")
        added = dict(item)
        if item_type != "compaction":
            added["status"] = "in_progress"
        yield _sse_frame(
            "response.output_item.added",
            {"type": "response.output_item.added", "output_index": index, "item": added},
        )
        if item_type == "message":
            text = _content_text(item.get("content", ""))
            _validate_textual_protocol(text)
            if text:
                content_part = {"type": "output_text", "text": text, "annotations": []}
                yield _sse_frame(
                    "response.content_part.added",
                    {"type": "response.content_part.added", "item_id": item["id"], "output_index": index, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}},
                )
                yield _sse_frame(
                    "response.output_text.delta",
                    {"type": "response.output_text.delta", "item_id": item["id"], "output_index": index, "content_index": 0, "delta": text},
                )
                yield _sse_frame(
                    "response.output_text.done",
                    {"type": "response.output_text.done", "item_id": item["id"], "output_index": index, "content_index": 0, "text": text},
                )
                yield _sse_frame(
                    "response.content_part.done",
                    {"type": "response.content_part.done", "item_id": item["id"], "output_index": index, "content_index": 0, "part": content_part},
                )
        elif item_type == "function_call":
            arguments = item.get("arguments", "{}")
            if not isinstance(arguments, str):
                arguments = json.dumps(arguments, ensure_ascii=False)
            yield _sse_frame(
                "response.function_call_arguments.done",
                {"type": "response.function_call_arguments.done", "item_id": item["id"], "output_index": index, "arguments": arguments},
            )
        done = dict(item)
        if item_type != "compaction":
            done["status"] = "completed"
        yield _sse_frame("response.output_item.done", {"type": "response.output_item.done", "output_index": index, "item": done})
    terminal_event = {
        "completed": "response.completed",
        "incomplete": "response.incomplete",
        "failed": "response.failed",
    }[final_status]
    yield _sse_frame(
        terminal_event,
        {"type": terminal_event, "response": response},
    )


def _validated_responses_stream(
    io: StreamAdapterIO,
    response: Any,
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    provider: Optional[Dict[str, Any]] = None,
) -> Iterator[bytes]:
    """Pass through valid Responses SSE and terminate malformed streams explicitly."""
    reported = False
    lifecycle = StreamLifecycle()

    def report(
        status: Any,
        error_class: str = "none",
        context_observation: Optional[Mapping[str, Any]] = None,
        terminal: Optional[Mapping[str, Any]] = None,
    ) -> None:
        nonlocal reported
        if reported:
            return
        reported = True
        diagnostics = lifecycle.diagnostics(terminal or {})
        _notify_terminal(
            terminal_callback,
            status,
            error_class,
            context_observation,
            diagnostics,
        )

    try:
        response_status = getattr(response, "status", None)
        if isinstance(response_status, int) and not isinstance(response_status, bool):
            if response_status == 504:
                raise TransportFailure(UPSTREAM_504, 504, PHASE_CONNECT)
            if 500 <= response_status <= 599:
                raise TransportFailure("upstream_5xx", response_status, PHASE_CONNECT)
        content_type = str(getattr(response, "headers", {}).get("Content-Type", "")).lower()
        chunks = response
        if "text/event-stream" not in content_type:
            raw = io.read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses stream")
            stripped = raw.lstrip()
            if stripped.startswith((b"event:", b"data:", b":")):
                chunks = (raw,)
            else:
                try:
                    value = json.loads(raw.decode("utf-8", "strict"))
                except (UnicodeDecodeError, ValueError):
                    raise RouterError("upstream Responses stream was neither SSE nor valid JSON", 502)
                if provider is not None:
                    io.raise_if_context_response(
                        provider,
                        getattr(response, "status", 400),
                        content_type,
                        raw,
                    )
                strict_output = not (
                    provider is not None
                    and provider.get("auth_mode") == "forward"
                )
                lifecycle.mark_iterator_created()
                lifecycle.observe_event(
                    {"type": "response." + str(value.get("status")), "response": value}
                )
                terminal = lifecycle.observe_terminal(
                    {"type": "response." + str(value.get("status")), "response": value},
                    responses_terminal_observation(value, strict_output),
                )
                yield from _response_json_stream(value, strict_output)
                report(
                    terminal["status"],
                    terminal["error_class"],
                    terminal=terminal,
                )
                return

        line_buffer = ""
        pending_data = []
        saw_data = False
        saw_terminal = False
        terminal_observation: Optional[Dict[str, Any]] = None

        def consume_line(line: str) -> None:
            nonlocal saw_data, saw_terminal, pending_data, terminal_observation
            line = line.rstrip("\r")
            if line.startswith("data:"):
                saw_data = True
                pending_data.append(line[5:].lstrip())
                return
            if line or not pending_data:
                return
            data = "\n".join(pending_data)
            pending_data = []
            if data == "[DONE]":
                return
            try:
                value = json.loads(data)
            except ValueError:
                return
            if not isinstance(value, dict):
                return
            if provider is not None and is_explicit_context_error(
                400, "application/json", data.encode("utf-8", "replace")
            ):
                observation = mark_explicit_failure(
                    provider.get("_context_observation", {}),
                    provider.get("_context_observation", {}).get("input_estimate"),
                )
                raise ContextLengthError(observation)
            lifecycle.observe_event(value)
            terminal = _stream_terminal(value)
            if terminal is not None:
                saw_terminal = True
                terminal_observation = lifecycle.observe_terminal(value, terminal)
                _validate_terminal_payload(value)

        for raw in chunks:
            raw_bytes = raw if isinstance(raw, bytes) else str(raw).encode("utf-8")
            try:
                text = raw_bytes.decode("utf-8", "strict")
            except UnicodeDecodeError as exc:
                raise ExternalProtocolError(
                    "upstream Responses stream is not valid UTF-8"
                ) from exc
            line_buffer += text
            while "\n" in line_buffer:
                line, line_buffer = line_buffer.split("\n", 1)
                consume_line(line)
            lines = raw_bytes.splitlines(keepends=True)
            filtered = b"".join(line for line in lines if line.strip() != b"data: [DONE]")
            if filtered:
                yield filtered
        if line_buffer:
            consume_line(line_buffer)
        if pending_data:
            consume_line("")
        if not saw_terminal:
            message = "upstream Responses stream ended before response.completed" if saw_data else "upstream Responses stream contained no SSE data"
            yield _response_failure_frame(message, 502, STREAM_INCOMPLETE)
            report(502, STREAM_INCOMPLETE)
        else:
            terminal = terminal_observation or {
                "status": 502,
                "success": False,
                "error_class": "stream_error",
            }
            report(
                terminal.get("status", 502),
                terminal.get("error_class", "stream_error"),
                terminal=terminal,
            )
    except RouterError as exc:
        if isinstance(exc, TransportFailure):
            raise
        if isinstance(exc, ContextLengthError):
            report(exc.status, "context_length_exceeded", exc.context_observation)
        else:
            report(exc.status, _route_error_class(exc.status))
        yield _response_failure_frame(
            "upstream stream failed",
            exc.status,
            "context_length_exceeded"
            if isinstance(exc, ContextLengthError)
            else _route_error_class(exc.status),
        )
    finally:
        response.close()


def stream_chat_completion(
    io: StreamAdapterIO,
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
) -> Iterable[bytes]:
    body = io.body_with_supported_effort(provider, body, model)
    payload = responses_to_chat(body, io.upstream_model(provider, model, body["model"]))
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    text = []
    text_bytes = 0
    protocol_probe = ""
    tool_calls = {}
    tool_call_ids = set()
    custom_names = custom_tool_names(body)
    finish_reason = None
    saw_output = False
    saw_done = False
    ordinary_complete = False
    message_started = False
    response = {
        "id": response_id,
        "object": "response",
        "status": "in_progress",
        "model": body["model"],
        "output": [],
    }
    sequence = 0

    def frame(event: str, value: Dict[str, Any]) -> bytes:
        nonlocal sequence
        sequence += 1
        value = dict(value)
        value.setdefault("sequence_number", sequence)
        return _sse_frame(event, value)

    try:
        upstream = _request_stream(io, provider, payload, incoming, context_check)
        yield frame("response.created", {"type": "response.created", "response": response})
        for data, framed_as_sse in _sse_data(upstream):
            if data == "[DONE]":
                saw_done = True
                break
            try:
                chunk = json.loads(data)
            except ValueError:
                raise ExternalProtocolError(
                    "Chat Completions upstream returned malformed SSE data"
                ) from None
            if not isinstance(chunk, Mapping):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned malformed SSE data"
                )
            if chunk.get("error"):
                if is_explicit_context_error(
                    400,
                    "application/json",
                    json.dumps(chunk, ensure_ascii=False).encode("utf-8"),
                ):
                    raise ContextLengthError(
                        mark_explicit_failure(
                            provider.get("_context_observation", {}),
                            provider.get("_context_observation", {}).get("input_estimate"),
                        )
                    )
                raise RouterError(
                    "Chat Completions upstream returned an error",
                    502,
                )
            choices = chunk.get("choices")
            if not isinstance(choices, list):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned an invalid stream chunk"
                )
            if not choices:
                # A final usage-only chunk is valid when stream_options asks for it.
                if chunk.get("usage") is not None:
                    continue
                raise ExternalProtocolError(
                    "Chat Completions upstream returned an invalid stream chunk"
                )
            if not isinstance(choices[0], Mapping):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned an invalid stream chunk"
                )
            choice = choices[0]
            if choice.get("finish_reason") is not None:
                finish_reason = choice.get("finish_reason")
            delta = choice.get("delta") or {}
            if not delta and isinstance(choice.get("message"), dict):
                # Some OpenAI-compatible gateways ignore stream=true and return
                # one ordinary Chat Completions response instead of SSE deltas.
                delta = choice["message"]
                ordinary_complete = (
                    not framed_as_sse and choice.get("finish_reason") is not None
                )
            if not isinstance(delta, Mapping):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned an invalid stream chunk"
                )
            raw_piece = delta.get("content")
            if raw_piece is None:
                piece = ""
            elif isinstance(raw_piece, str):
                piece = raw_piece
            elif isinstance(raw_piece, list):
                parts = []
                for part in raw_piece:
                    if (
                        not isinstance(part, Mapping)
                        or part.get("type") not in {"text", "output_text"}
                        or not isinstance(part.get("text"), str)
                    ):
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned invalid message content"
                        )
                    parts.append(part["text"])
                piece = "".join(parts)
            else:
                raise ExternalProtocolError(
                    "Chat Completions upstream returned invalid message content"
                )
            if piece:
                protocol_probe = _advance_textual_protocol_probe(protocol_probe, piece)
                piece_bytes = len(piece.encode("utf-8"))
                if text_bytes + piece_bytes > MAX_STREAM_TEXT_BYTES:
                    raise RouterError("upstream streamed text is too large", 502)
                text_bytes += piece_bytes
                text.append(piece)
                saw_output = True
                if not message_started:
                    message_started = True
                    yield frame(
                        "response.output_item.added",
                        {
                            "type": "response.output_item.added",
                            "output_index": 0,
                            "item": {"id": message_id, "type": "message", "status": "in_progress", "role": "assistant", "content": []},
                        },
                    )
                    yield frame(
                        "response.content_part.added",
                        {
                            "type": "response.content_part.added",
                            "item_id": message_id,
                            "output_index": 0,
                            "content_index": 0,
                            "part": {"type": "output_text", "text": "", "annotations": []},
                        },
                    )
                yield frame(
                    "response.output_text.delta",
                    {
                        "type": "response.output_text.delta",
                        "item_id": message_id,
                        "output_index": 0,
                        "content_index": 0,
                        "delta": piece,
                    },
                )
            raw_calls = delta.get("tool_calls", [])
            if not isinstance(raw_calls, list):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned an invalid tool call"
                )
            for raw_call in raw_calls:
                if not isinstance(raw_call, Mapping):
                    raise ExternalProtocolError(
                        "Chat Completions upstream returned an invalid tool call"
                    )
                raw_index = raw_call.get("index", len(tool_calls))
                try:
                    if isinstance(raw_index, bool):
                        raise ValueError
                    index = int(raw_index)
                except (TypeError, ValueError):
                    raise ExternalProtocolError(
                        "Chat Completions upstream returned an invalid tool call"
                    ) from None
                function = raw_call.get("function") or {}
                if not isinstance(function, Mapping):
                    raise ExternalProtocolError(
                        "Chat Completions upstream returned an invalid tool call"
                    )
                state = tool_calls.get(index)
                if state is None:
                    saw_output = True
                    call_id = raw_call.get("id")
                    if not isinstance(call_id, str) or not call_id:
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned an invalid tool call"
                        )
                    if call_id in tool_call_ids:
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned a duplicate tool call ID"
                        )
                    tool_call_ids.add(call_id)
                    state = {
                        "id": call_id,
                        "call_id": call_id,
                        "name": "",
                        "arguments": "",
                        "custom": False,
                        "added": False,
                    }
                    tool_calls[index] = state
                elif raw_call.get("id"):
                    if (
                        not isinstance(raw_call["id"], str)
                        or raw_call["id"] != state["call_id"]
                    ):
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned an invalid tool call"
                        )
                name_fragment = function.get("name")
                if name_fragment is not None:
                    if not isinstance(name_fragment, str):
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned an invalid tool call"
                        )
                    state["name"] += name_fragment
                if isinstance(raw_call.get("extra_content"), Mapping):
                    state["extra_content"] = dict(raw_call["extra_content"])
                arguments = function.get("arguments")
                if arguments is not None:
                    if not isinstance(arguments, str):
                        raise ExternalProtocolError(
                            "Chat Completions upstream returned invalid tool arguments"
                        )
                if arguments:
                    saw_output = True
                    state["arguments"] += arguments
        if not saw_done and not ordinary_complete:
            raise StreamBoundaryError(
                "Chat Completions upstream ended before [DONE]",
                "stream_incomplete",
            )
        if not saw_output:
            raise StreamBoundaryError(
                "upstream returned an empty Chat Completions response",
                "stream_incomplete",
            )
        incomplete_reason = _chat_incomplete_reason(finish_reason)
        final_text = "".join(text)
        output = {
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": final_text, "annotations": []}],
        }
        function_outputs = []
        tool_output_base = 1 if message_started else 0
        for tool_position, raw_index in enumerate(sorted(tool_calls)):
            state = tool_calls[raw_index]
            output_index = tool_output_base + tool_position
            if not state["name"]:
                raise RouterError("upstream returned a tool call without a name", 502)
            validated_arguments = _upstream_tool_arguments(
                state["arguments"], "Chat Completions"
            )
            state["custom"] = state["name"] in custom_names
            if state["custom"]:
                state["id"], state["call_id"] = custom_tool_ids(
                    state["id"], state["call_id"]
                )
            added_item = {
                "id": state["id"],
                "type": "custom_tool_call" if state["custom"] else "function_call",
                "status": "in_progress",
                "call_id": state["call_id"],
                "name": state["name"],
            }
            added_item["input" if state["custom"] else "arguments"] = ""
            if "extra_content" in state:
                added_item["extra_content"] = state["extra_content"]
            yield frame(
                "response.output_item.added",
                {
                    "type": "response.output_item.added",
                    "output_index": output_index,
                    "item": added_item,
                },
            )
            function_output = {
                "id": state["id"],
                "type": "custom_tool_call" if state["custom"] else "function_call",
                "status": "completed",
                "call_id": state["call_id"],
                "name": state["name"],
            }
            function_output[
                "input" if state["custom"] else "arguments"
            ] = (
                custom_tool_input(validated_arguments)
                if state["custom"]
                else validated_arguments
            )
            if "extra_content" in state:
                function_output["extra_content"] = state["extra_content"]
            function_outputs.append(function_output)
            if state["custom"]:
                yield frame(
                    "response.custom_tool_call_input.delta",
                    {
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": state["id"],
                        "output_index": output_index,
                        "delta": function_output["input"],
                    },
                )
            else:
                yield frame(
                    "response.function_call_arguments.delta",
                    {
                        "type": "response.function_call_arguments.delta",
                        "item_id": state["id"],
                        "output_index": output_index,
                        "delta": validated_arguments,
                    },
                )
                yield frame(
                    "response.function_call_arguments.done",
                    {
                        "type": "response.function_call_arguments.done",
                        "item_id": state["id"],
                        "output_index": output_index,
                        "arguments": validated_arguments,
                    },
                )
            yield frame(
                "response.output_item.done",
                {
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": function_output,
                },
            )
        response["status"] = "incomplete" if incomplete_reason else "completed"
        response["output"] = ([output] if message_started else []) + function_outputs
        response["output_text"] = final_text
        if incomplete_reason:
            response["incomplete_details"] = {"reason": incomplete_reason}
        if message_started:
            yield frame(
                "response.output_text.done",
                {
                    "type": "response.output_text.done",
                    "item_id": message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "text": final_text,
                },
            )
            yield frame(
                "response.content_part.done",
                {
                    "type": "response.content_part.done",
                    "item_id": message_id,
                    "output_index": 0,
                    "content_index": 0,
                    "part": {"type": "output_text", "text": final_text, "annotations": []},
                },
            )
            yield frame(
                "response.output_item.done",
                {
                    "type": "response.output_item.done",
                    "output_index": 0,
                    "item": output,
                },
            )
        terminal_event = "response.incomplete" if incomplete_reason else "response.completed"
        yield frame(terminal_event, {"type": terminal_event, "response": response})
        _notify_terminal(
            terminal_callback,
            200,
            {
                "max_output_tokens": "output_limit",
                "content_filter": "content_filter",
            }.get(incomplete_reason, "none"),
        )
    except RouterError as exc:
        if isinstance(exc, TransportFailure):
            raise
        terminal = _terminal_exception(exc)
        _notify_terminal(
            terminal_callback,
            terminal["status"],
            terminal["error_class"],
            terminal.get("context_observation"),
        )
        yield _response_failure_frame(
            str(exc), exc.status, terminal.get("error_class")
        )


def stream_anthropic_completion(
    io: StreamAdapterIO,
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    context_check: Optional[Callable[[Dict[str, Any], bool, str], Mapping[str, Any]]] = None,
) -> Iterable[bytes]:
    payload = responses_to_anthropic(body, io.upstream_model(provider, model, body["model"]))
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    custom_names = custom_tool_names(body)
    blocks: Dict[int, Dict[str, Any]] = {}
    ordered_blocks = []
    tool_call_ids = set()
    all_text = []
    text_bytes = 0
    stop_reason = None
    saw_message_stop = False
    response = {
        "id": response_id,
        "object": "response",
        "status": "in_progress",
        "model": body["model"],
        "output": [],
    }
    sequence = 0

    def frame(event: str, value: Dict[str, Any]) -> bytes:
        nonlocal sequence
        sequence += 1
        value = dict(value)
        value.setdefault("sequence_number", sequence)
        return _sse_frame(event, value)

    try:
        upstream = _request_stream(io, provider, payload, incoming, context_check)
        yield frame("response.created", {"type": "response.created", "response": response})
        for data, _framed_as_sse in _sse_data(upstream):
            if data == "[DONE]":
                break
            try:
                event = json.loads(data)
            except ValueError as exc:
                raise ExternalProtocolError(
                    "Anthropic upstream returned malformed stream JSON"
                ) from exc
            if not isinstance(event, Mapping):
                raise ExternalProtocolError(
                    "Anthropic upstream returned an invalid stream event"
                )
            event_type = str(event.get("type") or "")
            if event_type == "error":
                raise ExternalProtocolError("Anthropic upstream returned an error event")
            if event_type == "message_delta":
                delta = event.get("delta")
                delta = delta if isinstance(delta, Mapping) else {}
                if delta.get("stop_reason"):
                    stop_reason = delta["stop_reason"]
                continue
            if event_type == "message_stop":
                saw_message_stop = True
                continue
            if event_type == "content_block_start":
                raw_index = event.get("index", len(blocks))
                if isinstance(raw_index, bool) or not isinstance(raw_index, int) or raw_index < 0:
                    raise ExternalProtocolError(
                        "Anthropic upstream returned an invalid content index"
                    )
                if raw_index in blocks:
                    raise ExternalProtocolError(
                        "Anthropic upstream repeated a content block"
                    )
                block = event.get("content_block")
                if not isinstance(block, Mapping):
                    raise ExternalProtocolError(
                        "Anthropic upstream returned an invalid content block"
                    )
                block_type = str(block.get("type") or "")
                output_index = len(ordered_blocks)
                if block_type == "text":
                    state = {
                        "kind": "text",
                        "index": raw_index,
                        "output_index": output_index,
                        "id": "msg_" + uuid.uuid4().hex,
                        "parts": [],
                        "probe": "",
                        "explicit": True,
                        "closed": False,
                    }
                    blocks[raw_index] = state
                    ordered_blocks.append(state)
                    yield frame(
                        "response.output_item.added",
                        {
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": {
                                "id": state["id"],
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            },
                        },
                    )
                    yield frame(
                        "response.content_part.added",
                        {
                            "type": "response.content_part.added",
                            "item_id": state["id"],
                            "output_index": output_index,
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                            },
                        },
                    )
                    initial_text = block.get("text")
                    if initial_text is not None and not isinstance(initial_text, str):
                        raise ExternalProtocolError(
                            "Anthropic upstream returned invalid message content"
                        )
                    if initial_text:
                        event = {
                            "type": "content_block_delta",
                            "index": raw_index,
                            "delta": {
                                "type": "text_delta",
                                "text": initial_text,
                            },
                        }
                        event_type = "content_block_delta"
                    else:
                        continue
                elif block_type == "tool_use":
                    raw_call_id = block.get("id")
                    name = block.get("name")
                    initial_input = block.get("input", {})
                    if (
                        not isinstance(raw_call_id, str)
                        or not raw_call_id
                        or not isinstance(name, str)
                        or not name
                        or not isinstance(initial_input, Mapping)
                    ):
                        raise ExternalProtocolError(
                            "Anthropic upstream returned an invalid tool call"
                        )
                    if raw_call_id in tool_call_ids:
                        raise ExternalProtocolError(
                            "Anthropic upstream returned a duplicate tool call ID"
                        )
                    tool_call_ids.add(raw_call_id)
                    custom = name in custom_names
                    item_id, call_id = (
                        custom_tool_ids(raw_call_id, raw_call_id)
                        if custom
                        else (raw_call_id, raw_call_id)
                    )
                    state = {
                        "kind": "tool",
                        "index": raw_index,
                        "output_index": output_index,
                        "id": item_id,
                        "call_id": call_id,
                        "name": name,
                        "custom": custom,
                        "initial_input": dict(initial_input),
                        "json_parts": [],
                        "json_bytes": 0,
                        "explicit": True,
                        "closed": False,
                    }
                    blocks[raw_index] = state
                    ordered_blocks.append(state)
                    added = {
                        "id": item_id,
                        "type": "custom_tool_call" if custom else "function_call",
                        "status": "in_progress",
                        "call_id": call_id,
                        "name": name,
                    }
                    added["input" if custom else "arguments"] = ""
                    yield frame(
                        "response.output_item.added",
                        {
                            "type": "response.output_item.added",
                            "output_index": output_index,
                            "item": added,
                        },
                    )
                    continue
                elif block_type in {"thinking", "redacted_thinking"}:
                    state = {
                        "kind": "suppressed_reasoning",
                        "index": raw_index,
                        "output_index": None,
                        "explicit": True,
                        "closed": False,
                    }
                    blocks[raw_index] = state
                    continue
                else:
                    raise ExternalProtocolError(
                        "Anthropic upstream returned an unsupported content block"
                    )
            if event_type == "content_block_delta":
                raw_index = event.get("index", 0)
                if isinstance(raw_index, bool) or not isinstance(raw_index, int) or raw_index < 0:
                    raise ExternalProtocolError(
                        "Anthropic upstream returned an invalid content index"
                    )
                delta = event.get("delta")
                if not isinstance(delta, Mapping):
                    raise ExternalProtocolError(
                        "Anthropic upstream returned an invalid content delta"
                    )
                delta_type = str(delta.get("type") or "")
                state = blocks.get(raw_index)
                if state is None and delta_type == "text_delta":
                    state = {
                        "kind": "text",
                        "index": raw_index,
                        "output_index": len(ordered_blocks),
                        "id": "msg_" + uuid.uuid4().hex,
                        "parts": [],
                        "probe": "",
                        "explicit": False,
                        "closed": False,
                    }
                    blocks[raw_index] = state
                    ordered_blocks.append(state)
                    yield frame(
                        "response.output_item.added",
                        {
                            "type": "response.output_item.added",
                            "output_index": state["output_index"],
                            "item": {
                                "id": state["id"],
                                "type": "message",
                                "status": "in_progress",
                                "role": "assistant",
                                "content": [],
                            },
                        },
                    )
                    yield frame(
                        "response.content_part.added",
                        {
                            "type": "response.content_part.added",
                            "item_id": state["id"],
                            "output_index": state["output_index"],
                            "content_index": 0,
                            "part": {
                                "type": "output_text",
                                "text": "",
                                "annotations": [],
                            },
                        },
                    )
                if state is None or state.get("closed"):
                    raise ExternalProtocolError(
                        "Anthropic upstream returned a delta for an unknown content block"
                    )
                if state["kind"] == "suppressed_reasoning":
                    continue
                if state["kind"] == "text" and delta_type == "text_delta":
                    piece = delta.get("text")
                    if not isinstance(piece, str):
                        raise ExternalProtocolError(
                            "Anthropic upstream returned invalid message content"
                        )
                    if not piece:
                        continue
                    piece_bytes = len(piece.encode("utf-8"))
                    if text_bytes + piece_bytes > MAX_STREAM_TEXT_BYTES:
                        raise RouterError("upstream streamed text is too large", 502)
                    text_bytes += piece_bytes
                    state["probe"] = _advance_textual_protocol_probe(
                        state["probe"], piece
                    )
                    state["parts"].append(piece)
                    all_text.append(piece)
                    yield frame(
                        "response.output_text.delta",
                        {
                            "type": "response.output_text.delta",
                            "item_id": state["id"],
                            "output_index": state["output_index"],
                            "content_index": 0,
                            "delta": piece,
                        },
                    )
                    continue
                if state["kind"] == "tool" and delta_type == "input_json_delta":
                    piece = delta.get("partial_json")
                    if not isinstance(piece, str):
                        raise ExternalProtocolError(
                            "Anthropic upstream returned invalid tool input"
                        )
                    piece_bytes = len(piece.encode("utf-8"))
                    if state["json_bytes"] + piece_bytes > MAX_SSE_FRAME_BYTES:
                        raise ExternalProtocolError(
                            "Anthropic upstream tool input is too large"
                        )
                    state["json_bytes"] += piece_bytes
                    state["json_parts"].append(piece)
                    if piece and not state["custom"]:
                        yield frame(
                            "response.function_call_arguments.delta",
                            {
                                "type": "response.function_call_arguments.delta",
                                "item_id": state["id"],
                                "output_index": state["output_index"],
                                "delta": piece,
                            },
                        )
                    continue
                raise ExternalProtocolError(
                    "Anthropic upstream returned a mismatched content delta"
                )
            if event_type != "content_block_stop":
                continue
            raw_index = event.get("index", 0)
            if isinstance(raw_index, bool) or not isinstance(raw_index, int):
                raise ExternalProtocolError(
                    "Anthropic upstream returned an invalid content index"
                )
            state = blocks.get(raw_index)
            if state is None or state.get("closed"):
                raise ExternalProtocolError(
                    "Anthropic upstream stopped an unknown content block"
                )
            state["closed"] = True
            if state["kind"] == "suppressed_reasoning":
                continue
            if state["kind"] == "text":
                final_text = "".join(state["parts"])
                item = {
                    "id": state["id"],
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": final_text,
                            "annotations": [],
                        }
                    ],
                }
                state["item"] = item
                yield frame(
                    "response.output_text.done",
                    {
                        "type": "response.output_text.done",
                        "item_id": state["id"],
                        "output_index": state["output_index"],
                        "content_index": 0,
                        "text": final_text,
                    },
                )
                yield frame(
                    "response.content_part.done",
                    {
                        "type": "response.content_part.done",
                        "item_id": state["id"],
                        "output_index": state["output_index"],
                        "content_index": 0,
                        "part": {
                            "type": "output_text",
                            "text": final_text,
                            "annotations": [],
                        },
                    },
                )
                yield frame(
                    "response.output_item.done",
                    {
                        "type": "response.output_item.done",
                        "output_index": state["output_index"],
                        "item": item,
                    },
                )
                continue
            raw_arguments = "".join(state["json_parts"])
            if raw_arguments:
                try:
                    tool_input = json.loads(raw_arguments)
                except ValueError as exc:
                    raise ExternalProtocolError(
                        "Anthropic upstream returned malformed tool input"
                    ) from exc
                if not isinstance(tool_input, Mapping):
                    raise ExternalProtocolError(
                        "Anthropic upstream returned invalid tool input"
                    )
            else:
                tool_input = state["initial_input"]
            arguments = _anthropic_tool_arguments(tool_input)
            item = {
                "id": state["id"],
                "type": "custom_tool_call" if state["custom"] else "function_call",
                "status": "completed",
                "call_id": state["call_id"],
                "name": state["name"],
            }
            if state["custom"]:
                item["input"] = custom_tool_input(arguments)
                yield frame(
                    "response.custom_tool_call_input.delta",
                    {
                        "type": "response.custom_tool_call_input.delta",
                        "item_id": state["id"],
                        "output_index": state["output_index"],
                        "delta": item["input"],
                    },
                )
            else:
                item["arguments"] = arguments
                yield frame(
                    "response.function_call_arguments.done",
                    {
                        "type": "response.function_call_arguments.done",
                        "item_id": state["id"],
                        "output_index": state["output_index"],
                        "arguments": arguments,
                    },
                )
            state["item"] = item
            yield frame(
                "response.output_item.done",
                {
                    "type": "response.output_item.done",
                    "output_index": state["output_index"],
                    "item": item,
                },
            )
        if not saw_message_stop:
            raise StreamBoundaryError(
                "Anthropic upstream ended before message_stop",
                "stream_incomplete",
            )
        incomplete_reason = _anthropic_incomplete_reason(stop_reason)
        for state in ordered_blocks:
            if state.get("closed"):
                continue
            if state["kind"] != "text" or state.get("explicit"):
                raise StreamBoundaryError(
                    "Anthropic upstream ended with an unfinished content block",
                    "stream_incomplete",
                )
            state["closed"] = True
            final_text = "".join(state["parts"])
            item = {
                "id": state["id"],
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [
                    {
                        "type": "output_text",
                        "text": final_text,
                        "annotations": [],
                    }
                ],
            }
            state["item"] = item
            yield frame(
                "response.output_text.done",
                {
                    "type": "response.output_text.done",
                    "item_id": state["id"],
                    "output_index": state["output_index"],
                    "content_index": 0,
                    "text": final_text,
                },
            )
            yield frame(
                "response.content_part.done",
                {
                    "type": "response.content_part.done",
                    "item_id": state["id"],
                    "output_index": state["output_index"],
                    "content_index": 0,
                    "part": {
                        "type": "output_text",
                        "text": final_text,
                        "annotations": [],
                    },
                },
            )
            yield frame(
                "response.output_item.done",
                {
                    "type": "response.output_item.done",
                    "output_index": state["output_index"],
                    "item": item,
                },
            )
        output = [
            state["item"]
            for state in ordered_blocks
            if isinstance(state.get("item"), Mapping)
        ]
        if not output:
            raise StreamBoundaryError(
                "upstream returned an empty Anthropic Messages response",
                "stream_incomplete",
            )
        response["status"] = "incomplete" if incomplete_reason else "completed"
        response["output"] = output
        response["output_text"] = "".join(all_text)
        if incomplete_reason:
            response["incomplete_details"] = {"reason": incomplete_reason}
        terminal_event = "response.incomplete" if incomplete_reason else "response.completed"
        yield frame(terminal_event, {"type": terminal_event, "response": response})
        _notify_terminal(
            terminal_callback,
            200,
            {
                "max_output_tokens": "output_limit",
                "content_filter": "content_filter",
            }.get(incomplete_reason, "none"),
        )
    except RouterError as exc:
        if isinstance(exc, TransportFailure):
            raise
        terminal = _terminal_exception(exc)
        _notify_terminal(
            terminal_callback,
            terminal["status"],
            terminal["error_class"],
            terminal.get("context_observation"),
        )
        yield _response_failure_frame(
            str(exc), exc.status, terminal.get("error_class")
        )
