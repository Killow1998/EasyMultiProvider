"""Pure Responses request and response projection for external protocols."""

from __future__ import annotations

import base64
import binascii
import json
import re
import uuid
from typing import Any, Dict, Mapping, Optional
from urllib.parse import urlparse

from .dialects import (
    custom_tool_arguments,
    custom_tool_ids,
    custom_tool_input,
    portable_tool_definitions,
)
from .router_errors import (
    ExternalProtocolError,
    HistoryReconstructionError,
    RouterError,
)


_TEXTUAL_PROTOCOL_MARKERS = (
    "<think>",
    "</think>",
    "<tool_call>",
    "</tool_call>",
    "<|tool_call|>",
    "<|tool_calls|>",
)
_TEXTUAL_PROTOCOL_PROBE_BYTES = max(len(marker) for marker in _TEXTUAL_PROTOCOL_MARKERS) - 1
_COMPACTION_SUMMARY_PREFIX = (
    "Another language model started this task and produced a continuation summary. "
    "Use it to continue without repeating completed work:"
)
_COMPACTION_PREFIX = "emp1:"
_TEXT_PART_TYPES = frozenset({"input_text", "output_text", "text"})
_INTENTIONALLY_OMITTED_INPUT_TYPES = frozenset({"reasoning", "additional_tools"})
_RESPONSES_TERMINAL_STATUSES = frozenset({"completed", "incomplete", "failed"})
_RESPONSES_OUTPUT_TYPES = frozenset(
    {"message", "function_call", "custom_tool_call", "reasoning", "compaction"}
)
_ANTHROPIC_IMAGE_MEDIA_TYPES = frozenset(
    {"image/jpeg", "image/png", "image/gif", "image/webp"}
)


def _response_string(item: Mapping[str, Any], field: str) -> str:
    value = item.get(field)
    if not isinstance(value, str) or not value:
        raise ExternalProtocolError(
            "upstream Responses JSON contains an invalid output item"
        )
    return value


def _validate_responses_output_item(item: Mapping[str, Any]) -> None:
    item_type = item.get("type")
    if item_type not in _RESPONSES_OUTPUT_TYPES:
        raise ExternalProtocolError(
            "upstream Responses JSON contains an unsupported output item"
        )
    if item_type == "message":
        if item.get("role") != "assistant" or not isinstance(item.get("content"), list):
            raise ExternalProtocolError(
                "upstream Responses JSON contains an invalid message item"
            )
        for part in item["content"]:
            if not isinstance(part, Mapping):
                raise ExternalProtocolError(
                    "upstream Responses JSON contains invalid message content"
                )
            part_type = part.get("type")
            if part_type == "output_text":
                if not isinstance(part.get("text"), str):
                    raise ExternalProtocolError(
                        "upstream Responses JSON contains invalid message content"
                    )
                annotations = part.get("annotations")
                if annotations is not None and not isinstance(annotations, list):
                    raise ExternalProtocolError(
                        "upstream Responses JSON contains invalid message content"
                    )
            elif part_type == "refusal":
                if not isinstance(part.get("refusal"), str):
                    raise ExternalProtocolError(
                        "upstream Responses JSON contains invalid message content"
                    )
            else:
                raise ExternalProtocolError(
                    "upstream Responses JSON contains unsupported message content"
                )
        return
    if item_type == "function_call":
        _response_string(item, "call_id")
        _response_string(item, "name")
        _upstream_tool_arguments(item.get("arguments"), "Responses")
        return
    if item_type == "custom_tool_call":
        _response_string(item, "call_id")
        _response_string(item, "name")
        _response_string(item, "input")
        return
    if item_type == "reasoning":
        summary = item.get("summary")
        if summary is not None:
            if not isinstance(summary, list) or any(
                not isinstance(part, Mapping)
                or part.get("type") != "summary_text"
                or not isinstance(part.get("text"), str)
                for part in summary
            ):
                raise ExternalProtocolError(
                    "upstream Responses JSON contains invalid reasoning output"
                )
        opaque = item.get("encrypted_content")
        if opaque is not None and not isinstance(opaque, str):
            raise ExternalProtocolError(
                "upstream Responses JSON contains invalid reasoning output"
            )
        return
    _response_string(item, "encrypted_content")


