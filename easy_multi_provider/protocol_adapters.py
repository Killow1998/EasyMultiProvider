"""Destination protocol adapters for Codex request and response boundaries."""

from __future__ import annotations

from dataclasses import dataclass
import json
from typing import Any, Dict, Mapping

from .dialects import (
    ANTHROPIC_MESSAGES,
    CHAT_COMPLETIONS,
    CODEX_NATIVE,
    PORTABLE_RESPONSES,
    ProjectionError,
    custom_tool_names,
    project_request,
    project_response,
)
from .protocol_projection import (
    _response_from_anthropic,
    _response_from_chat,
    responses_to_anthropic,
    responses_to_chat,
    validate_responses_body,
)
from .router_errors import (
    ExternalProtocolError,
    HistoryReconstructionError,
    RouterError,
)


def body_with_supported_effort(
    provider: Mapping[str, Any],
    body: Mapping[str, Any],
    model: Mapping[str, Any],
) -> Dict[str, Any]:
    """Remove only an explicitly unsupported external reasoning effort."""

    projected = dict(body)
    if provider.get("auth_mode") in {"account", "forward"}:
        return projected
    reasoning = projected.get("reasoning")
    if not isinstance(reasoning, Mapping) or "effort" not in reasoning:
        return projected
    levels = model.get("reasoning_levels")
    effort = reasoning.get("effort")
    if isinstance(levels, list) and effort in levels:
        return projected
    reasoning = dict(reasoning)
    reasoning.pop("effort", None)
    if reasoning:
        projected["reasoning"] = reasoning
    else:
        projected.pop("reasoning", None)
    return projected


def _request_projection_error(exc: ProjectionError) -> RouterError:
    if exc.failure_class in {"opaque_compaction", "invalid_compaction"}:
        return HistoryReconstructionError("history_projection_incomplete")
    return RouterError(str(exc), 422)


def _json_object(raw: bytes, invalid_message: str) -> Dict[str, Any]:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise RouterError(invalid_message, 502) from exc
    if not isinstance(value, dict):
        raise RouterError(invalid_message, 502)
    return value


@dataclass(frozen=True)
class ProtocolAdapter:
    dialect: str
    protocol: str
    native: bool = False
    replay_safe: bool = True

    def project_request(
        self,
        provider: Mapping[str, Any],
        body: Mapping[str, Any],
        model: Mapping[str, Any],
        upstream_model: str,
    ) -> Dict[str, Any]:
        raise NotImplementedError

    def normalize_response(
        self,
        provider: Mapping[str, Any],
        raw: bytes,
        requested_model: str,
        body: Mapping[str, Any],
        model: Mapping[str, Any],
    ) -> bytes:
        raise NotImplementedError


class _ResponsesAdapter(ProtocolAdapter):
    def project_request(
        self,
        provider: Mapping[str, Any],
        body: Mapping[str, Any],
        model: Mapping[str, Any],
        upstream_model: str,
    ) -> Dict[str, Any]:
        try:
            payload = project_request(
                provider,
                body_with_supported_effort(provider, body, model),
                preserve_reasoning_state=model.get(
                    "_emp_preserve_reasoning_state"
                )
                is True,
            )
        except ProjectionError as exc:
            raise _request_projection_error(exc) from exc
        payload["model"] = upstream_model
        return payload


class CodexNativeAdapter(_ResponsesAdapter):
    def __init__(self) -> None:
        super().__init__(CODEX_NATIVE, "responses", native=True, replay_safe=False)

    def normalize_response(
        self,
        provider: Mapping[str, Any],
        raw: bytes,
        requested_model: str,
        body: Mapping[str, Any],
        model: Mapping[str, Any],
    ) -> bytes:
        return raw


class PortableResponsesAdapter(_ResponsesAdapter):
    def __init__(self) -> None:
        super().__init__(PORTABLE_RESPONSES, "responses")

    def normalize_response(
        self,
        provider: Mapping[str, Any],
        raw: bytes,
        requested_model: str,
        body: Mapping[str, Any],
        model: Mapping[str, Any],
    ) -> bytes:
        value = _json_object(
            raw, "upstream Responses response is not valid JSON"
        )
        validate_responses_body(value)
        try:
            projected = project_response(
                provider,
                value,
                custom_names=custom_tool_names(body),
                preserve_reasoning_summary=model.get(
                    "_emp_preserve_reasoning_summary"
                )
                is True,
                preserve_reasoning_state=model.get(
                    "_emp_preserve_reasoning_state"
                )
                is True,
            )
        except ProjectionError as exc:
            message = (
                "external upstream returned an unexpected compaction item"
                if exc.failure_class == "external_compaction"
                else "external upstream response projection failed"
            )
            raise ExternalProtocolError(message) from exc
        validate_responses_body(projected)
        return json.dumps(
            projected, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")


class ChatCompletionsAdapter(ProtocolAdapter):
    def __init__(self) -> None:
        super().__init__(CHAT_COMPLETIONS, "chat_completions")

    def project_request(
        self,
        provider: Mapping[str, Any],
        body: Mapping[str, Any],
        model: Mapping[str, Any],
        upstream_model: str,
    ) -> Dict[str, Any]:
        return responses_to_chat(
            body_with_supported_effort(provider, body, model), upstream_model
        )

    def normalize_response(
        self,
        provider: Mapping[str, Any],
        raw: bytes,
        requested_model: str,
        body: Mapping[str, Any],
        model: Mapping[str, Any],
    ) -> bytes:
        value = _json_object(
            raw, "chat-completions upstream returned invalid JSON"
        )
        response = _response_from_chat(
            value, requested_model, custom_names=custom_tool_names(body)
        )
        return json.dumps(response).encode("utf-8")


class AnthropicMessagesAdapter(ProtocolAdapter):
    def __init__(self) -> None:
        super().__init__(ANTHROPIC_MESSAGES, "anthropic_messages")

    def project_request(
        self,
        provider: Mapping[str, Any],
        body: Mapping[str, Any],
        model: Mapping[str, Any],
        upstream_model: str,
    ) -> Dict[str, Any]:
        return responses_to_anthropic(body, upstream_model)

    def normalize_response(
        self,
        provider: Mapping[str, Any],
        raw: bytes,
        requested_model: str,
        body: Mapping[str, Any],
        model: Mapping[str, Any],
    ) -> bytes:
        value = _json_object(raw, "Anthropic upstream returned invalid JSON")
        response = _response_from_anthropic(
            value, requested_model, custom_names=custom_tool_names(body)
        )
        return json.dumps(response).encode("utf-8")


_ADAPTERS = {
    CODEX_NATIVE: CodexNativeAdapter(),
    PORTABLE_RESPONSES: PortableResponsesAdapter(),
    CHAT_COMPLETIONS: ChatCompletionsAdapter(),
    ANTHROPIC_MESSAGES: AnthropicMessagesAdapter(),
}


def protocol_adapter(dialect: str) -> ProtocolAdapter:
    try:
        return _ADAPTERS[dialect]
    except KeyError as exc:
        raise RouterError("provider protocol is unresolved", 502) from exc
