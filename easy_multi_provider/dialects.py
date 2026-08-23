"""Destination-aware, content-safe Responses request projections."""

from __future__ import annotations

import base64
import copy
import hashlib
import json
import re
from typing import Any, Dict, Mapping


CODEX_NATIVE = "codex_native"
PORTABLE_RESPONSES = "portable_responses"
CHAT_COMPLETIONS = "chat_completions"
ANTHROPIC_MESSAGES = "anthropic_messages"

_PORTABLE_TOP_LEVEL = frozenset(
    {
        "model",
        "instructions",
        "input",
        "tools",
        "tool_choice",
        "parallel_tool_calls",
        "temperature",
        "top_p",
        "stop",
        "max_output_tokens",
        "stream",
        "reasoning",
        "text",
        "truncation",
    }
)
_TEXT_PART_TYPES = frozenset({"input_text", "output_text", "text"})
_COMPACTION_PREFIX = "emp1:"
_COMPACTION_SUMMARY_PREFIX = (
    "Another model produced a continuation summary. Continue from this summary:"
)
_SHAPE_ID = re.compile(r"^[A-Za-z0-9_.:-]{1,64}$")
_CUSTOM_TOOL_PARAMETERS = {
    "type": "object",
    "properties": {"input": {"type": "string"}},
    "required": ["input"],
    "additionalProperties": False,
}


class ProjectionError(ValueError):
    """A bounded projection failure that never embeds conversation content."""

    def __init__(
        self,
        index: int,
        item_type: str,
        part_types: tuple[str, ...] = (),
        failure_class: str = "unsupported_item",
    ):
        self.index = max(0, int(index))
        self.item_type = str(item_type or "unknown")[:64]
        self.part_types = tuple(str(item)[:64] for item in part_types[:16])
        self.failure_class = str(failure_class or "projection_error")[:64]
        parts = ",".join(self.part_types) if self.part_types else "none"
        super().__init__(
            "request projection failed: index=%d type=%s parts=%s class=%s"
            % (self.index, self.item_type, parts, self.failure_class)
        )


def classify_dialect(provider: Mapping[str, Any]) -> str:
    """Classify request dialect independently from endpoint protocol."""

    protocol = provider.get("protocol")
    if protocol == "responses":
        if provider.get("auth_mode") in {"account", "forward"}:
            return CODEX_NATIVE
        return PORTABLE_RESPONSES
    if protocol == "anthropic_messages":
        return ANTHROPIC_MESSAGES
    return CHAT_COMPLETIONS


def _project_content(content: Any, index: int) -> Any:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise ProjectionError(index, "message", (), "invalid_content")
    result = []
    part_types = tuple(
        str(item.get("type") or "unknown")
        for item in content
        if isinstance(item, Mapping)
    )
    for part in content:
        if isinstance(part, str):
            result.append(part)
            continue
        if not isinstance(part, Mapping):
            raise ProjectionError(index, "message", part_types, "invalid_content_part")
        part_type = part.get("type")
        if part_type in _TEXT_PART_TYPES and isinstance(part.get("text"), str):
            result.append({"type": part_type, "text": part["text"]})
            continue
        if part_type in {"input_image", "output_image"}:
            image_url = part.get("image_url")
            if isinstance(image_url, Mapping):
                image_url = image_url.get("url")
            if isinstance(image_url, str) and image_url:
                projected = {"type": part_type, "image_url": image_url}
                if part.get("detail") in {"auto", "low", "high", "original"}:
                    projected["detail"] = part["detail"]
                result.append(projected)
                continue
        raise ProjectionError(index, "message", part_types, "unsupported_content_part")
    return result


def _instruction_text(content: Any, index: int) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        raise ProjectionError(index, "message", (), "invalid_instruction_content")
    part_types = tuple(
        str(part.get("type") or "unknown")
        if isinstance(part, Mapping)
        else "text"
        if isinstance(part, str)
        else "unknown"
        for part in content
    )
    texts = []
    for part in content:
        if isinstance(part, str):
            texts.append(part)
            continue
        if (
            isinstance(part, Mapping)
            and part.get("type") in _TEXT_PART_TYPES
            and isinstance(part.get("text"), str)
        ):
            texts.append(part["text"])
            continue
        raise ProjectionError(
            index,
            "message",
            part_types,
            "unsupported_instruction_content",
        )
    return "\n".join(texts)