def validate_responses_body(
    value: Any, validate_output_items: bool = True
) -> Dict[str, Any]:
    """Validate one complete Responses body without trusting HTTP 2xx alone."""

    if not isinstance(value, Mapping):
        raise ExternalProtocolError("upstream Responses JSON is not an object")
    status = value.get("status")
    if not isinstance(status, str) or not status:
        raise ExternalProtocolError("upstream Responses JSON has no response status")
    if status not in _RESPONSES_TERMINAL_STATUSES:
        raise ExternalProtocolError(
            "upstream Responses JSON has an unknown response status"
        )
    output = value.get("output")
    if not isinstance(output, list):
        raise ExternalProtocolError("upstream Responses JSON has no valid output")
    for item in output:
        if not isinstance(item, Mapping):
            raise ExternalProtocolError(
                "upstream Responses JSON contains an invalid output item"
            )
        if validate_output_items:
            _validate_responses_output_item(item)
    if "output_text" in value and not isinstance(value.get("output_text"), str):
        raise ExternalProtocolError("upstream Responses JSON has invalid output text")
    error = value.get("error")
    if status == "failed":
        if not isinstance(error, Mapping) or not error:
            raise ExternalProtocolError(
                "upstream Responses JSON has a failed status without an error"
            )
    elif error not in (None, {}):
        raise ExternalProtocolError(
            "upstream Responses JSON has a contradictory error"
        )
    incomplete_details = value.get("incomplete_details")
    if status == "incomplete":
        if incomplete_details is not None and not isinstance(
            incomplete_details, Mapping
        ):
            raise ExternalProtocolError(
                "upstream Responses JSON has invalid incomplete details"
            )
    elif incomplete_details not in (None, {}):
        raise ExternalProtocolError(
            "upstream Responses JSON has contradictory incomplete details"
        )
    return dict(value)


def responses_terminal_observation(
    value: Any, validate_output_items: bool = True
) -> Dict[str, Any]:
    """Return the diagnostic terminal represented by a validated body."""

    response = validate_responses_body(value, validate_output_items)
    status = response["status"]
    if status == "completed":
        return {"status": 200, "success": True, "error_class": "none"}
    if status == "incomplete":
        details = response.get("incomplete_details")
        reason = str(details.get("reason") or "") if isinstance(details, Mapping) else ""
        return {
            "status": 200,
            "success": False,
            "error_class": {
                "max_output_tokens": "output_limit",
                "content_filter": "content_filter",
            }.get(reason, "stream_incomplete"),
        }
    return {"status": 502, "success": False, "error_class": "stream_error"}


def _content_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for item in content:
        if isinstance(item, str):
            parts.append(item)
        elif isinstance(item, dict) and item.get("type") in ("input_text", "output_text", "text"):
            parts.append(str(item.get("text", "")))
    return "".join(parts)


def _request_text(content: Any, field: str = "content") -> str:
    """Return representable request text or reject instead of dropping parts."""

    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise RouterError("request projection failed: invalid %s" % field, 422)
    parts = []
    for item in content:
        if isinstance(item, str):
            parts.append(item)
            continue
        if not isinstance(item, Mapping) or item.get("type") not in _TEXT_PART_TYPES:
            raise RouterError("request projection failed: unsupported %s" % field, 422)
        text = item.get("text")
        if not isinstance(text, str):
            raise RouterError("request projection failed: invalid %s" % field, 422)
        parts.append(text)
    return "".join(parts)


def _required_string(value: Any, field: str) -> str:
    if not isinstance(value, str) or not value:
        raise RouterError("request projection failed: invalid %s" % field, 422)
    return value


def _request_tool_arguments(value: Any) -> str:
    if not isinstance(value, str):
        raise RouterError("request projection failed: invalid tool arguments", 422)
    try:
        decoded = json.loads(value)
    except (TypeError, ValueError):
        raise RouterError("request projection failed: invalid tool arguments", 422) from None
    if not isinstance(decoded, dict):
        raise RouterError("request projection failed: invalid tool arguments", 422)
    return value


