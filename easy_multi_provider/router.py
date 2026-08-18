"""Translate Codex Responses requests to configured upstream providers."""

from __future__ import annotations

import json
import re
import time
import uuid
from typing import Any, Dict, Iterable, Iterator, Tuple
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlparse
from urllib.request import Request, urlopen

from .accounts import AccountError, auth_headers
from .config import api_key
from .quota import QuotaError, read_account_quota


class RouterError(Exception):
    def __init__(self, message: str, status: int = 400):
        super().__init__(message)
        self.status = status


_GEMINI_REASONING_LEVELS = (
    ("gemini-3.7-flash", ["low", "medium", "high"]),
    ("gemini-3.6-flash", ["minimal", "low", "medium", "high"]),
    ("gemini-3.5-flash", ["minimal", "low", "medium", "high"]),
    ("gemini-3.5-flash-lite", ["minimal", "low", "medium", "high"]),
    ("gemini-3.1-pro", ["low", "medium", "high"]),
    ("gemini-3.1-flash-lite", ["minimal", "low", "medium", "high"]),
    ("gemini-3-flash", ["minimal", "low", "medium", "high"]),
    ("gemini-3-pro", ["low", "high"]),
    ("gemini-2.5", ["low", "medium", "high"]),
)


def _gemini_reasoning_levels(model: str, metadata: Dict[str, Any]) -> list:
    if metadata.get("thinking") is not True:
        return []
    for prefix, levels in _GEMINI_REASONING_LEVELS:
        if model == prefix or model.startswith(prefix + "-"):
            return list(levels)
    return []


def find_route(config: Dict[str, Any], model_id: str) -> Tuple[Dict[str, Any], Dict[str, Any]]:
    for model in config.get("models", []):
        if model.get("enabled", True) and model.get("id") == model_id:
            providers = {item["id"]: item for item in config.get("providers", [])}
            provider = providers.get(model.get("provider"))
            if provider and provider.get("enabled", True):
                return provider, model
            raise RouterError("provider for model is missing or disabled: %s" % model_id, 503)

    for account in config.get("accounts", []):
        prefix = str(account.get("prefix", "")) + "/"
        if account.get("enabled", True) and prefix != "/" and model_id.startswith(prefix):
            upstream_id = model_id[len(prefix):]
            if not upstream_id:
                break
            return {
                "id": account["id"],
                "name": account.get("name", account["id"]),
                "base_url": config.get("codex_base_url", "https://chatgpt.com/backend-api/codex"),
                "protocol": "responses",
                "auth_mode": "account",
                "account": account,
            }, {"id": model_id, "upstream_id": upstream_id}

    forward = [
        item
        for item in config.get("providers", [])
        if item.get("enabled", True) and item.get("auth_mode") == "forward"
    ]
    if len(forward) == 1:
        return forward[0], {"id": model_id, "upstream_id": model_id}
    raise RouterError("unknown model: %s" % model_id, 404)


def _upstream_model(provider: Dict[str, Any], model: Dict[str, Any], requested: str) -> str:
    explicit = model.get("upstream_id", "")
    if explicit:
        prefix = str(provider.get("id", "")) + "/"
        return explicit[len(prefix):] if prefix != "/" and explicit.startswith(prefix) else explicit
    prefix = provider.get("id", "") + "/"
    return requested[len(prefix):] if requested.startswith(prefix) else requested


def _endpoint(provider: Dict[str, Any]) -> str:
    base = provider["base_url"].rstrip("/")
    suffixes = {
        "responses": "/responses",
        "chat_completions": "/chat/completions",
        "anthropic_messages": "/messages",
    }
    suffix = suffixes[provider["protocol"]]
    return base if base.endswith(suffix) else base + suffix


