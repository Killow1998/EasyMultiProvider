"""Translate Codex Responses requests to configured upstream providers."""

from __future__ import annotations

import base64
import json
import re
import time
import uuid
from typing import Any, Dict, Iterable, Iterator, Tuple
from urllib.error import HTTPError, URLError
from urllib.parse import quote, urlparse
from urllib.request import HTTPRedirectHandler, Request, build_opener

from . import __version__
from .accounts import AccountError, auth_headers
from .config import MAX_CONTEXT_WINDOW, api_key
from .quota import QuotaError, refresh_account_quota


class _NoRedirectHandler(HTTPRedirectHandler):
    def redirect_request(self, request, fp, code, msg, headers, newurl):
        raise URLError("upstream redirects are disabled")


# Credential-bearing requests must never replay headers to a redirected URL.
def urlopen(request: Request, timeout: float):
    """Build after startup so automatically imported system proxies are used."""
    return build_opener(_NoRedirectHandler()).open(request, timeout=timeout)


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

MAX_UPSTREAM_BODY_BYTES = 16 * 1024 * 1024
MAX_UPSTREAM_ERROR_BYTES = 4096
MAX_UPSTREAM_ERROR_TEXT_CHARS = 512
MAX_DISCOVERY_BODY_BYTES = 4 * 1024 * 1024
MAX_DISCOVERY_TOTAL_BYTES = 8 * 1024 * 1024
MAX_DISCOVERY_SECONDS = 60
MAX_DISCOVERY_FIELD_BYTES = 4096
MAX_DISCOVERY_TOKEN_BYTES = 4096
MAX_SSE_FRAME_BYTES = 1024 * 1024
MAX_STREAM_TEXT_BYTES = 16 * 1024 * 1024
MAX_DISCOVERED_MODELS = 1000
MAX_UPSTREAM_SECONDS = 180
UPSTREAM_SOCKET_TIMEOUT = 30
_TEXTUAL_PROTOCOL_MARKERS = (
    "<think>",
    "</think>",
    "<tool_call>",
    "</tool_call>",
    "<|tool_call|>",
    "<|tool_calls|>",
)
_COMPACTION_PROMPT = """You are performing a CONTEXT CHECKPOINT COMPACTION. Create a handoff summary for another language model that will resume the task.

Include current progress, key decisions, constraints, user preferences, remaining steps, and critical data or references. Be concise, structured, and focused on seamless continuation."""
_COMPACTION_SUMMARY_PREFIX = (
    "Another language model started this task and produced a continuation summary. "
    "Use it to continue without repeating completed work:"
)
_COMPACTION_PREFIX = "emp1:"


def _http_error_detail(exc: HTTPError) -> Tuple[str, str]:
    """Return a bounded diagnostic without echoing an upstream HTML page."""
    headers = getattr(exc, "headers", {}) or {}
    content_type = str(headers.get("Content-Type", "")).split(";", 1)[0].strip().lower()
    raw = exc.read(MAX_UPSTREAM_ERROR_BYTES)
    decoded = raw.decode("utf-8", "replace")
    if content_type in ("text/html", "application/xhtml+xml") or re.match(
        r"\s*(?:<!doctype\s+html|<html\b)", decoded, re.I
    ):
        return (
            content_type or "text/html",
            "HTML error page omitted; the upstream gateway or WAF may have rejected the request",
        )

    detail = decoded
    if "json" in content_type or decoded.lstrip().startswith(("{", "[")):
        try:
            value = json.loads(decoded)
        except ValueError:
            pass
        else:
            if isinstance(value, dict):
                detail = _upstream_error_text(value.get("error") or value.get("message") or value)
            else:
                detail = str(value)
    detail = re.sub(r"\s+", " ", detail).strip()
    if not detail:
        detail = str(getattr(exc, "reason", "") or "no response detail")
    if len(detail) > MAX_UPSTREAM_ERROR_TEXT_CHARS:
        detail = detail[: MAX_UPSTREAM_ERROR_TEXT_CHARS - 3].rstrip() + "..."
    return content_type or "unknown content type", detail


def _http_error_message(prefix: str, exc: HTTPError) -> str:
    content_type, detail = _http_error_detail(exc)
    return "%s %d (%s): %s" % (prefix, exc.code, content_type, detail)


def _set_response_timeout(response: Any, timeout: float) -> None:
    for candidate in (
        response,
        getattr(response, "fp", None),
        getattr(getattr(response, "fp", None), "raw", None),
    ):
        sock = getattr(candidate, "_sock", None)
        if sock is not None and hasattr(sock, "settimeout"):
            sock.settimeout(max(0.1, timeout))
            return