def _custom_tool_arguments(value: Any) -> str:
    if not isinstance(value, str):
        value = json.dumps(value, ensure_ascii=False, separators=(",", ":"))
    return json.dumps(
        {"input": value},
        ensure_ascii=False,
        separators=(",", ":"),
    )


def custom_tool_arguments(value: Any) -> str:
    return _custom_tool_arguments(value)


def custom_tool_input(arguments: Any) -> str:
    if isinstance(arguments, Mapping):
        value = arguments.get("input", arguments)
    elif isinstance(arguments, str):
        try:
            value = json.loads(arguments)
        except ValueError:
            return arguments
        if isinstance(value, Mapping) and "input" in value:
            value = value["input"]
    else:
        return str(arguments)
    if isinstance(value, str):
        return value
    return json.dumps(value, ensure_ascii=False, separators=(",", ":"))


def custom_tool_ids(raw_id: Any, call_id: Any = None) -> tuple[str, str]:
    paired = call_id if isinstance(call_id, str) and call_id else raw_id
    paired = paired if isinstance(paired, str) and paired else "call_unknown"
    if isinstance(raw_id, str) and raw_id.startswith("ctc_"):
        return raw_id, paired
    digest = hashlib.sha256(paired.encode("utf-8", "replace")).hexdigest()[:24]
    return "ctc_" + digest, paired


def _raw_tools(body: Mapping[str, Any]) -> list:
    collected = []

    def visit(items: Any) -> None:
        if not isinstance(items, list):
            return
        for item in items:
            if not isinstance(item, Mapping):
                continue
            if item.get("type") == "namespace":
                visit(item.get("tools"))
            else:
                collected.append(item)

    visit(body.get("tools"))
    source = body.get("input")
    if isinstance(source, Mapping):
        source = [source]
    if isinstance(source, list):
        for item in source:
            if isinstance(item, Mapping) and item.get("type") == "additional_tools":
                visit(item.get("tools"))
    return collected


def custom_tool_names(body: Mapping[str, Any]) -> set:
    names = set()
    for item in _raw_tools(body):
        if item.get("type") != "custom":
            continue
        name = item.get("name")
        if isinstance(name, str) and name:
            names.add(name)
    return names


def portable_tool_definitions(body: Mapping[str, Any]) -> list:
    """Return the representable Responses function surface for an upstream."""

    raw = _raw_tools(body)
    return _portable_tools(raw) if raw else []


def _portable_input(source: Any) -> tuple[Any, list[str]]:
    if isinstance(source, (str, type(None))):
        return source, []
    if isinstance(source, Mapping):
        source = [source]
    if not isinstance(source, list):
        raise ProjectionError(0, "input", (), "invalid_input")
    result = []
    instructions = []
    calls = set()
    outputs = set()
    for index, item in enumerate(source):
        if not isinstance(item, Mapping):
            raise ProjectionError(index, "unknown", (), "invalid_item")
        item_type = str(item.get("type") or "message")
        if item_type in {"reasoning", "additional_tools"}:
            continue
        if item_type == "item_reference":
            raise ProjectionError(index, item_type, (), "opaque_item_reference")
        if item_type == "compaction":
            encoded = item.get("encrypted_content")
            if not isinstance(encoded, str) or not encoded.startswith(
                _COMPACTION_PREFIX
            ):
                raise ProjectionError(index, item_type, (), "opaque_compaction")
            try:
                summary = base64.b64decode(
                    encoded[len(_COMPACTION_PREFIX) :],
                    altchars=b"-_",
                    validate=True,
                ).decode("utf-8")
            except (UnicodeDecodeError, ValueError):
                raise ProjectionError(
                    index, item_type, (), "invalid_compaction"
                ) from None
            if not summary:
                raise ProjectionError(index, item_type, (), "invalid_compaction")
            result.append(
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {
                            "type": "input_text",
                            "text": _COMPACTION_SUMMARY_PREFIX + "\n\n" + summary,
                        }
                    ],
                }
            )
            continue
        if item_type == "message":
            role = item.get("role", "user")
            if role in {"system", "developer"}:
                instructions.append(_instruction_text(item.get("content", ""), index))
                continue
            if role not in {"user", "assistant"}:
                raise ProjectionError(index, item_type, (), "unsupported_role")
            result.append(
                {
                    "type": "message",
                    "role": role,
                    "content": _project_content(item.get("content", ""), index),
                }
            )
            continue
        if item_type in {"function_call", "custom_tool_call"}:
            call_id = item.get("call_id") or item.get("id")
            if not isinstance(call_id, str) or not call_id:
                raise ProjectionError(index, item_type, (), "invalid_tool_pair")
            arguments = (
                _custom_tool_arguments(item.get("input", ""))
                if item_type == "custom_tool_call"
                else item.get("arguments", "{}")
            )
            if not isinstance(arguments, str):
                raise ProjectionError(index, item_type, (), "invalid_tool_arguments")
            calls.add(call_id)
            result.append(
                {
                    "type": "function_call",
                    "call_id": call_id,
                    "name": str(item.get("name") or ""),
                    "arguments": arguments,
                }
            )
            continue
        if item_type in {"function_call_output", "custom_tool_call_output"}:
            call_id = item.get("call_id")
            if not isinstance(call_id, str) or not call_id or call_id not in calls:
                raise ProjectionError(index, item_type, (), "invalid_tool_pair")
            if call_id in outputs:
                raise ProjectionError(index, item_type, (), "duplicate_tool_output")
            output = item.get("output", "")
            if not isinstance(output, (str, list)):
                raise ProjectionError(index, item_type, (), "invalid_tool_output")
            outputs.add(call_id)
            result.append(
                {"type": "function_call_output", "call_id": call_id, "output": copy.deepcopy(output)}
            )
            continue
        part_types = tuple(
            str(part.get("type") or "unknown")
            for part in item.get("content", [])
            if isinstance(part, Mapping)
        ) if isinstance(item.get("content"), list) else ()
        raise ProjectionError(index, item_type, part_types)
    return result, instructions