def model_metadata(provider: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    """Read model limits where the upstream exposes a standard metadata API."""
    parsed = urlparse(provider.get("base_url", ""))
    if parsed.hostname != "generativelanguage.googleapis.com":
        raise RouterError("该 Provider 没有可自动读取的模型上限，请手动填写", 400)
    key = api_key(provider)
    if not key:
        raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
    base = provider["base_url"].rstrip("/")
    if base.endswith("/openai"):
        base = base[:-len("/openai")]
    request = Request(
        base + "/models/" + quote(upstream_model, safe=""),
        headers={"x-goog-api-key": key},
    )
    try:
        with urlopen(request, timeout=30) as response:
            value = json.loads(response.read().decode("utf-8"))
    except HTTPError as exc:
        detail = exc.read(4096).decode("utf-8", "replace")
        raise RouterError("上游模型信息查询失败 %d: %s" % (exc.code, detail), exc.code) from exc
    except (OSError, ValueError) as exc:
        raise RouterError("上游模型信息查询失败: %s" % exc, 502) from exc
    input_limit = value.get("inputTokenLimit")
    output_limit = value.get("outputTokenLimit")
    if (
        not isinstance(input_limit, int)
        or isinstance(input_limit, bool)
        or input_limit <= 0
        or not isinstance(output_limit, int)
        or isinstance(output_limit, bool)
        or output_limit <= 0
    ):
        raise RouterError("上游未返回有效的上下文上限，请手动填写", 502)
    return {
        "model": upstream_model,
        "context_window": input_limit,
        "input_token_limit": input_limit,
        "output_token_limit": output_limit,
        "reasoning_levels": _gemini_reasoning_levels(upstream_model, value),
    }


_DISCOVERABLE_MODEL_ID = re.compile(r"^[A-Za-z0-9._/-]+$")


def _get_json(request: Request, purpose: str) -> Dict[str, Any]:
    try:
        with urlopen(request, timeout=30) as response:
            value = json.loads(response.read().decode("utf-8"))
    except HTTPError as exc:
        detail = exc.read(4096).decode("utf-8", "replace")
        raise RouterError("上游%s失败 %d: %s" % (purpose, exc.code, detail), exc.code) from exc
    except (OSError, ValueError) as exc:
        raise RouterError("上游%s失败: %s" % (purpose, exc), 502) from exc
    if not isinstance(value, dict):
        raise RouterError("上游%s返回格式无效" % purpose, 502)
    return value


def _discovery_headers(provider: Dict[str, Any], native_gemini: bool) -> Dict[str, str]:
    if provider.get("auth_mode") != "api_key":
        raise RouterError("该 Provider 没有可自动读取模型列表的 API Key，请手动添加", 400)
    key = api_key(provider)
    if not key:
        raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
    headers = {"Accept": "application/json", "User-Agent": "EasyMultiProvider/0.1"}
    headers["x-goog-api-key" if native_gemini else "Authorization"] = (
        key if native_gemini else "Bearer " + key
    )
    return headers


def _positive_int(value: Any) -> int:
    return value if isinstance(value, int) and not isinstance(value, bool) and value > 0 else 0


def _model_id(value: Any) -> str:
    model_id = str(value or "").strip()
    if model_id.startswith("models/"):
        model_id = model_id[len("models/"):]
    return model_id if _DISCOVERABLE_MODEL_ID.fullmatch(model_id or "") else ""


def _gemini_models(provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    if base.endswith("/openai"):
        base = base[:-len("/openai")]
    headers = _discovery_headers(provider, True)
    result = []
    page_token = ""
    for _ in range(20):
        url = base + "/models"
        if page_token:
            url += "?pageToken=" + quote(page_token, safe="")
        value = _get_json(Request(url, headers=headers), "Gemini 模型列表查询")
        for item in value.get("models", []):
            if not isinstance(item, dict):
                continue
            methods = item.get("supportedGenerationMethods")
            if isinstance(methods, list) and methods and "generateContent" not in methods:
                continue
            model_id = _model_id(item.get("name"))
            if not model_id:
                continue
            result.append(
                {
                    "upstream_id": model_id,
                    "display_name": str(item.get("displayName") or model_id),
                    "description": str(item.get("description") or ""),
                    "context_window": _positive_int(item.get("inputTokenLimit")),
                    "input_token_limit": _positive_int(item.get("inputTokenLimit")),
                    "output_token_limit": _positive_int(item.get("outputTokenLimit")),
                    "reasoning_levels": _gemini_reasoning_levels(model_id, item) or ["medium"],
                }
            )
        page_token = str(value.get("nextPageToken") or "")
        if not page_token:
            break
    return result


def _generic_models(provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    for suffix in ("/chat/completions", "/responses"):
        if base.endswith(suffix):
            base = base[:-len(suffix)]
            break
    value = _get_json(
        Request(base + "/models", headers=_discovery_headers(provider, False)),
        "模型列表查询",
    )
    result = []
    for item in value.get("data", []):
        if not isinstance(item, dict):
            continue
        model_id = _model_id(item.get("id"))
        if not model_id:
            continue
        context = _positive_int(
            item.get("context_window")
            or item.get("context_length")
            or item.get("inputTokenLimit")
        )
        result.append(
            {
                "upstream_id": model_id,
                "display_name": str(item.get("display_name") or item.get("name") or model_id),
                "description": str(item.get("description") or ""),
                "context_window": context,
                "reasoning_levels": ["medium"],
            }
        )
    return result


def discover_models(provider: Dict[str, Any]) -> list:
    """Fetch safe model metadata without making one request per model."""
    if provider.get("protocol") == "anthropic_messages":
        raise RouterError("Anthropic Messages 没有统一的模型列表接口，请手动添加", 400)
    parsed = urlparse(provider.get("base_url", ""))
    if parsed.hostname == "generativelanguage.googleapis.com":
        return _gemini_models(provider)
    return _generic_models(provider)


def _headers(provider: Dict[str, Any], incoming: Dict[str, str], stream: bool) -> Dict[str, str]:
    headers = {
        "Content-Type": "application/json",
        "Accept": "text/event-stream" if stream else "application/json",
        "User-Agent": "EasyMultiProvider/0.1",
    }
    if provider.get("auth_mode") == "api_key":
        key = api_key(provider)
        if not key:
            raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
        headers["Authorization"] = "Bearer " + key
    elif provider.get("auth_mode") == "anthropic_api_key":
        key = api_key(provider)
        if not key:
            raise RouterError("API key is not configured for provider: %s" % provider["id"], 503)
        headers["x-api-key"] = key
        headers["anthropic-version"] = provider.get("anthropic_version", "2023-06-01")
    elif provider.get("auth_mode") == "account":
        try:
            headers.update(auth_headers(provider["account"]))
        except AccountError as exc:
            raise RouterError(str(exc), 503)
    else:
        # Only forward the headers needed by a local Codex-to-Codex pass-through.
        allowed = {
            "authorization": "Authorization",
            "chatgpt-account-id": "chatgpt-account-id",
            "openai-beta": "OpenAI-Beta",
            "originator": "originator",
        }
        lower = {key.lower(): value for key, value in incoming.items()}
        for source, target in allowed.items():
            if lower.get(source):
                headers[target] = lower[source]
        if "Authorization" not in headers:
            raise RouterError("forward provider requires an incoming Authorization header", 401)
    return headers


def _request(
    provider: Dict[str, Any],
    payload: Dict[str, Any],
    incoming: Dict[str, str],
    stream: bool = False,
):
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    for attempt in range(2):
        request = Request(
            _endpoint(provider),
            data=data,
            headers=_headers(provider, incoming, stream),
            method="POST",
        )
        try:
            return urlopen(request, timeout=180)
        except HTTPError as exc:
            if attempt == 0 and exc.code == 401 and provider.get("auth_mode") == "account":
                try:
                    read_account_quota(provider["account"])
                except (OSError, QuotaError, ValueError):
                    pass
                else:
                    continue
            if attempt == 0 and exc.code in (429, 500, 502, 503, 504):
                exc.read(4096)
                time.sleep(0.5)
                continue
            detail = exc.read(4096).decode("utf-8", "replace")
            raise RouterError("upstream returned %d: %s" % (exc.code, detail), exc.code)
        except URLError as exc:
            raise RouterError("upstream connection failed: %s" % exc.reason, 502)
    raise RouterError("upstream request failed", 502)


def _content_text(content: Any) -> str:
    if isinstance(content, str):
        return content
    if not isinstance(content, list):
        return ""
    parts = []
    for item in content:
        if isinstance(item, str):
            parts.append(item)
        elif isinstance(item, dict) and item.get("type") in ("input_text", "text"):
            parts.append(str(item.get("text", "")))
    return "".join(parts)


def _messages(body: Dict[str, Any]) -> list:
    messages = []
    instructions = body.get("instructions")
    if instructions:
        messages.append({"role": "system", "content": _content_text(instructions)})
    source = body.get("input", "")
    if isinstance(source, str):
        messages.append({"role": "user", "content": source})
        return messages
    if isinstance(source, dict):
        source = [source]
    if not isinstance(source, list):
        return messages
    for item in source:
        if not isinstance(item, dict):
            continue
        item_type = item.get("type", "message")
        if item_type == "message":
            role = item.get("role", "user")
            messages.append({"role": role, "content": _content_text(item.get("content", ""))})
        elif item_type == "function_call_output":
            messages.append(
                {
                    "role": "tool",
                    "tool_call_id": item.get("call_id", ""),
                    "content": _content_text(item.get("output", "")),
                }
            )
    return messages


def _tools(body: Dict[str, Any]) -> list:
    result = []
    for item in body.get("tools", []) or []:
        if not isinstance(item, dict) or item.get("type") != "function":
            continue
        function = item.get("function", item)
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


def responses_to_chat(body: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    payload = {"model": upstream_model, "messages": _messages(body), "stream": bool(body.get("stream"))}
    tools = _tools(body)
    if tools:
        payload["tools"] = tools
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
        return []
    result = []
    for item in content:
        if isinstance(item, str):
            result.append({"type": "text", "text": item})
        elif isinstance(item, dict) and item.get("type") in ("input_text", "output_text", "text"):
            result.append({"type": "text", "text": str(item.get("text", ""))})
    return result


def _anthropic_messages(body: Dict[str, Any]) -> list:
    source = body.get("input", "")
    if isinstance(source, str):
        source = [{"type": "message", "role": "user", "content": source}]
    elif isinstance(source, dict):
        source = [source]
    if not isinstance(source, list):
        source = []
    messages = []
    for item in source:
        if not isinstance(item, dict):
            continue
        item_type = item.get("type", "message")
        if item_type == "message":
            role = item.get("role", "user")
            if role not in ("user", "assistant"):
                role = "user"
            content = _anthropic_content(item.get("content", ""))
            if content:
                messages.append({"role": role, "content": content})
        elif item_type == "function_call":
            arguments = item.get("arguments", "{}")
            if isinstance(arguments, str):
                try:
                    arguments = json.loads(arguments)
                except ValueError:
                    arguments = {}
            messages.append(
                {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": item.get("call_id") or item.get("id", "tool_call"),
                        "name": item.get("name", "tool"),
                        "input": arguments if isinstance(arguments, dict) else {},
                    }],
                }
            )
        elif item_type == "function_call_output":
            messages.append(
                {
                    "role": "user",
                    "content": [{
                        "type": "tool_result",
                        "tool_use_id": item.get("call_id", ""),
                        "content": _content_text(item.get("output", "")),
                    }],
                }
            )
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


def responses_to_anthropic(body: Dict[str, Any], upstream_model: str) -> Dict[str, Any]:
    payload = {
        "model": upstream_model,
        "max_tokens": int(body.get("max_output_tokens", 4096) or 4096),
        "messages": _anthropic_messages(body),
        "stream": bool(body.get("stream")),
    }
    instructions = body.get("instructions")
    if instructions:
        payload["system"] = _content_text(instructions)
    tools = _anthropic_tools(body)
    if tools:
        payload["tools"] = tools
    for source, target in (("temperature", "temperature"), ("top_p", "top_p")):
        if source in body:
            payload[target] = body[source]
    if "stop" in body:
        payload["stop_sequences"] = body["stop"] if isinstance(body["stop"], list) else [body["stop"]]
    return payload


def _response_from_chat(value: Dict[str, Any], requested_model: str) -> Dict[str, Any]:
    choices = value.get("choices") or [{}]
    message = choices[0].get("message") or {}
    output = []
    text = message.get("content") or ""
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
    for call in message.get("tool_calls", []) or []:
        function = call.get("function", {})
        output.append(
            {
                "id": call.get("id", "fc_" + uuid.uuid4().hex),
                "type": "function_call",
                "status": "completed",
                "call_id": call.get("id", ""),
                "name": function.get("name", ""),
                "arguments": function.get("arguments", "{}"),
            }
        )
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "completed",
        "model": requested_model,
        "output": output,
        "output_text": text,
    }
    if value.get("usage") is not None:
        response["usage"] = value["usage"]
    return response


def _response_from_anthropic(value: Dict[str, Any], requested_model: str) -> Dict[str, Any]:
    output = []
    text = []
    for block in value.get("content", []) or []:
        if not isinstance(block, dict):
            continue
        if block.get("type") == "text":
            text.append(str(block.get("text", "")))
        elif block.get("type") == "tool_use":
            output.append(
                {
                    "id": block.get("id", "fc_" + uuid.uuid4().hex),
                    "type": "function_call",
                    "status": "completed",
                    "call_id": block.get("id", ""),
                    "name": block.get("name", ""),
                    "arguments": json.dumps(block.get("input", {}), ensure_ascii=False),
                }
            )
    final_text = "".join(text)
    if final_text:
        output.insert(
            0,
            {
                "id": "msg_" + uuid.uuid4().hex,
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": final_text, "annotations": []}],
            },
        )
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "completed",
        "model": requested_model,
        "output": output,
        "output_text": final_text,
    }
    usage = value.get("usage")
    if isinstance(usage, dict):
        response["usage"] = {
            "input_tokens": usage.get("input_tokens", 0),
            "output_tokens": usage.get("output_tokens", 0),
            "total_tokens": usage.get("input_tokens", 0) + usage.get("output_tokens", 0),
        }
    return response