def _read_limited(
    response: Any, limit: int, purpose: str, deadline: float = None
) -> bytes:
    response_headers = getattr(response, "headers", {})
    content_length = response_headers.get("Content-Length") if response_headers else None
    if content_length:
        try:
            if int(content_length) < 0 or int(content_length) > limit:
                raise RouterError("upstream %s is too large" % purpose, 502)
        except ValueError:
            raise RouterError("upstream %s has invalid Content-Length" % purpose, 502)
    chunks = []
    total = 0
    while True:
        if deadline is not None:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                response.close()
                raise RouterError("upstream %s timed out" % purpose, 504)
            _set_response_timeout(response, min(UPSTREAM_SOCKET_TIMEOUT, remaining))
        try:
            chunk = response.read(64 * 1024)
        except TypeError:
            # Small test doubles may expose read() without a size argument.
            chunk = response.read()
            if len(chunk) > limit:
                raise RouterError("upstream %s is too large" % purpose, 502)
            return chunk
        except OSError as exc:
            if deadline is not None and time.monotonic() >= deadline:
                response.close()
                raise RouterError("upstream %s timed out" % purpose, 504) from exc
            raise
        if not chunk:
            break
        total += len(chunk)
        if total > limit:
            raise RouterError("upstream %s is too large" % purpose, 502)
        chunks.append(chunk)
    return b"".join(chunks)


class _DeadlineResponse:
    def __init__(self, response: Any, deadline: float):
        self._response = response
        self._deadline = deadline
        self._unsized_read = False

    @property
    def status(self):
        return self._response.status

    @property
    def headers(self):
        return self._response.headers

    def _check_deadline(self) -> None:
        if time.monotonic() > self._deadline:
            self.close()
            raise RouterError("upstream request timed out", 504)

    def read(self, size: int = -1) -> bytes:
        self._check_deadline()
        if self._unsized_read:
            return b""
        _set_response_timeout(
            self._response,
            self._deadline - time.monotonic(),
        )
        try:
            return self._response.read(size)
        except TypeError:
            self._unsized_read = True
            return self._response.read()
        except TimeoutError as exc:
            self.close()
            raise RouterError("upstream request timed out", 504) from exc
        except OSError as exc:
            if time.monotonic() >= self._deadline:
                self.close()
                raise RouterError("upstream request timed out", 504) from exc
            raise

    def __iter__(self):
        return self

    def __next__(self):
        self._check_deadline()
        _set_response_timeout(
            self._response,
            self._deadline - time.monotonic(),
        )
        try:
            return next(self._response)
        except TimeoutError as exc:
            self.close()
            raise RouterError("upstream request timed out", 504) from exc
        except OSError as exc:
            if time.monotonic() >= self._deadline:
                self.close()
                raise RouterError("upstream request timed out", 504) from exc
            raise

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()

    def close(self) -> None:
        close = getattr(self._response, "close", None)
        if close:
            close()


class _LimitedResponse:
    def __init__(self, response: Any, limit: int):
        self._response = response
        self._limit = limit
        self._read = 0

    @property
    def status(self):
        return self._response.status

    @property
    def headers(self):
        return self._response.headers

    def read(self, size: int = -1) -> bytes:
        if self._read >= self._limit:
            return b""
        remaining = self._limit - self._read
        requested = remaining if size is None or size < 0 else min(size, remaining)
        try:
            chunk = self._response.read(requested)
        except TypeError:
            chunk = self._response.read()
        if not chunk:
            return b""
        self._read += len(chunk)
        if self._read > self._limit:
            raise RouterError("upstream stream is too large", 502)
        return chunk

    def __iter__(self):
        for chunk in self._response:
            if not chunk:
                return
            self._read += len(chunk)
            if self._read > self._limit:
                raise RouterError("upstream stream is too large", 502)
            yield chunk

    def close(self) -> None:
        self._response.close()


def _bounded_stream_response(response: Any) -> _LimitedResponse:
    headers = getattr(response, "headers", {})
    content_length = headers.get("Content-Length") if headers else None
    if content_length:
        try:
            if int(content_length) < 0 or int(content_length) > MAX_UPSTREAM_BODY_BYTES:
                response.close()
                raise RouterError("upstream stream is too large", 502)
        except ValueError:
            response.close()
            raise RouterError("upstream stream has invalid Content-Length", 502)
    return _LimitedResponse(response, MAX_UPSTREAM_BODY_BYTES)


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


def _endpoint(provider: Dict[str, Any], operation: str = "") -> str:
    provider = resolve_provider_protocol(provider)
    base = provider["base_url"].rstrip("/")
    if operation == "responses_compact":
        if provider["protocol"] != "responses":
            raise RouterError("remote compact passthrough requires the Responses protocol")
        if base.endswith("/responses/compact"):
            return base
        if base.endswith("/responses"):
            return base + "/compact"
        return base + "/responses/compact"
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
            value = json.loads(
                _read_limited(response, MAX_DISCOVERY_BODY_BYTES, "模型信息响应").decode("utf-8")
            )
    except HTTPError as exc:
        raise RouterError(_http_error_message("上游模型信息查询失败", exc), exc.code) from exc
    except (OSError, ValueError) as exc:
        raise RouterError("上游模型信息查询失败: %s" % exc, 502) from exc
    input_limit = value.get("inputTokenLimit")
    output_limit = value.get("outputTokenLimit")
    if (
        not _positive_int(input_limit)
        or not _positive_int(output_limit)
    ):
        raise RouterError("上游未返回有效的上下文上限，请手动填写", 502)
    return {
        "model": upstream_model,
        "context_window": _positive_int(input_limit),
        "input_token_limit": _positive_int(input_limit),
        "output_token_limit": _positive_int(output_limit),
        "reasoning_levels": _gemini_reasoning_levels(upstream_model, value),
    }


