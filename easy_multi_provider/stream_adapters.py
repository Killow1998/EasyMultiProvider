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
    ProjectionError,
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


MAX_UPSTREAM_BODY_BYTES = 16 * 1024 * 1024
MAX_SSE_FRAME_BYTES = 1024 * 1024
MAX_STREAM_TEXT_BYTES = 16 * 1024 * 1024
_PROTOCOL_REJECTION_STATUSES = frozenset({404, 405, 415, 501})


@dataclass(frozen=True)
class StreamAdapterIO:
    request: Callable[..., Any]
    read_limited: Callable[..., bytes]
    raise_if_context_response: Callable[..., None]
    body_with_supported_effort: Callable[..., Dict[str, Any]]
    upstream_model: Callable[..., str]


def _route_error_class(status: Any) -> str:
    if status in (401, 403):
        return "auth"
    if status == 429:
        return "rate_limit"
    if status in _PROTOCOL_REJECTION_STATUSES:
        return "protocol_rejection"
    if status in (408, 504):
        return "timeout"
    if isinstance(status, int) and 500 <= status <= 599:
        return "upstream_5xx"
    if status is None:
        return "network"
    return "router_error"


def _stream_exception(exc: BaseException) -> Tuple[Optional[int], str]:
    if isinstance(exc, ContextLengthError):
        return exc.status, "context_length_exceeded"
    if isinstance(exc, StreamBoundaryError):
        return exc.status, exc.error_class
    if isinstance(exc, RouterError):
        return exc.status, str(
            getattr(exc, "error_class", None) or _route_error_class(exc.status)
        )
    if isinstance(exc, TimeoutError):
        return 504, "timeout"
    if isinstance(exc, (OSError, URLError)):
        return 502, "network"
    return 502, "stream_error"


def _terminal_exception(exc: BaseException) -> Dict[str, Any]:
    if isinstance(exc, ContextLengthError):
        return {
            "success": False,
            "status": exc.status,
            "error_class": "context_length_exceeded",
            "context_observation": dict(exc.context_observation),
        }
    status, error_class = _stream_exception(exc)
    return {"success": False, "status": status, "error_class": error_class}


def _notify_terminal(
    callback: Optional[Callable[[Dict[str, Any]], None]],
    status: Any,
    error_class: str = "none",
    context_observation: Optional[Mapping[str, Any]] = None,
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
        callback(value)
    except Exception:
        pass


def _sse_frame(event: str, value: Dict[str, Any]) -> bytes:
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(value, ensure_ascii=False))).encode("utf-8")


def _response_failure_frame(
    message: str,
    status: int = 502,
    error_class: Optional[str] = None,
) -> bytes:
    display_message = "HTTP %d: %s" % (status, message)
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "failed",
        "error": {
            "code": "upstream_error",
            "message": display_message,
            "error_class": error_class or _route_error_class(status),
        },
    }
    return _sse_frame("response.failed", {"type": "response.failed", "response": response})


_PRE_OUTPUT_RECOVERY_ERRORS = frozenset(
    {"network", "stream_incomplete", "upstream_close_pre_output", "proxy_reset"}
)
_TERMINAL_EVENT_TYPES = frozenset(
    {"response.completed", "response.incomplete", "response.failed", "error"}
)


def _stream_event_activity(event: Mapping[str, Any]) -> Tuple[bool, bool]:
    """Return visible-output and tool-activity flags without retaining content."""

    event_type = str(event.get("type") or "")
    item = event.get("item")
    item_type = str(item.get("type") or "") if isinstance(item, Mapping) else ""
    tool_activity = item_type in {
        "function_call",
        "custom_tool_call",
        "tool_call",
    } or "function_call" in event_type or "tool_call" in event_type
    output_emitted = tool_activity
    if event_type.endswith((".delta", ".done")):
        output_emitted = output_emitted or any(
            marker in event_type
            for marker in ("output_text", "output_image", "image_generation", "reasoning")
        )
    elif any(marker in event_type for marker in ("output_image", "image_generation")):
        output_emitted = True
    part = event.get("part")
    if isinstance(part, Mapping) and part.get("type") in {
        "output_text",
        "output_image",
        "reasoning_text",
        "summary_text",
    }:
        output_emitted = True
    if isinstance(item, Mapping):
        content = item.get("content")
        if isinstance(content, list) and any(
            isinstance(part, Mapping)
            and part.get("type")
            in {"output_text", "output_image", "image", "image_url", "reasoning_text"}
            for part in content
        ):
            output_emitted = True
    return output_emitted, tool_activity