def _upstream_tool_arguments(value: Any, protocol: str) -> str:
    if not isinstance(value, str):
        raise ExternalProtocolError(
            "%s upstream returned invalid tool arguments" % protocol
        )
    try:
        decoded = json.loads(value)
    except (TypeError, ValueError):
        raise ExternalProtocolError(
            "%s upstream returned invalid tool arguments" % protocol
        ) from None
    if not isinstance(decoded, dict):
        raise ExternalProtocolError(
            "%s upstream returned invalid tool arguments" % protocol
        )
    return value


def _chat_content(content: Any) -> Any:
    """Translate Responses message parts without losing image order."""

    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise RouterError("request projection failed: invalid message content", 422)
    result = []
    has_image = False
    for item in content:
        if isinstance(item, str):
            result.append({"type": "text", "text": item})
            continue
        if not isinstance(item, Mapping):
            raise RouterError("request projection failed: invalid message content", 422)
        item_type = item.get("type")
        if item_type in _TEXT_PART_TYPES:
            text = item.get("text")
            if not isinstance(text, str):
                raise RouterError("request projection failed: invalid message content", 422)
            result.append({"type": "text", "text": text})
            continue
        if item_type not in {"input_image", "output_image"}:
            raise RouterError("request projection failed: unsupported message content", 422)
        image_url = item.get("image_url")
        if isinstance(image_url, dict):
            image_url = image_url.get("url")
        if not isinstance(image_url, str) or not image_url:
            raise RouterError("request projection failed: invalid image", 422)
        projected_image = {"url": image_url}
        if item.get("detail") in {"auto", "low", "high", "original"}:
            projected_image["detail"] = item["detail"]
        result.append({"type": "image_url", "image_url": projected_image})
        has_image = True
    if has_image:
        return result
    return "".join(item["text"] for item in result if item["type"] == "text")


def _message_item(text: str) -> Dict[str, Any]:
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def _encode_compaction(summary: str) -> str:
    encoded = base64.urlsafe_b64encode(summary.encode("utf-8")).decode("ascii")
    return _COMPACTION_PREFIX + encoded


def _decode_compaction(item: Dict[str, Any]) -> str:
    value = item.get("encrypted_content")
    if not isinstance(value, str) or not value.startswith(_COMPACTION_PREFIX):
        return ""
    encoded = value[len(_COMPACTION_PREFIX):]
    try:
        return base64.b64decode(encoded, altchars=b"-_", validate=True).decode("utf-8")
    except (UnicodeDecodeError, ValueError):
        return ""


def _normalize_compaction_input(
    source: Any,
    drop_trigger: bool = False,
) -> Any:
    if not isinstance(source, list):
        return source
    result = []
    for item in source:
        if not isinstance(item, dict):
            result.append(item)
            continue
        if drop_trigger and item.get("type") == "compaction_trigger":
            continue
        if item.get("type") == "compaction":
            summary = _decode_compaction(item)
            if summary:
                result.append(_message_item(_COMPACTION_SUMMARY_PREFIX + "\n\n" + summary))
                continue
            raise HistoryReconstructionError("history_projection_incomplete")
        result.append(item)
    return result