_DISCOVERABLE_MODEL_ID = re.compile(r"^[A-Za-z0-9._/-]+$")


def _get_json(request: Request, purpose: str, budget: Dict[str, Any] = None) -> Dict[str, Any]:
    if budget and time.monotonic() > budget["deadline"]:
        raise RouterError("上游%s超时" % purpose, 504)
    try:
        with urlopen(request, timeout=30) as response:
            raw = _read_limited(
                response,
                MAX_DISCOVERY_BODY_BYTES,
                purpose,
                budget["deadline"] if budget else None,
            )
            if budget:
                budget["bytes"] += len(raw)
                if budget["bytes"] > MAX_DISCOVERY_TOTAL_BYTES:
                    raise RouterError("上游%s超过安全总大小" % purpose, 502)
            value = json.loads(raw.decode("utf-8"))
    except HTTPError as exc:
        raise RouterError(_http_error_message("上游%s失败" % purpose, exc), exc.code) from exc
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
    headers = {
        "Accept": "application/json",
        "User-Agent": "EasyMultiProvider/%s" % __version__,
    }
    headers["x-goog-api-key" if native_gemini else "Authorization"] = (
        key if native_gemini else "Bearer " + key
    )
    return headers


def _positive_int(value: Any) -> int:
    return (
        value
        if isinstance(value, int)
        and not isinstance(value, bool)
        and 0 < value <= MAX_CONTEXT_WINDOW
        else 0
    )


def _model_id(value: Any) -> str:
    model_id = str(value or "").strip()
    if len(model_id.encode("utf-8")) > MAX_DISCOVERY_FIELD_BYTES:
        return ""
    if model_id.startswith("models/"):
        model_id = model_id[len("models/"):]
    return model_id if _DISCOVERABLE_MODEL_ID.fullmatch(model_id or "") else ""


def _model_text(value: Any, fallback: str, field: str) -> str:
    text = str(value or fallback)
    if len(text.encode("utf-8")) > MAX_DISCOVERY_FIELD_BYTES:
        raise RouterError("上游模型%s过大" % field, 502)
    return text


def _gemini_models(provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    if base.endswith("/openai"):
        base = base[:-len("/openai")]
    headers = _discovery_headers(provider, True)
    result = []
    page_token = ""
    budget = {"bytes": 0, "deadline": time.monotonic() + MAX_DISCOVERY_SECONDS}
    for _ in range(20):
        url = base + "/models"
        if page_token:
            url += "?pageToken=" + quote(page_token, safe="")
        value = _get_json(Request(url, headers=headers), "Gemini 模型列表查询", budget)
        for item in value.get("models", []):
            if len(result) >= MAX_DISCOVERED_MODELS:
                raise RouterError("上游模型列表超过安全上限", 502)
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
                    "display_name": _model_text(item.get("displayName"), model_id, "显示名称"),
                    "description": _model_text(item.get("description"), "", "描述"),
                    "context_window": _positive_int(item.get("inputTokenLimit")),
                    "input_token_limit": _positive_int(item.get("inputTokenLimit")),
                    "output_token_limit": _positive_int(item.get("outputTokenLimit")),
                    "reasoning_levels": _gemini_reasoning_levels(model_id, item) or ["medium"],
                    "created_at": _positive_int(
                        item.get("created") or item.get("created_at") or item.get("updated_at")
                    ),
                }
            )
        page_token = value.get("nextPageToken") or ""
        if not isinstance(page_token, str) or len(page_token.encode("utf-8")) > MAX_DISCOVERY_TOKEN_BYTES:
            raise RouterError("上游分页标记过大", 502)
        if not page_token:
            break
    return result


def _generic_models(provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    for suffix in ("/chat/completions", "/responses"):
        if base.endswith(suffix):
            base = base[:-len(suffix)]
            break
    budget = {"bytes": 0, "deadline": time.monotonic() + MAX_DISCOVERY_SECONDS}
    value = _get_json(
        Request(base + "/models", headers=_discovery_headers(provider, False)),
        "模型列表查询",
        budget,
    )
    result = []
    for item in value.get("data", []):
        if len(result) >= MAX_DISCOVERED_MODELS:
            raise RouterError("上游模型列表超过安全上限", 502)
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
                "display_name": _model_text(
                    item.get("display_name") or item.get("name"), model_id, "显示名称"
                ),
                "description": _model_text(item.get("description"), "", "描述"),
                "context_window": context,
                "reasoning_levels": ["medium"],
                "created_at": _positive_int(
                    item.get("created") or item.get("created_at") or item.get("updated_at")
                ),
            }
        )
    return result