def forward_responses(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    payload = dict(body)
    payload["model"] = _upstream_model(provider, model, body["model"])
    with _request(provider, payload, incoming, bool(body.get("stream"))) as response:
        return response.status, response.headers.get("Content-Type", "application/json"), response.read()


def forward_responses_stream(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
):
    payload = dict(body)
    payload["model"] = _upstream_model(provider, model, body["model"])
    return _request(provider, payload, incoming, True)


def chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    payload = responses_to_chat(body, _upstream_model(provider, model, body["model"]))
    with _request(provider, payload, incoming, False) as response:
        raw = response.read()
    try:
        value = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise RouterError("chat-completions upstream returned invalid JSON", 502)
    return 200, "application/json", json.dumps(_response_from_chat(value, body["model"])).encode("utf-8")


def anthropic_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    payload = responses_to_anthropic(body, _upstream_model(provider, model, body["model"]))
    with _request(provider, payload, incoming, False) as response:
        raw = response.read()
    try:
        value = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise RouterError("Anthropic upstream returned invalid JSON", 502)
    return 200, "application/json", json.dumps(_response_from_anthropic(value, body["model"])).encode("utf-8")


def _sse_frame(event: str, value: Dict[str, Any]) -> bytes:
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(value, ensure_ascii=False))).encode("utf-8")