def _native_input(source: Any) -> Any:
    if not isinstance(source, list):
        return copy.deepcopy(source)
    result = []
    for index, item in enumerate(source):
        if isinstance(item, Mapping) and item.get("type") == "compaction":
            encoded = item.get("encrypted_content")
            if isinstance(encoded, str) and encoded.startswith(_COMPACTION_PREFIX):
                try:
                    summary = base64.b64decode(
                        encoded[len(_COMPACTION_PREFIX) :],
                        altchars=b"-_",
                        validate=True,
                    ).decode("utf-8")
                except (UnicodeDecodeError, ValueError):
                    raise ProjectionError(
                        index, "compaction", (), "invalid_compaction"
                    ) from None
                if not summary:
                    raise ProjectionError(
                        index, "compaction", (), "invalid_compaction"
                    )
                result.append(
                    {
                        "type": "message",
                        "role": "user",
                        "content": [
                            {
                                "type": "input_text",
                                "text": _COMPACTION_SUMMARY_PREFIX
                                + "\n\n"
                                + summary,
                            }
                        ],
                    }
                )
                continue
        if not isinstance(item, Mapping) or item.get("type") != "reasoning":
            result.append(copy.deepcopy(item))
            continue
        if not isinstance(item.get("encrypted_content"), str) or not item.get(
            "encrypted_content"
        ):
            continue
        opaque = copy.deepcopy(dict(item))
        for field in ("content", "text", "reasoning_text", "thinking"):
            opaque.pop(field, None)
        result.append(opaque)
    return result


def _portable_tools(source: Any) -> list:
    if source is None:
        return []
    if not isinstance(source, list):
        raise ProjectionError(0, "tools", (), "invalid_tools")
    result = []
    seen = set()
    for index, item in enumerate(source):
        if not isinstance(item, Mapping):
            raise ProjectionError(index, "unknown", (), "invalid_tool_definition")
        tool_type = str(item.get("type") or "unknown")
        if tool_type not in {"function", "custom"}:
            raise ProjectionError(index, tool_type, (), "unsupported_tool_definition")
        function = item.get("function") if isinstance(item.get("function"), Mapping) else item
        name = function.get("name")
        parameters = (
            _CUSTOM_TOOL_PARAMETERS
            if tool_type == "custom"
            else function.get("parameters", {})
        )
        if not isinstance(name, str) or not name or not isinstance(parameters, Mapping):
            raise ProjectionError(index, tool_type, (), "invalid_tool_definition")
        if name in seen:
            continue
        seen.add(name)
        projected = {
            "type": "function",
            "name": name,
            "description": str(function.get("description") or ""),
            "parameters": copy.deepcopy(dict(parameters)),
        }
        if isinstance(function.get("strict"), bool):
            projected["strict"] = function["strict"]
        result.append(projected)
    return result


