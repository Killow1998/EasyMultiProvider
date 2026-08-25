"""Non-recursive destination-model adapter for history compaction."""

from __future__ import annotations

import copy
import json
from collections.abc import Mapping
from typing import Any, Dict

from .history_compaction import SummaryRequest
from .protocol_projection import validate_responses_body
from .router import anthropic_completion, chat_completion, forward_responses
from .router_errors import ExternalProtocolError


class DestinationSummaryAdapter:
    """Run one bounded summary request without entering the main Router again."""

    def __call__(self, request: SummaryRequest) -> Dict[str, Any]:
        provider = copy.deepcopy(dict(request.provider))
        model = copy.deepcopy(dict(request.model))
        body = copy.deepcopy(dict(request.body))
        provider["protocol"] = request.protocol

        # Compaction is an internal, deterministic model call. It must not
        # expose Codex tools, invoke search, stream partial output, inherit an
        # opaque response chain, or trigger another history preparation pass.
        body["stream"] = False
        body["tools"] = []
        body.pop("previous_response_id", None)
        body.pop("tool_choice", None)
        incoming: Dict[str, str] = {}

        if request.protocol == "responses":
            _, _, raw = forward_responses(
                provider,
                body,
                model,
                incoming,
                allow_retries=False,
            )
        elif request.protocol == "chat_completions":
            _, _, raw = chat_completion(
                provider,
                body,
                model,
                incoming,
                allow_retries=False,
            )
        elif request.protocol == "anthropic_messages":
            _, _, raw = anthropic_completion(
                provider,
                body,
                model,
                incoming,
                allow_retries=False,
            )
        else:
            raise ExternalProtocolError("destination summary protocol is unsupported")

        try:
            value = json.loads(raw.decode("utf-8", "strict"))
        except (UnicodeDecodeError, ValueError):
            raise ExternalProtocolError(
                "destination summary returned invalid JSON"
            ) from None
        if not isinstance(value, Mapping):
            raise ExternalProtocolError(
                "destination summary must be a Responses object"
            )
        result = validate_responses_body(dict(value))
        if result.get("status") != "completed":
            raise ExternalProtocolError("destination summary did not complete")
        return result


__all__ = ["DestinationSummaryAdapter"]