def _stream_terminal(event: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    event_type = str(event.get("type") or "")
    if event_type not in _TERMINAL_EVENT_TYPES:
        return None
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
                "error_class": str(
                    error.get("error_class") or _route_error_class(status)
                ),
            }
        if nested_status == "incomplete":
            details = response.get("incomplete_details")
            reason = (
                str(details.get("reason") or "")
                if isinstance(details, Mapping)
                else ""
            )
            return {
                "success": False,
                "status": 200,
                "error_class": {
                    "max_output_tokens": "output_limit",
                    "content_filter": "content_filter",
                }.get(reason, "stream_incomplete"),
            }
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
        error_class = {
            "max_output_tokens": "output_limit",
            "content_filter": "content_filter",
        }.get(reason, "stream_incomplete")
        return {"success": False, "status": 200, "error_class": error_class}
    error = response.get("error")
    if not isinstance(error, Mapping):
        error = event.get("error")
    error = error if isinstance(error, Mapping) else {}
    status = error.get("status")
    if not isinstance(status, int):
        message = str(error.get("message") or event.get("message") or "")
        match = re.search(r"HTTP\s+(\d{3})", message)
        status = int(match.group(1)) if match else 502
    error_class = str(error.get("error_class") or _route_error_class(status))
    close_code = error.get("close_code")
    terminal = {
        "success": False,
        "status": status,
        "error_class": error_class,
    }
    if isinstance(close_code, int) and not isinstance(close_code, bool):
        terminal["close_code"] = close_code
    return terminal