def resolve_provider_protocol(
    provider: Dict[str, Any], preferred: str = "responses"
) -> Dict[str, Any]:
    """Return an explicit protocol candidate without mutating saved config."""
    if provider.get("protocol") != "auto":
        return provider
    resolved = dict(provider)
    resolved["protocol"] = (
        "anthropic_messages"
        if provider.get("auth_mode") == "anthropic_api_key"
        else preferred
    )
    return resolved


def discover_models(provider: Dict[str, Any]) -> list:
    """Fetch safe model metadata without making one request per model."""
    if provider.get("protocol") == "anthropic_messages" or (
        provider.get("protocol") == "auto"
        and provider.get("auth_mode") == "anthropic_api_key"
    ):
        raise RouterError("Anthropic Messages 没有统一的模型列表接口，请手动添加", 400)
    parsed = urlparse(provider.get("base_url", ""))
    if parsed.hostname == "generativelanguage.googleapis.com":
        return _gemini_models(provider)
    return _generic_models(provider)


def _headers(provider: Dict[str, Any], incoming: Dict[str, str], stream: bool) -> Dict[str, str]:
    headers = {
        "Content-Type": "application/json",
        "Accept": "text/event-stream" if stream else "application/json",
        "User-Agent": "EasyMultiProvider/%s" % __version__,
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
    operation: str = "",
):
    data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
    deadline = time.monotonic() + MAX_UPSTREAM_SECONDS
    for attempt in range(2):
        remaining = deadline - time.monotonic()
        if remaining <= 0:
            raise RouterError("upstream request timed out", 504)
        request = Request(
            _endpoint(provider, operation),
            data=data,
            headers=_headers(provider, incoming, stream),
            method="POST",
        )
        try:
            response = urlopen(request, timeout=min(UPSTREAM_SOCKET_TIMEOUT, remaining))
            return (
                _DeadlineResponse(response, deadline)
                if hasattr(response, "close")
                else response
            )
        except HTTPError as exc:
            if attempt == 0 and exc.code == 401 and provider.get("auth_mode") == "account":
                try:
                    refresh_account_quota(provider["account"])
                except (OSError, QuotaError, ValueError):
                    pass
                else:
                    continue
            if attempt == 0 and exc.code in (429, 500, 502, 503, 504):
                exc.read(4096)
                time.sleep(0.5)
                continue
            content_type, detail = _http_error_detail(exc)
            if (
                attempt == 0
                and exc.code == 400
                and "reasoning_effort" in payload
                and "reasoning_effort" in detail
            ):
                payload = dict(payload)
                payload.pop("reasoning_effort", None)
                data = json.dumps(payload, ensure_ascii=False).encode("utf-8")
                continue
            raise RouterError(
                "upstream returned %d (%s): %s" % (exc.code, content_type, detail),
                exc.code,
            )
        except URLError as exc:
            if attempt == 0:
                time.sleep(0.5)
                continue
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
        elif isinstance(item, dict) and item.get("type") in ("input_text", "output_text", "text"):
            parts.append(str(item.get("text", "")))
    return "".join(parts)


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
    source: Any, drop_trigger: bool = False, opaque_placeholder: bool = False
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
            if opaque_placeholder:
                result.append(
                    _message_item("[Earlier conversation history was compacted by another provider.]")
                )
                continue
        result.append(item)
    return result


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
    source = _normalize_compaction_input(source, opaque_placeholder=True)
    for item in source:
        if not isinstance(item, dict):
            continue
        item_type = item.get("type", "message")
        if item_type == "message":
            role = item.get("role", "user")
            messages.append({"role": role, "content": _content_text(item.get("content", ""))})
        elif item_type == "function_call":
            arguments = item.get("arguments", "{}")
            if not isinstance(arguments, str):
                arguments = json.dumps(arguments, ensure_ascii=False)
            call_id = item.get("call_id") or item.get("id") or "tool_call"
            messages.append(
                {
                    "role": "assistant",
                    "content": None,
                    "tool_calls": [
                        {
                            "id": call_id,
                            "type": "function",
                            "function": {
                                "name": item.get("name", "tool"),
                                "arguments": arguments,
                            },
                        }
                    ],
                }
            )
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


def responses_to_chat(
    body: Dict[str, Any], upstream_model: str, provider: Dict[str, Any] = None
) -> Dict[str, Any]:
    payload = {"model": upstream_model, "messages": _messages(body), "stream": bool(body.get("stream"))}
    tools = [] if provider and provider.get("tool_call_mode") == "disabled" else _tools(body)
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
    source = _normalize_compaction_input(source, opaque_placeholder=True)
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