def project_request(provider: Mapping[str, Any], body: Mapping[str, Any]) -> Dict[str, Any]:
    """Build an ephemeral destination view without retaining request content."""

    dialect = classify_dialect(provider)
    if dialect == CODEX_NATIVE:
        projected = copy.deepcopy(dict(body))
        projected["input"] = _native_input(body.get("input"))
        return projected
    if dialect != PORTABLE_RESPONSES:
        return copy.deepcopy(dict(body))
    projected = {
        key: copy.deepcopy(value)
        for key, value in body.items()
        if key in _PORTABLE_TOP_LEVEL
    }
    projected_input, hoisted_instructions = _portable_input(body.get("input"))
    projected["input"] = projected_input
    existing_instructions = projected.get("instructions")
    if existing_instructions is not None and not isinstance(
        existing_instructions, str
    ):
        raise ProjectionError(0, "instructions", (), "invalid_instruction_content")
    instruction_parts = []
    if isinstance(existing_instructions, str):
        instruction_parts.append(existing_instructions)
    instruction_parts.extend(hoisted_instructions)
    if instruction_parts:
        projected["instructions"] = "\n\n".join(instruction_parts)
    raw_tools = _raw_tools(body)
    if raw_tools:
        projected["tools"] = _portable_tools(raw_tools)
        if not projected["tools"]:
            projected.pop("tools")
    elif "tools" in projected:
        projected.pop("tools")
    tool_choice = projected.get("tool_choice")
    if isinstance(tool_choice, Mapping) and tool_choice.get("type") == "custom":
        projected["tool_choice"] = dict(tool_choice, type="function")
    stream = projected.get("stream")
    if stream is None:
        # LiteLLM and some Responses-compatible gateways internally validate
        # this as ChatCompletionRequest.stream, where null is rejected.
        projected["stream"] = False
    elif not isinstance(stream, bool):
        raise ProjectionError(0, "stream", (), "invalid_stream")
    return projected


def project_response(
    provider: Mapping[str, Any],
    response: Mapping[str, Any],
    custom_names: set = None,
) -> Dict[str, Any]:
    """Remove external plaintext reasoning while preserving final output items."""

    projected = copy.deepcopy(dict(response))
    if classify_dialect(provider) == CODEX_NATIVE:
        return projected
    for field in ("reasoning", "reasoning_text", "reasoning_content", "thinking"):
        projected.pop(field, None)
    output = []
    custom_names = custom_names or set()
    for index, item in enumerate(
        projected.get("output", [])
        if isinstance(projected.get("output"), list)
        else []
    ):
        if not isinstance(item, Mapping) or item.get("type") == "reasoning":
            continue
        if item.get("type") == "compaction":
            raise ProjectionError(
                index, "compaction", (), "external_compaction"
            )
        clean = copy.deepcopy(dict(item))
        for field in ("reasoning", "reasoning_text", "reasoning_content", "thinking"):
            clean.pop(field, None)
        if isinstance(clean.get("content"), list):
            clean["content"] = [
                part
                for part in clean["content"]
                if not (
                    isinstance(part, Mapping)
                    and str(part.get("type") or "").lower()
                    in {"reasoning", "reasoning_text", "thinking", "thinking_text"}
                )
            ]
        if clean.get("type") == "function_call" and clean.get("name") in custom_names:
            item_id, call_id = custom_tool_ids(
                clean.get("id"), clean.get("call_id")
            )
            clean["id"] = item_id
            clean["call_id"] = call_id
            clean["type"] = "custom_tool_call"
            clean["input"] = custom_tool_input(clean.pop("arguments", ""))
        output.append(clean)
    if isinstance(projected.get("output"), list):
        projected["output"] = output
    return projected


def _shape_id(value: Any) -> str:
    value = str(value or "unknown")
    return value if _SHAPE_ID.fullmatch(value) else "unknown"