def _sse_data(response: Any) -> Iterator[str]:
    pending = []
    try:
        for raw in response:
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if line.startswith("data:"):
                pending.append(line[5:].lstrip())
            elif not line and pending:
                yield "\n".join(pending)
                pending = []
        if pending:
            yield "\n".join(pending)
    finally:
        response.close()


def stream_chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Iterable[bytes]:
    payload = responses_to_chat(body, _upstream_model(provider, model, body["model"]))
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    text = []
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
        upstream = _request(provider, payload, incoming, True)
        yield frame("response.created", {"type": "response.created", "response": response})
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
        for data in _sse_data(upstream):
            if data == "[DONE]":
                break
            try:
                chunk = json.loads(data)
            except ValueError:
                continue
            choices = chunk.get("choices") or []
            delta = choices[0].get("delta", {}) if choices else {}
            piece = delta.get("content") or ""
            if piece:
                text.append(piece)
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
        final_text = "".join(text)
        output = {
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": final_text, "annotations": []}],
        }
        response["status"] = "completed"
        response["output"] = [output]
        response["output_text"] = final_text
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
        yield frame("response.completed", {"type": "response.completed", "response": response})
    except RouterError as exc:
        yield frame("error", {"type": "error", "message": str(exc), "code": exc.status})


def stream_anthropic_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Iterable[bytes]:
    payload = responses_to_anthropic(body, _upstream_model(provider, model, body["model"]))
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    text = []
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
        upstream = _request(provider, payload, incoming, True)
        yield frame("response.created", {"type": "response.created", "response": response})
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
        for data in _sse_data(upstream):
            try:
                event = json.loads(data)
            except ValueError:
                continue
            delta = event.get("delta") or {}
            if delta.get("type") != "text_delta":
                continue
            piece = delta.get("text") or ""
            if piece:
                text.append(piece)
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
        final_text = "".join(text)
        output = {
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": final_text, "annotations": []}],
        }
        response["status"] = "completed"
        response["output"] = [output]
        response["output_text"] = final_text
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
        yield frame("response.output_item.done", {"output_index": 0, "item": output})
        yield frame("response.completed", {"type": "response.completed", "response": response})
    except RouterError as exc:
        yield frame("error", {"type": "error", "message": str(exc), "code": exc.status})


def proxy(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[Dict[str, Any], Any]:
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    if body.get("stream") and provider["protocol"] == "chat_completions":
        return {"kind": "stream", "content_type": "text/event-stream"}, stream_chat_completion(
            provider, body, model, incoming
        )
    if body.get("stream") and provider["protocol"] == "anthropic_messages":
        return {"kind": "stream", "content_type": "text/event-stream"}, stream_anthropic_completion(
            provider, body, model, incoming
        )
    if provider["protocol"] == "responses":
        if body.get("stream"):
            upstream = forward_responses_stream(provider, body, model, incoming)
            return {
                "kind": "raw_stream",
                "status": upstream.status,
                "content_type": upstream.headers.get("Content-Type", "text/event-stream"),
            }, upstream
        status, content_type, raw = forward_responses(provider, body, model, incoming)
    elif provider["protocol"] == "chat_completions":
        status, content_type, raw = chat_completion(provider, body, model, incoming)
    else:
        status, content_type, raw = anthropic_completion(provider, body, model, incoming)
    return {"kind": "body", "status": status, "content_type": content_type}, raw