def responses_to_anthropic(
    body: Dict[str, Any], upstream_model: str, provider: Dict[str, Any] = None
) -> Dict[str, Any]:
    payload = {
        "model": upstream_model,
        "max_tokens": int(body.get("max_output_tokens", 4096) or 4096),
        "messages": _anthropic_messages(body),
        "stream": bool(body.get("stream")),
    }
    instructions = body.get("instructions")
    if instructions:
        payload["system"] = _content_text(instructions)
    tools = [] if provider and provider.get("tool_call_mode") == "disabled" else _anthropic_tools(body)
    if tools:
        payload["tools"] = tools
    for source, target in (("temperature", "temperature"), ("top_p", "top_p")):
        if source in body:
            payload[target] = body[source]
    if "stop" in body:
        payload["stop_sequences"] = body["stop"] if isinstance(body["stop"], list) else [body["stop"]]
    return payload


def _response_from_chat(value: Dict[str, Any], requested_model: str) -> Dict[str, Any]:
    error = value.get("error")
    if error:
        raise RouterError("Chat Completions upstream error: %s" % _upstream_error_text(error), 502)
    choices = value.get("choices") or [{}]
    message = choices[0].get("message") or {}
    output = []
    text = message.get("content") or ""
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


def _upstream_error_text(error: Any) -> str:
    if isinstance(error, dict):
        return str(error.get("message") or error.get("type") or error.get("code") or error)
    return str(error)


def _validate_textual_protocol(value: Any) -> None:
    text = str(value or "")
    if any(marker in text for marker in _TEXTUAL_PROTOCOL_MARKERS):
        raise RouterError(
            "upstream returned textual reasoning/tool-call markup; "
            "use structured tool calls or set this Provider to text-only mode",
            502,
        )


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
    payload = _responses_payload(provider, body, model)
    with _request(provider, payload, incoming, bool(body.get("stream"))) as response:
        return (
            response.status,
            response.headers.get("Content-Type", "application/json"),
            _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses 响应"),
        )


def forward_responses_stream(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
):
    payload = _responses_payload(provider, body, model)
    upstream = _bounded_stream_response(_request(provider, payload, incoming, True))
    return _validated_responses_stream(upstream)