def request_shape(body: Mapping[str, Any]) -> Dict[str, Any]:
    """Return bounded structural facts without names, IDs, arguments, or content."""

    source = body.get("input")
    if isinstance(source, Mapping):
        source = [source]
    if not isinstance(source, list):
        source = []
    item_types = []
    part_types = []
    calls = set()
    outputs = set()
    invalid_pair = False
    for item in source[:256]:
        if not isinstance(item, Mapping):
            item_types.append("unknown")
            continue
        item_type = _shape_id(item.get("type") or "message")
        item_types.append(item_type)
        content = item.get("content")
        if isinstance(content, list):
            for part in content:
                if len(part_types) >= 256:
                    break
                if isinstance(part, Mapping):
                    part_types.append(_shape_id(part.get("type")))
                elif isinstance(part, str):
                    part_types.append("text")
                else:
                    part_types.append("unknown")
        if item_type in {"function_call", "custom_tool_call"}:
            call_id = item.get("call_id") or item.get("id")
            if isinstance(call_id, str) and call_id:
                calls.add(call_id)
            else:
                invalid_pair = True
        elif item_type in {"function_call_output", "custom_tool_call_output"}:
            call_id = item.get("call_id")
            if not isinstance(call_id, str) or not call_id or call_id in outputs:
                invalid_pair = True
            else:
                outputs.add(call_id)
                if call_id not in calls:
                    invalid_pair = True
    if invalid_pair:
        pairing = "invalid"
    elif not calls and not outputs:
        pairing = "none"
    elif calls == outputs:
        pairing = "paired"
    else:
        pairing = "incomplete"
    return {
        "request_item_count": min(len(source), 256),
        "request_item_types": item_types,
        "content_part_types": part_types,
        "tool_pairing_status": pairing,
    }


def project_stream_event(
    provider: Mapping[str, Any],
    event: Mapping[str, Any],
    suppressed_item_ids: set,
    custom_names: set = None,
    custom_state: Dict[str, Dict[str, Any]] = None,
) -> Dict[str, Any] | None:
    """Project one Responses stream event without retaining reasoning text."""

    projected = copy.deepcopy(dict(event))
    custom_names = custom_names or set()
    custom_state = custom_state if custom_state is not None else {}
    if classify_dialect(provider) == CODEX_NATIVE:
        return projected
    event_type = str(projected.get("type") or "").lower()
    item = projected.get("item")
    if isinstance(item, Mapping) and item.get("type") == "compaction":
        output_index = projected.get("output_index")
        index = (
            output_index
            if isinstance(output_index, int) and not isinstance(output_index, bool)
            else 0
        )
        raise ProjectionError(index, "compaction", (), "external_compaction")
    if isinstance(item, Mapping) and item.get("type") == "reasoning":
        item_id = item.get("id")
        if isinstance(item_id, str) and item_id:
            suppressed_item_ids.add(item_id)
        return None
    if (
        isinstance(item, Mapping)
        and item.get("type") == "function_call"
        and item.get("name") in custom_names
    ):
        raw_item_id = str(item.get("id") or item.get("call_id") or "")
        item_id, call_id = custom_tool_ids(raw_item_id, item.get("call_id"))
        state = custom_state.setdefault(
            raw_item_id,
            {
                "item_id": item_id,
                "call_id": call_id,
                "name": str(item.get("name") or ""),
                "arguments": "",
            },
        )
        clean_item = dict(item)
        clean_item["id"] = state["item_id"]
        clean_item["call_id"] = state["call_id"]
        clean_item["type"] = "custom_tool_call"
        arguments = clean_item.pop("arguments", "")
        if arguments:
            state["arguments"] = str(arguments)
        clean_item["input"] = custom_tool_input(state["arguments"])
        projected["item"] = clean_item
    raw_item_id = str(projected.get("item_id") or "")
    if raw_item_id in custom_state:
        state = custom_state[raw_item_id]
        event_type = str(projected.get("type") or "")
        if event_type == "response.function_call_arguments.delta":
            state["arguments"] += str(projected.get("delta") or "")
            return None
        if event_type == "response.function_call_arguments.done":
            arguments = projected.get("arguments")
            if isinstance(arguments, str) and arguments:
                state["arguments"] = arguments
            projected["type"] = "response.custom_tool_call_input.delta"
            projected["item_id"] = state["item_id"]
            projected.pop("arguments", None)
            projected["delta"] = custom_tool_input(state["arguments"])
        else:
            projected["item_id"] = state["item_id"]
    item_id = projected.get("item_id")
    if isinstance(item_id, str) and item_id in suppressed_item_ids:
        return None
    if "reasoning" in event_type or "thinking" in event_type:
        return None
    part = projected.get("part")
    if isinstance(part, Mapping) and str(part.get("type") or "").lower() in {
        "reasoning",
        "reasoning_text",
        "thinking",
        "thinking_text",
    }:
        return None
    response = projected.get("response")
    if isinstance(response, Mapping):
        projected["response"] = project_response(
            provider, response, custom_names=custom_names
        )
    return projected