def _messages(body: Dict[str, Any]) -> list:
    messages = []
    instructions = body.get("instructions")
    if instructions is not None:
        messages.append({"role": "system", "content": _request_text(instructions, "instructions")})
    source = body.get("input", "")
    if isinstance(source, str):
        messages.append({"role": "user", "content": source})
        return messages
    if isinstance(source, dict):
        source = [source]
    if not isinstance(source, list):
        raise RouterError("request projection failed: invalid input", 422)
    source = _normalize_compaction_input(source)
    pending_calls = []
    call_ids = set()
    output_ids = set()

    def flush_calls() -> None:
        if pending_calls:
            messages.append(
                {"role": "assistant", "content": None, "tool_calls": list(pending_calls)}
            )
            pending_calls.clear()

    for item in source:
        if not isinstance(item, Mapping):
            raise RouterError("request projection failed: invalid input item", 422)
        item_type = item.get("type", "message")
        if item_type == "message":
            flush_calls()
            role = item.get("role", "user")
            if role not in {"user", "assistant", "system", "developer"}:
                raise RouterError("request projection failed: unsupported message role", 422)
            messages.append({"role": role, "content": _chat_content(item.get("content", ""))})
        elif item_type in {"function_call", "custom_tool_call"}:
            arguments = (
                custom_tool_arguments(item.get("input", ""))
                if item_type == "custom_tool_call"
                else _request_tool_arguments(item.get("arguments", "{}"))
            )
            call_id = _required_string(
                item.get("call_id") or item.get("id"), "tool call ID"
            )
            if call_id in call_ids:
                raise RouterError(
                    "request projection failed: duplicate tool call ID", 422
                )
            call_ids.add(call_id)
            name = _required_string(item.get("name"), "tool name")
            call = {
                "id": call_id,
                "type": "function",
                "function": {
                    "name": name,
                    "arguments": arguments,
                },
            }
            if isinstance(item.get("extra_content"), Mapping):
                call["extra_content"] = dict(item["extra_content"])
            pending_calls.append(call)
        elif item_type in {"function_call_output", "custom_tool_call_output"}:
            flush_calls()
            call_id = _required_string(item.get("call_id"), "tool call ID")
            if call_id not in call_ids or call_id in output_ids:
                raise RouterError(
                    "request projection failed: invalid tool output pairing", 422
                )
            output_ids.add(call_id)
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": _request_text(item.get("output", ""), "tool output"),
                }
            )
        elif item_type not in _INTENTIONALLY_OMITTED_INPUT_TYPES:
            flush_calls()
            raise RouterError("request projection failed: unsupported input item", 422)
    flush_calls()
    return messages


def _tools(body: Dict[str, Any]) -> list:
    result = []
    for function in portable_tool_definitions(body):
        result.append(
            {
                "type": "function",
                "function": {
                    "name": function.get("name", "tool"),
                    "description": function.get("description", ""),
                    "parameters": function.get("parameters", {}),
                },
            }
        )
    return result


def _chat_tool_choice(value: Any) -> Any:
    if isinstance(value, str) and value in {"auto", "none", "required"}:
        return value
    if isinstance(value, Mapping) and value.get("type") in {"function", "custom"}:
        name = value.get("name")
        if isinstance(name, str) and name:
            return {"type": "function", "function": {"name": name}}
    raise RouterError("request projection failed: unsupported tool choice", 422)