def forward_responses_compact(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    payload = _responses_payload(provider, body, model)
    with _request(provider, payload, incoming, False, "responses_compact") as response:
        return (
            response.status,
            response.headers.get("Content-Type", "application/json"),
            _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses compact response"),
        )


def _responses_payload(
    provider: Dict[str, Any], body: Dict[str, Any], model: Dict[str, Any]
) -> Dict[str, Any]:
    payload = dict(body)
    # Codex client telemetry is not model input. Keep it on native account
    # passthroughs, but do not disclose it to unrelated API-key Providers.
    if provider.get("auth_mode") == "api_key":
        payload.pop("client_metadata", None)
    payload["model"] = _upstream_model(provider, model, body["model"])
    payload["input"] = _normalize_compaction_input(payload.get("input"))
    return payload


def chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[int, str, bytes]:
    payload = responses_to_chat(
        body, _upstream_model(provider, model, body["model"]), provider
    )
    with _request(provider, payload, incoming, False) as response:
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Chat Completions 响应")
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
    payload = responses_to_anthropic(
        body, _upstream_model(provider, model, body["model"]), provider
    )
    with _request(provider, payload, incoming, False) as response:
        raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Anthropic 响应")
    try:
        value = json.loads(raw.decode("utf-8"))
    except ValueError:
        raise RouterError("Anthropic upstream returned invalid JSON", 502)
    return 200, "application/json", json.dumps(_response_from_anthropic(value, body["model"])).encode("utf-8")


def _response_text(raw: bytes) -> str:
    try:
        value = json.loads(raw.decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise RouterError("compaction upstream returned invalid JSON", 502) from exc
    text = value.get("output_text") if isinstance(value, dict) else None
    if isinstance(text, str) and text.strip():
        return text.strip()
    parts = []
    for item in value.get("output", []) if isinstance(value, dict) else []:
        if not isinstance(item, dict):
            continue
        for part in item.get("content", []) or []:
            if isinstance(part, dict) and part.get("type") in ("output_text", "text"):
                parts.append(str(part.get("text", "")))
    summary = "\n".join(parts).strip()
    if not summary:
        raise RouterError("compaction upstream returned an empty summary", 502)
    return summary


def _summary_body(body: Dict[str, Any]) -> Dict[str, Any]:
    source = body.get("input", [])
    if isinstance(source, str):
        source = [_message_item(source)]
    elif isinstance(source, dict):
        source = [source]
    elif not isinstance(source, list):
        source = []
    payload = dict(body)
    payload["input"] = list(_normalize_compaction_input(source, drop_trigger=True)) + [
        _message_item(_COMPACTION_PROMPT)
    ]
    payload["stream"] = False
    payload["tools"] = []
    payload.pop("previous_response_id", None)
    return payload


def _summarize(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> str:
    payload = _summary_body(body)
    if provider["protocol"] == "responses":
        _, _, raw = forward_responses(provider, payload, model, incoming)
    elif provider["protocol"] == "chat_completions":
        _, _, raw = chat_completion(provider, payload, model, incoming)
    else:
        _, _, raw = anthropic_completion(provider, payload, model, incoming)
    return _response_text(raw)


def _retained_user_messages(source: Any, budget: int = 80_000) -> list:
    if not isinstance(source, list):
        return []
    messages = []
    for item in source:
        if not isinstance(item, dict) or item.get("type", "message") != "message":
            continue
        if item.get("role") != "user":
            continue
        text = _content_text(item.get("content", ""))
        if text.strip():
            messages.append(text)
    retained = []
    for text in reversed(messages):
        if budget <= 0:
            break
        retained.append(text[-budget:])
        budget -= len(retained[-1])
    return [_message_item(text) for text in reversed(retained)]


def _compaction_response(model: str, summary: str) -> Dict[str, Any]:
    item = {
        "id": "cmp_" + uuid.uuid4().hex,
        "type": "compaction",
        "encrypted_content": _encode_compaction(summary),
    }
    return {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "created_at": int(time.time()),
        "status": "completed",
        "model": model,
        "output": [item],
        "usage": None,
    }


def _is_compaction_trigger(body: Dict[str, Any]) -> bool:
    source = body.get("input")
    return bool(
        isinstance(source, list)
        and source
        and isinstance(source[-1], dict)
        and source[-1].get("type") == "compaction_trigger"
    )


def _sse_frame(event: str, value: Dict[str, Any]) -> bytes:
    return ("event: %s\ndata: %s\n\n" % (event, json.dumps(value, ensure_ascii=False))).encode("utf-8")


def _response_failure_frame(message: str, status: int = 502) -> bytes:
    display_message = "HTTP %d: %s" % (status, message)
    response = {
        "id": "resp_" + uuid.uuid4().hex,
        "object": "response",
        "status": "failed",
        "error": {"code": "upstream_error", "message": display_message},
    }
    return _sse_frame("response.failed", {"type": "response.failed", "response": response})


def _sse_data(response: Any) -> Iterator[str]:
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
            raw_body.extend(raw)
            line = raw.decode("utf-8", "replace").rstrip("\r\n")
            if line.startswith("data:"):
                saw_sse_data = True
                value = line[5:].lstrip()
                pending_bytes += len(value.encode("utf-8"))
                if pending_bytes > MAX_SSE_FRAME_BYTES:
                    raise RouterError("upstream SSE frame is too large", 502)
                pending.append(value)
            elif not line and pending:
                yield "\n".join(pending)
                pending = []
                pending_bytes = 0
        if pending:
            yield "\n".join(pending)
        if not saw_sse_data and raw_body:
            body = bytes(raw_body).decode("utf-8", "replace").strip()
            try:
                json.loads(body)
            except ValueError:
                raise RouterError("upstream returned neither SSE nor JSON data", 502)
            yield body
    finally:
        response.close()


def _response_json_stream(value: Dict[str, Any]) -> Iterator[bytes]:
    """Convert one complete Responses JSON body to a finite SSE stream."""
    if not isinstance(value, dict):
        raise RouterError("upstream Responses JSON is not an object", 502)
    if value.get("error"):
        raise RouterError("upstream Responses error: %s" % _upstream_error_text(value["error"]), 502)
    output = value.get("output")
    if not isinstance(output, list):
        raise RouterError("upstream Responses JSON has no valid output", 502)
    response = dict(value)
    response.setdefault("id", "resp_" + uuid.uuid4().hex)
    response.setdefault("object", "response")
    response["status"] = "completed"
    output = [dict(item) for item in output if isinstance(item, dict)]
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
    yield _sse_frame("response.completed", {"type": "response.completed", "response": response})


def _validated_responses_stream(response: Any) -> Iterator[bytes]:
    """Pass through valid Responses SSE and terminate malformed streams explicitly."""
    try:
        content_type = str(getattr(response, "headers", {}).get("Content-Type", "")).lower()
        chunks = response
        if "text/event-stream" not in content_type:
            raw = _read_limited(response, MAX_UPSTREAM_BODY_BYTES, "Responses stream")
            stripped = raw.lstrip()
            if stripped.startswith((b"event:", b"data:", b":")):
                chunks = (raw,)
            else:
                try:
                    value = json.loads(raw.decode("utf-8", "replace"))
                except (UnicodeDecodeError, ValueError):
                    raise RouterError("upstream Responses stream was neither SSE nor valid JSON", 502)
                yield from _response_json_stream(value)
                return

        line_buffer = ""
        pending_data = []
        saw_data = False
        saw_terminal = False
        saw_error = False
        error_message = ""

        def consume_line(line: str) -> None:
            nonlocal saw_data, saw_terminal, saw_error, error_message, pending_data
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
            event_type = str(value.get("type") or "")
            if event_type == "response.completed":
                saw_terminal = True
            elif event_type in ("error", "response.failed"):
                saw_error = True
                error_message = str(value.get("message") or value.get("error") or "upstream Responses stream returned an error")

        for raw in chunks:
            raw_bytes = raw if isinstance(raw, bytes) else str(raw).encode("utf-8")
            text = raw_bytes.decode("utf-8", "replace")
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
            yield _response_failure_frame(message)
        elif not saw_terminal and saw_error:
            yield _response_failure_frame(error_message or "upstream Responses stream returned an error")
    except RouterError as exc:
        yield _response_failure_frame(str(exc), exc.status)
    finally:
        response.close()


def stream_chat_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Iterable[bytes]:
    payload = responses_to_chat(
        body, _upstream_model(provider, model, body["model"]), provider
    )
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    text = []
    text_bytes = 0
    protocol_probe = ""
    tool_calls = {}
    saw_output = False
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
            if not isinstance(chunk, dict):
                continue
            if chunk.get("error"):
                raise RouterError(
                    "Chat Completions upstream error: %s" % _upstream_error_text(chunk["error"]),
                    502,
                )
            choices = chunk.get("choices") or []
            choice = choices[0] if choices and isinstance(choices[0], dict) else {}
            delta = choice.get("delta") or {}
            if not delta and isinstance(choice.get("message"), dict):
                # Some OpenAI-compatible gateways ignore stream=true and return
                # one ordinary Chat Completions response instead of SSE deltas.
                delta = choice["message"]
            piece = delta.get("content") or ""
            if piece:
                saw_output = True
                piece = piece if isinstance(piece, str) else _content_text(piece)
                protocol_probe = (protocol_probe + piece)[-128:]
                _validate_textual_protocol(protocol_probe)
                piece_bytes = len(str(piece).encode("utf-8"))
                if text_bytes + piece_bytes > MAX_STREAM_TEXT_BYTES:
                    raise RouterError("upstream streamed text is too large", 502)
                text_bytes += piece_bytes
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
            for raw_call in delta.get("tool_calls", []) or []:
                if not isinstance(raw_call, dict):
                    continue
                raw_index = raw_call.get("index", len(tool_calls))
                try:
                    index = int(raw_index)
                except (TypeError, ValueError):
                    index = len(tool_calls)
                function = raw_call.get("function") or {}
                if not isinstance(function, dict):
                    function = {}
                state = tool_calls.get(index)
                if state is None:
                    saw_output = True
                    call_id = str(raw_call.get("id") or "call_" + uuid.uuid4().hex)
                    state = {
                        "id": call_id,
                        "call_id": call_id,
                        "name": str(function.get("name") or ""),
                        "arguments": "",
                    }
                    tool_calls[index] = state
                    yield frame(
                        "response.output_item.added",
                        {
                            "type": "response.output_item.added",
                            "output_index": index + 1,
                            "item": {
                                "id": state["id"],
                                "type": "function_call",
                                "status": "in_progress",
                                "call_id": state["call_id"],
                                "name": state["name"],
                                "arguments": "",
                            },
                        },
                    )
                elif raw_call.get("id"):
                    state["call_id"] = str(raw_call["id"])
                    state["id"] = state["call_id"]
                if function.get("name"):
                    state["name"] += str(function["name"])
                arguments = function.get("arguments") or ""
                if arguments:
                    saw_output = True
                    arguments = str(arguments)
                    state["arguments"] += arguments
                    yield frame(
                        "response.function_call_arguments.delta",
                        {
                            "type": "response.function_call_arguments.delta",
                            "item_id": state["id"],
                            "output_index": index + 1,
                            "delta": arguments,
                        },
                    )
        if not saw_output:
            raise RouterError("upstream returned an empty Chat Completions response", 502)
        final_text = "".join(text)
        output = {
            "id": message_id,
            "type": "message",
            "status": "completed",
            "role": "assistant",
            "content": [{"type": "output_text", "text": final_text, "annotations": []}],
        }
        function_outputs = []
        for index in sorted(tool_calls):
            state = tool_calls[index]
            function_output = {
                "id": state["id"],
                "type": "function_call",
                "status": "completed",
                "call_id": state["call_id"],
                "name": state["name"],
                "arguments": state["arguments"],
            }
            function_outputs.append(function_output)
            yield frame(
                "response.function_call_arguments.done",
                {
                    "type": "response.function_call_arguments.done",
                    "item_id": state["id"],
                    "output_index": index + 1,
                    "arguments": state["arguments"],
                },
            )
            yield frame(
                "response.output_item.done",
                {"output_index": index + 1, "item": function_output},
            )
        response["status"] = "completed"
        response["output"] = [output] + function_outputs
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
        yield _response_failure_frame(str(exc), exc.status)


def stream_anthropic_completion(
    provider: Dict[str, Any],
    body: Dict[str, Any],
    model: Dict[str, Any],
    incoming: Dict[str, str],
) -> Iterable[bytes]:
    payload = responses_to_anthropic(
        body, _upstream_model(provider, model, body["model"]), provider
    )
    payload["stream"] = True
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    text = []
    text_bytes = 0
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
                piece_bytes = len(str(piece).encode("utf-8"))
                if text_bytes + piece_bytes > MAX_STREAM_TEXT_BYTES:
                    raise RouterError("upstream streamed text is too large", 502)
                text_bytes += piece_bytes
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
        yield _response_failure_frame(str(exc), exc.status)


_AUTO_PROTOCOL_REJECTION_STATUSES = {404, 405, 415, 501}


def _auto_protocol_candidates(provider: Dict[str, Any]) -> Tuple[str, ...]:
    if provider.get("auth_mode") == "anthropic_api_key":
        return ("anthropic_messages",)
    base = str(provider.get("base_url", "")).rstrip("/")
    if base.endswith("/chat/completions"):
        return ("chat_completions", "responses")
    return ("responses", "chat_completions")


def _tag_route(
    metadata: Dict[str, Any], provider: Dict[str, Any]
) -> Dict[str, Any]:
    tagged = dict(metadata)
    tagged["provider_id"] = provider.get("id")
    tagged["resolved_protocol"] = provider.get("protocol")
    return tagged


def _proxy_resolved(
    provider: Dict[str, Any],
    model: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[Dict[str, Any], Any]:
    if _is_compaction_trigger(body) and provider["protocol"] != "responses":
        response = _compaction_response(
            body["model"], _summarize(provider, body, model, incoming)
        )
        if body.get("stream"):
            return _tag_route(
                {
                    "kind": "stream",
                    "status": 200,
                    "content_type": "text/event-stream",
                },
                provider,
            ), _response_json_stream(response)
        return _tag_route(
            {
                "kind": "body",
                "status": 200,
                "content_type": "application/json",
            },
            provider,
        ), json.dumps(response, ensure_ascii=False).encode("utf-8")
    if body.get("stream") and provider["protocol"] == "chat_completions":
        return _tag_route(
            {"kind": "stream", "content_type": "text/event-stream"}, provider
        ), stream_chat_completion(provider, body, model, incoming)
    if body.get("stream") and provider["protocol"] == "anthropic_messages":
        return _tag_route(
            {"kind": "stream", "content_type": "text/event-stream"}, provider
        ), stream_anthropic_completion(provider, body, model, incoming)
    if provider["protocol"] == "responses":
        if body.get("stream"):
            upstream = forward_responses_stream(provider, body, model, incoming)
            return _tag_route(
                {
                    "kind": "stream",
                    "status": 200,
                    "content_type": "text/event-stream",
                },
                provider,
            ), upstream
        status, content_type, raw = forward_responses(provider, body, model, incoming)
    elif provider["protocol"] == "chat_completions":
        status, content_type, raw = chat_completion(provider, body, model, incoming)
    else:
        status, content_type, raw = anthropic_completion(provider, body, model, incoming)
    return _tag_route(
        {"kind": "body", "status": status, "content_type": content_type}, provider
    ), raw


def proxy(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[Dict[str, Any], Any]:
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    if provider.get("protocol") != "auto":
        return _proxy_resolved(provider, model, body, incoming)

    candidates = _auto_protocol_candidates(provider)
    for index, protocol in enumerate(candidates):
        resolved = dict(provider)
        resolved["protocol"] = protocol
        try:
            return _proxy_resolved(resolved, model, body, incoming)
        except RouterError as exc:
            can_fallback = (
                index + 1 < len(candidates)
                and exc.status in _AUTO_PROTOCOL_REJECTION_STATUSES
            )
            if not can_fallback:
                raise
    raise RouterError("unable to resolve provider protocol", 502)


def proxy_compact(
    config: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[Dict[str, Any], bytes]:
    model_id = body.get("model")
    if not isinstance(model_id, str) or not model_id:
        raise RouterError("request.model is required")
    provider, model = find_route(config, model_id)
    if provider.get("protocol") == "auto":
        candidates = _auto_protocol_candidates(provider)
        for index, protocol in enumerate(candidates):
            resolved = dict(provider)
            resolved["protocol"] = protocol
            try:
                return _proxy_compact_resolved(resolved, model, body, incoming)
            except RouterError as exc:
                if (
                    index + 1 == len(candidates)
                    or exc.status not in _AUTO_PROTOCOL_REJECTION_STATUSES
                ):
                    raise
        raise RouterError("unable to resolve provider protocol", 502)
    return _proxy_compact_resolved(provider, model, body, incoming)


def _proxy_compact_resolved(
    provider: Dict[str, Any],
    model: Dict[str, Any],
    body: Dict[str, Any],
    incoming: Dict[str, str],
) -> Tuple[Dict[str, Any], bytes]:
    if provider["protocol"] == "responses":
        status, content_type, raw = forward_responses_compact(
            provider, body, model, incoming
        )
    else:
        summary = _summarize(provider, body, model, incoming)
        output = _retained_user_messages(body.get("input"))
        output.append(
            _message_item(_COMPACTION_SUMMARY_PREFIX + "\n\n" + summary)
        )
        status, content_type = 200, "application/json"
        raw = json.dumps({"output": output}, ensure_ascii=False).encode("utf-8")
    return _tag_route(
        {"kind": "body", "status": status, "content_type": content_type}, provider
    ), raw