def _reliable_responses_stream(
    factory: Callable[[], Iterable[bytes]],
    terminal_callback: Optional[Callable[[Dict[str, Any]], None]] = None,
    replay_safe: bool = False,
) -> Iterator[bytes]:
    """Enforce one terminal event; replay only an explicitly safe factory."""

    output_emitted = False
    tool_activity = False
    recovery_attempted = False
    terminal_event_observed = False
    reported = False

    def report(terminal: Mapping[str, Any], terminal_observed: bool) -> None:
        nonlocal reported
        if reported:
            return
        reported = True
        value = dict(terminal)
        value.update(
            {
                "output_emitted": output_emitted,
                "tool_activity": tool_activity,
                "terminal_event_observed": bool(
                    terminal_event_observed or terminal_observed
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
            pending = []
            iterator = None
            retry = False
            try:
                iterator = iter(factory())
                for event in sse_json_events(iterator):
                    visible, tool = _stream_event_activity(event)
                    output_emitted = output_emitted or visible
                    tool_activity = tool_activity or tool
                    terminal = _stream_terminal(event)
                    if terminal is not None:
                        terminal_event_observed = True
                        error_class = terminal.get("error_class")
                        if (
                            replay_safe
                            and
                            terminal.get("success") is not True
                            and attempt == 0
                            and not output_emitted
                            and not tool_activity
                            and error_class in _PRE_OUTPUT_RECOVERY_ERRORS
                        ):
                            recovery_attempted = True
                            retry = True
                            break
                        for buffered in pending:
                            yield _sse_frame(str(buffered.get("type") or "message"), buffered)
                        report(terminal, True)
                        if terminal.get("success") is True:
                            yield _sse_frame("response.completed", dict(event))
                        elif str(event.get("type") or "") == "response.incomplete":
                            yield _sse_frame("response.incomplete", dict(event))
                        else:
                            yield _response_failure_frame(
                                "upstream stream failed",
                                int(terminal.get("status") or 502),
                                str(terminal.get("error_class") or "stream_error"),
                            )
                        return
                    if output_emitted or tool_activity:
                        for buffered in pending:
                            yield _sse_frame(str(buffered.get("type") or "message"), buffered)
                        pending = []
                        yield _sse_frame(str(event.get("type") or "message"), dict(event))
                    else:
                        pending.append(dict(event))
                if retry:
                    continue
                if tool_activity:
                    error_class = "upstream_close_after_tool"
                elif output_emitted:
                    error_class = "upstream_close_after_output"
                else:
                    error_class = "upstream_close_pre_output"
                if (
                    replay_safe
                    and attempt == 0
                    and not output_emitted
                    and not tool_activity
                ):
                    recovery_attempted = True
                    continue
                for buffered in pending:
                    yield _sse_frame(str(buffered.get("type") or "message"), buffered)
                terminal = {
                    "success": False,
                    "status": 502,
                    "error_class": error_class,
                }
                report(terminal, False)
                yield _response_failure_frame(
                    "upstream stream ended without a terminal event",
                    502,
                    error_class,
                )
                return
            except GeneratorExit:
                raise
            except Exception as exc:
                status, failure_class = _stream_exception(exc)
                if isinstance(exc, TransportError):
                    failure_class = "malformed_terminal"
                if (
                    replay_safe
                    and attempt == 0
                    and not output_emitted
                    and not tool_activity
                    and failure_class == "network"
                ):
                    recovery_attempted = True
                    continue
                error_class = (
                    "upstream_close_after_tool"
                    if tool_activity
                    else "upstream_close_after_output"
                    if output_emitted
                    else "proxy_reset"
                    if failure_class == "network"
                    else failure_class
                )
                for buffered in pending:
                    yield _sse_frame(str(buffered.get("type") or "message"), buffered)
                terminal = {
                    "success": False,
                    "status": status or 502,
                    "error_class": error_class,
                }
                report(terminal, False)
                failure_message = (
                    str(exc)
                    if isinstance(exc, RouterError)
                    and isinstance(exc.__cause__, ProjectionError)
                    else "upstream stream failed"
                )
                yield _response_failure_frame(
                    failure_message,
                    int(status or 502),
                    error_class,
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

    def report(
        status: Any,
        error_class: str = "none",
        context_observation: Optional[Mapping[str, Any]] = None,
    ) -> None:
        nonlocal reported
        if reported:
            return
        reported = True
        _notify_terminal(terminal_callback, status, error_class, context_observation)

    try:
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
                terminal = responses_terminal_observation(value, strict_output)
                yield from _response_json_stream(value, strict_output)
                report(terminal["status"], terminal["error_class"])
                return

        line_buffer = ""
        pending_data = []
        saw_data = False
        saw_terminal = False
        saw_error = False
        error_message = ""
        incomplete_reason = ""

        def consume_line(line: str) -> None:
            nonlocal saw_data, saw_terminal, saw_error, error_message, pending_data, incomplete_reason
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
            event_type = str(value.get("type") or "")
            if event_type == "response.completed":
                saw_terminal = True
            elif event_type == "response.incomplete":
                saw_terminal = True
                response_value = value.get("response")
                details = response_value.get("incomplete_details") if isinstance(response_value, Mapping) else None
                incomplete_reason = str(details.get("reason") or "") if isinstance(details, Mapping) else ""
            elif event_type in ("error", "response.failed"):
                saw_error = True
                error_message = str(value.get("message") or value.get("error") or "upstream Responses stream returned an error")

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
        if not saw_terminal and not saw_error:
            message = "upstream Responses stream ended before response.completed" if saw_data else "upstream Responses stream contained no SSE data"
            yield _response_failure_frame(message, 502, "stream_incomplete")
            report(502, "stream_incomplete")
        elif not saw_terminal and saw_error:
            yield _response_failure_frame(
                error_message or "upstream Responses stream returned an error",
                502,
                "stream_error",
            )
            report(502, "stream_error")
        else:
            report(
                200,
                {
                    "max_output_tokens": "output_limit",
                    "content_filter": "content_filter",
                }.get(incomplete_reason, "none"),
            )
    except RouterError as exc:
        if isinstance(exc, ContextLengthError):
            report(exc.status, "context_length_exceeded", exc.context_observation)
        else:
            report(exc.status, _route_error_class(exc.status))
        yield _response_failure_frame(
            str(exc),
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
        upstream = io.request(provider, payload, incoming, True, context_check=context_check)
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
        upstream = io.request(provider, payload, incoming, True, context_check=context_check)
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