def responses_to_chat(body: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    payload = {"model": upstream_model, "messages": _messages(body), "stream": bool(body.get("stream"))}
    tools = _tools(body)
    if tools:
        payload["tools"] = tools
    if "tool_choice" in body:
        payload["tool_choice"] = _chat_tool_choice(body["tool_choice"])
    if "parallel_tool_calls" in body:
        if not isinstance(body["parallel_tool_calls"], bool):
            raise RouterError(
                "request projection failed: parallel tool calls must be boolean",
                422,
            )
        payload["parallel_tool_calls"] = body["parallel_tool_calls"]
    for source, target in (
        ("temperature", "temperature"),
        ("top_p", "top_p"),
        ("stop", "stop"),
        ("max_output_tokens", "max_tokens"),
    ):
        if source in body:
            payload[target] = body[source]
    reasoning = body.get("reasoning")
    if isinstance(reasoning, dict) and isinstance(reasoning.get("effort"), str):
        payload["reasoning_effort"] = reasoning["effort"]
    return payload


def _anthropic_content(content: Any) -> list:
    if isinstance(content, str):
        return [{"type": "text", "text": content}]
    if not isinstance(content, list):
        raise RouterError("request projection failed: invalid Anthropic content", 422)
    result = []
    for item in content:
        if isinstance(item, str):
            result.append({"type": "text", "text": item})
            continue
        if not isinstance(item, dict):
            raise RouterError("request projection failed: invalid Anthropic content", 422)
        item_type = item.get("type")
        if item_type in _TEXT_PART_TYPES:
            text = item.get("text")
            if not isinstance(text, str):
                raise RouterError("request projection failed: invalid Anthropic content", 422)
            result.append({"type": "text", "text": text})
            continue
        if item_type != "input_image":
            raise RouterError(
                "request projection failed: unsupported Anthropic content", 422
            )
        image_url = item.get("image_url")
        if isinstance(image_url, Mapping):
            image_url = image_url.get("url")
        if not isinstance(image_url, str) or not image_url:
            raise RouterError("request projection failed: invalid image", 422)
        if image_url.startswith("data:"):
            header, separator, data = image_url.partition(",")
            match = re.fullmatch(
                r"data:(image/[A-Za-z0-9.+-]+);base64", header, re.IGNORECASE
            )
            if not separator or match is None or not data:
                raise RouterError("request projection failed: invalid image", 422)
            media_type = match.group(1).lower()
            if media_type not in _ANTHROPIC_IMAGE_MEDIA_TYPES:
                raise RouterError(
                    "request projection failed: unsupported Anthropic image type", 422
                )
            try:
                base64.b64decode(data, validate=True)
            except (binascii.Error, ValueError):
                raise RouterError("request projection failed: invalid image", 422) from None
            result.append(
                {
                    "type": "image",
                    "source": {
                        "type": "base64",
                        "media_type": media_type,
                        "data": data,
                    },
                }
            )
            continue
        parsed = urlparse(image_url)
        if parsed.scheme not in {"http", "https"} or not parsed.netloc:
            raise RouterError("request projection failed: unsupported image URL", 422)
        result.append(
            {
                "type": "image",
                "source": {"type": "url", "url": image_url},
            }
        )
    return result


def _anthropic_tool_input(item_type: str, item: Mapping[str, Any]) -> Dict[str, Any]:
    if item_type == "custom_tool_call":
        arguments = custom_tool_arguments(item.get("input", ""))
    else:
        arguments = item.get("arguments", "{}")
    if isinstance(arguments, Mapping):
        value = dict(arguments)
    elif isinstance(arguments, str):
        try:
            value = json.loads(arguments)
        except (TypeError, ValueError):
            raise RouterError("request projection failed: invalid tool arguments", 422) from None
    else:
        raise RouterError("request projection failed: invalid tool arguments", 422)
    if not isinstance(value, dict):
        raise RouterError("request projection failed: invalid tool arguments", 422)
    return value


def _anthropic_messages(body: Dict[str, Any]) -> list:
    source = body.get("input", "")
    if isinstance(source, str):
        source = [{"type": "message", "role": "user", "content": source}]
    elif isinstance(source, dict):
        source = [source]
    if not isinstance(source, list):
        raise RouterError("request projection failed: invalid input", 422)
    source = _normalize_compaction_input(source)
    messages = []
    pending_calls = []
    pending_results = []
    call_ids = set()
    output_ids = set()

    def flush_pending() -> None:
        if pending_calls:
            messages.append({"role": "assistant", "content": list(pending_calls)})
            pending_calls.clear()
        if pending_results:
            messages.append({"role": "user", "content": list(pending_results)})
            pending_results.clear()

    for item in source:
        if not isinstance(item, Mapping):
            raise RouterError("request projection failed: invalid input item", 422)
        item_type = item.get("type", "message")
        if item_type == "message":
            flush_pending()
            role = item.get("role", "user")
            if role not in ("user", "assistant"):
                raise RouterError("request projection failed: unsupported Anthropic role", 422)
            content = _anthropic_content(item.get("content", ""))
            if not content:
                raise RouterError("request projection failed: empty Anthropic content", 422)
            messages.append({"role": role, "content": content})
        elif item_type in {"function_call", "custom_tool_call"}:
            if pending_results:
                messages.append({"role": "user", "content": list(pending_results)})
                pending_results.clear()
            arguments = _anthropic_tool_input(item_type, item)
            call_id = _required_string(
                item.get("call_id") or item.get("id"), "tool call ID"
            )
            if call_id in call_ids:
                raise RouterError(
                    "request projection failed: duplicate tool call ID", 422
                )
            call_ids.add(call_id)
            name = _required_string(item.get("name"), "tool name")
            pending_calls.append(
                {
                    "type": "tool_use",
                    "id": call_id,
                    "name": name,
                    "input": arguments,
                }
            )
        elif item_type in {"function_call_output", "custom_tool_call_output"}:
            if pending_calls:
                messages.append({"role": "assistant", "content": list(pending_calls)})
                pending_calls.clear()
            call_id = _required_string(item.get("call_id"), "tool call ID")
            if call_id not in call_ids or call_id in output_ids:
                raise RouterError(
                    "request projection failed: invalid tool output pairing", 422
                )
            output_ids.add(call_id)
            pending_results.append(
                {
                    "type": "tool_result",
                    "tool_use_id": call_id,
                    "content": _request_text(item.get("output", ""), "tool output"),
                }
            )
        elif item_type not in _INTENTIONALLY_OMITTED_INPUT_TYPES:
            flush_pending()
            raise RouterError("request projection failed: unsupported input item", 422)
    flush_pending()
    return messages


def _anthropic_tools(body: Dict[str, Any]) -> list:
    result = []
    for item in _tools(body):
        function = item["function"]
        result.append(
            {
                "name": function["name"],
                "description": function.get("description", ""),
                "input_schema": function.get("parameters", {}),
            }
        )
    return result


def _anthropic_tool_choice(value: Any, parallel: Any = None) -> Dict[str, Any]:
    if value == "auto":
        choice = {"type": "auto"}
    elif value == "required":
        choice = {"type": "any"}
    elif value == "none":
        choice = {"type": "none"}
    elif isinstance(value, Mapping) and value.get("type") in {"function", "custom"}:
        name = value.get("name")
        if not isinstance(name, str) or not name:
            raise RouterError("request projection failed: unsupported tool choice", 422)
        choice = {"type": "tool", "name": name}
    else:
        raise RouterError("request projection failed: unsupported tool choice", 422)
    if parallel is False:
        choice["disable_parallel_tool_use"] = True
    return choice


def responses_to_anthropic(body: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    payload = {
        "model": upstream_model,
        "max_tokens": int(body.get("max_output_tokens", 4096) or 4096),
        "messages": _anthropic_messages(body),
        "stream": bool(body.get("stream")),
    }
    instructions = body.get("instructions")
    if instructions is not None:
        payload["system"] = _request_text(instructions, "instructions")
    tools = _anthropic_tools(body)
    if tools:
        payload["tools"] = tools
    for source, target in (("temperature", "temperature"), ("top_p", "top_p")):
        if source in body:
            payload[target] = body[source]
    if "stop" in body:
        payload["stop_sequences"] = body["stop"] if isinstance(body["stop"], list) else [body["stop"]]
    parallel = body.get("parallel_tool_calls")
    if "parallel_tool_calls" in body and not isinstance(parallel, bool):
        raise RouterError(
            "request projection failed: parallel tool calls must be boolean", 422
        )
    if "tool_choice" in body:
        payload["tool_choice"] = _anthropic_tool_choice(
            body["tool_choice"], parallel
        )
    elif parallel is False:
        payload["tool_choice"] = _anthropic_tool_choice("auto", parallel)
    return payload


def _chat_incomplete_reason(finish_reason: Any) -> Optional[str]:
    if finish_reason is None:
        raise ExternalProtocolError(
            "Chat Completions upstream returned no finish reason"
        )
    reason = str(finish_reason).lower()
    if reason in {"stop", "tool_calls", "function_call"}:
        return None
    if reason in {"length", "max_tokens", "max_output_tokens"}:
        return "max_output_tokens"
    if reason in {"content_filter", "content_filtered", "safety"}:
        return "content_filter"
    raise ExternalProtocolError(
        "Chat Completions upstream returned an unknown finish reason"
    )


def _anthropic_incomplete_reason(stop_reason: Any) -> Optional[str]:
    if stop_reason is None:
        raise ExternalProtocolError(
            "Anthropic upstream returned no stop reason"
        )
    reason = str(stop_reason).lower()
    if reason in {"end_turn", "tool_use", "stop_sequence"}:
        return None
    if reason in {"max_tokens", "max_output_tokens"}:
        return "max_output_tokens"
    if reason in {"content_filter", "content_filtered", "safety", "refusal"}:
        return "content_filter"
    raise ExternalProtocolError(
        "Anthropic upstream returned an unknown stop reason"
    )


def _response_from_chat(
    value: Dict[str, Any],
    requested_model: str,
    custom_names: set = None,
) -> Dict[str, Any]:
    error = value.get("error")
    if error:
        raise RouterError("Chat Completions upstream returned an error", 502)
    choices = value.get("choices")
    if not isinstance(choices, list) or not choices or not isinstance(choices[0], Mapping):
        raise ExternalProtocolError(
            "Chat Completions upstream returned an invalid response"
        )
    choice = choices[0]
    message = choice.get("message")
    if not isinstance(message, Mapping):
        raise ExternalProtocolError(
            "Chat Completions upstream returned an invalid response"
        )
    custom_names = custom_names or set()
    output = []
    raw_text = message.get("content")
    if raw_text is None:
        text = ""
    elif isinstance(raw_text, str):
        text = raw_text
    elif isinstance(raw_text, list):
        parts = []
        for part in raw_text:
            if not isinstance(part, Mapping) or part.get("type") not in {
                "text",
                "output_text",
            }:
                raise ExternalProtocolError(
                    "Chat Completions upstream returned invalid message content"
                )
            part_text = part.get("text")
            if not isinstance(part_text, str):
                raise ExternalProtocolError(
                    "Chat Completions upstream returned invalid message content"
                )
            parts.append(part_text)
        text = "".join(parts)
    else:
        raise ExternalProtocolError(
            "Chat Completions upstream returned invalid message content"
        )
    _validate_textual_protocol(text)
    if text:
        output.append(
            {
                "id": "msg_" + uuid.uuid4().hex,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": text, "annotations": []}],
            }
        )
    tool_calls = message.get("tool_calls", [])
    if not isinstance(tool_calls, list):
        raise ExternalProtocolError(
            "Chat Completions upstream returned an invalid tool call"
        )
    call_ids = set()
    for call in tool_calls:
        if not isinstance(call, Mapping):
            raise ExternalProtocolError(
                "Chat Completions upstream returned an invalid tool call"
            )
        function = call.get("function")
        raw_call_id = call.get("id")
        name = function.get("name") if isinstance(function, Mapping) else None
        if (
            not isinstance(raw_call_id, str)
            or not raw_call_id
            or not isinstance(name, str)
            or not name
        ):
            raise ExternalProtocolError(
                "Chat Completions upstream returned an invalid tool call"
            )
        if raw_call_id in call_ids:
            raise ExternalProtocolError(
                "Chat Completions upstream returned a duplicate tool call ID"
            )
        call_ids.add(raw_call_id)
        arguments = _upstream_tool_arguments(
            function.get("arguments"), "Chat Completions"
        )
        if name in custom_names:
            item_id, call_id = custom_tool_ids(raw_call_id, raw_call_id)
            item = {
                "id": item_id,
                "type": "custom_tool_call",
                "status": "completed",
                "call_id": call_id,
                "name": name,
                "input": custom_tool_input(arguments),
            }
        else:
            item = {
                "id": raw_call_id or "fc_" + uuid.uuid4().hex,
                "type": "function_call",
                "status": "completed",
                "call_id": raw_call_id,
                "name": name,
                "arguments": arguments,
            }
        if isinstance(call.get("extra_content"), Mapping):
            item["extra_content"] = dict(call["extra_content"])
        output.append(item)
    incomplete_reason = _chat_incomplete_reason(choice.get("finish_reason"))
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "incomplete" if incomplete_reason else "completed",
        "model": requested_model,
        "output": output,
        "output_text": text,
    }
    if incomplete_reason:
        response["incomplete_details"] = {"reason": incomplete_reason}
    if value.get("usage") is not None:
        response["usage"] = value["usage"]
    return response


def _upstream_error_text(error: Any) -> str:
    if isinstance(error, dict):
        return str(error.get("message") or error.get("type") or error.get("code") or error)
    return str(error)


def _validate_textual_protocol(value: Any) -> None:
    text = str(value or "")
    if any(marker in text for marker in _TEXTUAL_PROTOCOL_MARKERS):
        raise RouterError(
            "upstream returned textual reasoning/tool-call markup; "
            "this endpoint must return structured tool calls for Codex",
            502,
        )


def _advance_textual_protocol_probe(previous: str, piece: str) -> str:
    """Validate a full new text fragment while retaining only boundary suffix."""

    combined = str(previous or "") + str(piece or "")
    _validate_textual_protocol(combined)
    if _TEXTUAL_PROTOCOL_PROBE_BYTES <= 0:
        return ""
    return combined[-_TEXTUAL_PROTOCOL_PROBE_BYTES:]


def _anthropic_tool_arguments(value: Any) -> str:
    if not isinstance(value, Mapping):
        raise ExternalProtocolError("Anthropic upstream returned invalid tool input")
    try:
        return json.dumps(dict(value), ensure_ascii=False, separators=(",", ":"))
    except (TypeError, ValueError):
        raise ExternalProtocolError("Anthropic upstream returned invalid tool input") from None


def _response_from_anthropic(
    value: Dict[str, Any], requested_model: str, custom_names: set = None
) -> Dict[str, Any]:
    if value.get("error"):
        raise ExternalProtocolError("Anthropic upstream returned an error")
    output = []
    text = []
    custom_names = custom_names or set()
    content = value.get("content")
    if not isinstance(content, list):
        raise ExternalProtocolError("Anthropic upstream returned invalid content")
    call_ids = set()
    for block in content:
        if not isinstance(block, Mapping):
            raise ExternalProtocolError("Anthropic upstream returned invalid content")
        block_type = block.get("type")
        if block_type == "text":
            block_text = block.get("text")
            if not isinstance(block_text, str):
                raise ExternalProtocolError(
                    "Anthropic upstream returned invalid content"
                )
            _validate_textual_protocol(block_text)
            text.append(block_text)
            if block_text:
                output.append(
                    {
                        "id": "msg_" + uuid.uuid4().hex,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{
                            "type": "output_text",
                            "text": block_text,
                            "annotations": [],
                        }],
                    }
                )
        elif block_type == "tool_use":
            raw_call_id = block.get("id")
            if not isinstance(raw_call_id, str) or not raw_call_id:
                raise ExternalProtocolError("Anthropic upstream returned an invalid tool call")
            name = block.get("name")
            if not isinstance(name, str) or not name:
                raise ExternalProtocolError(
                    "Anthropic upstream returned an invalid tool call"
                )
            if raw_call_id in call_ids:
                raise ExternalProtocolError(
                    "Anthropic upstream returned a duplicate tool call ID"
                )
            call_ids.add(raw_call_id)
            arguments = _anthropic_tool_arguments(block.get("input", {}))
            if name in custom_names:
                item_id, call_id = custom_tool_ids(raw_call_id, raw_call_id)
                output.append(
                    {
                        "id": item_id,
                        "type": "custom_tool_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": name,
                        "input": custom_tool_input(arguments),
                    }
                )
            else:
                output.append(
                    {
                        "id": raw_call_id,
                        "type": "function_call",
                        "status": "completed",
                        "call_id": raw_call_id,
                        "name": name,
                        "arguments": arguments,
                    }
                )
        else:
            raise ExternalProtocolError(
                "Anthropic upstream returned unsupported content"
            )
    final_text = "".join(text)
    incomplete_reason = _anthropic_incomplete_reason(value.get("stop_reason"))
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "incomplete" if incomplete_reason else "completed",
        "model": requested_model,
        "output": output,
        "output_text": final_text,
    }
    if incomplete_reason:
        response["incomplete_details"] = {"reason": incomplete_reason}
    usage = value.get("usage")
    if isinstance(usage, dict):
        response["usage"] = {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "total_tokens": usage.get("input_tokens", 0) + usage.get("output_tokens", 0),
        }
    return response
