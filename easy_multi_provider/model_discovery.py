"""Bounded model discovery and advertised-capability parsing."""

from __future__ import annotations

from dataclasses import dataclass
from datetime import datetime
import json
import math
import re
import time
from typing import Any, Callable, Dict, Mapping, Optional, Tuple
from urllib.error import HTTPError
from urllib.parse import quote, urlparse
from urllib.request import Request

from .capabilities import (
    input_modalities_metadata_source,
    normalize_input_modalities,
    normalize_output_modalities,
    normalize_reasoning_levels,
    output_modalities_metadata_source,
)
from .config import MAX_CONTEXT_WINDOW, api_key
from .official_registry import enrich_discovered_models
from .router_errors import RouterError


MAX_DISCOVERY_BODY_BYTES = 4 * 1024 * 1024
MAX_DISCOVERY_TOTAL_BYTES = 8 * 1024 * 1024
MAX_DISCOVERY_SECONDS = 60
MAX_DISCOVERY_FIELD_BYTES = 4096
MAX_DISCOVERY_TOKEN_BYTES = 4096
MAX_DISCOVERED_MODELS = 1000
MAX_MODEL_TIMESTAMP = 4_102_444_800  # 2100-01-01 UTC
_DISCOVERABLE_MODEL_ID = re.compile(r"^[A-Za-z0-9._/:-]+$")
_REASONING_LEVEL_ID = re.compile(r"^[A-Za-z0-9_.-]{1,64}$")


@dataclass(frozen=True)
class DiscoveryIO:
    """Injected bounded network primitives owned by the router transport."""

    open_url: Callable[..., Any]
    read_limited: Callable[..., bytes]
    http_error_message: Callable[[str, HTTPError], str]
    discovery_headers: Callable[[Dict[str, Any], bool], Dict[str, str]]
    anthropic_headers: Callable[[Dict[str, Any]], Dict[str, str]]


def advertised_reasoning(
    metadata: Mapping[str, Any],
) -> Tuple[Optional[bool], list]:
    """Read advertised reasoning support without fabricating effort levels."""

    parameters = metadata.get("supported_parameters")
    parameter_support = bool(
        isinstance(parameters, list)
        and any(
            isinstance(item, str)
            and item.strip().lower()
            in {"reasoning", "reasoning_effort", "thinking"}
            for item in parameters
        )
    )
    nested = metadata.get("reasoning")
    nested = nested if isinstance(nested, Mapping) else {}
    raw_support = metadata.get(
        "supports_reasoning",
        metadata.get("reasoning_supported", nested.get("supported")),
    )
    if isinstance(raw_support, bool):
        support: Optional[bool] = raw_support
    elif metadata.get("thinking") is True or parameter_support:
        support = True
    else:
        support = None
    raw_levels = metadata.get(
        "reasoning_levels",
        metadata.get(
            "supported_reasoning_levels",
            nested.get("effort_levels", nested.get("supported_efforts")),
        ),
    )
    levels = []
    if isinstance(raw_levels, list) and len(raw_levels) <= 16:
        for raw in raw_levels:
            value = str(raw or "").strip()
            if not _REASONING_LEVEL_ID.fullmatch(value):
                levels = []
                break
            if value not in levels:
                levels.append(value)
    if levels:
        support = True
    return support, normalize_reasoning_levels(levels)


def advertised_reasoning_summaries(metadata: Mapping[str, Any]) -> Optional[bool]:
    """Read only an explicit structured-summary contract."""

    parameters = metadata.get("supported_parameters")
    parameter_support = bool(
        isinstance(parameters, list)
        and any(
            isinstance(item, str)
            and item.strip().lower()
            in {
                "reasoning_summary",
                "reasoning.summary",
                "reasoning_summary_text",
            }
            for item in parameters
        )
    )
    nested = metadata.get("reasoning")
    nested = nested if isinstance(nested, Mapping) else {}
    raw = metadata.get(
        "supports_reasoning_summaries",
        metadata.get(
            "supports_reasoning_summary_parameter",
            nested.get("supports_summary", nested.get("summary_supported")),
        ),
    )
    if isinstance(raw, bool):
        return raw
    return True if parameter_support else None


def discovery_headers(
    provider: Dict[str, Any], native_gemini: bool, version: str
) -> Dict[str, str]:
    if provider.get("auth_mode") != "api_key":
        raise RouterError(
            "该 Provider 没有可自动读取模型列表的 API Key，请手动添加", 400
        )
    key = api_key(provider)
    if not key:
        raise RouterError(
            "API key is not configured for provider: %s" % provider["id"], 503
        )
    headers = {
        "Accept": "application/json",
        "User-Agent": "EMP/%s" % version,
    }
    headers["x-goog-api-key" if native_gemini else "Authorization"] = (
        key if native_gemini else "Bearer " + key
    )
    return headers


def anthropic_discovery_headers(
    provider: Dict[str, Any], version: str
) -> Dict[str, str]:
    key = api_key(provider)
    if not key:
        raise RouterError(
            "API key is not configured for provider: %s" % provider["id"], 503
        )
    return {
        "Accept": "application/json",
        "User-Agent": "EMP/%s" % version,
        "x-api-key": key,
        "anthropic-version": provider.get("anthropic_version", "2023-06-01"),
    }


def positive_int(value: Any) -> int:
    return (
        value
        if isinstance(value, int)
        and not isinstance(value, bool)
        and 0 < value <= MAX_CONTEXT_WINDOW
        else 0
    )


def created_timestamp(value: Any) -> int:
    """Normalize advertised Unix seconds/milliseconds or ISO-8601 timestamps."""

    if isinstance(value, str):
        text = value.strip()
        if not text:
            return 0
        try:
            number = float(text)
        except ValueError:
            try:
                number = datetime.fromisoformat(
                    text.replace("Z", "+00:00")
                ).timestamp()
            except ValueError:
                return 0
    elif isinstance(value, (int, float)) and not isinstance(value, bool):
        number = float(value)
    else:
        return 0
    if not math.isfinite(number):
        return 0
    if number > 100_000_000_000:
        number /= 1000.0
    timestamp = int(number)
    return timestamp if 0 < timestamp <= MAX_MODEL_TIMESTAMP else 0


def _model_id(value: Any) -> str:
    model_id = str(value or "").strip()
    if len(model_id.encode("utf-8")) > MAX_DISCOVERY_FIELD_BYTES:
        return ""
    if model_id.startswith("models/"):
        model_id = model_id[len("models/") :]
    return model_id if _DISCOVERABLE_MODEL_ID.fullmatch(model_id or "") else ""


def _model_text(value: Any, fallback: str, field: str) -> str:
    text = str(value or fallback)
    if len(text.encode("utf-8")) > MAX_DISCOVERY_FIELD_BYTES:
        raise RouterError("上游模型%s过大" % field, 502)
    return text


def _nested_supported(container: Mapping[str, Any], field: str) -> Optional[bool]:
    value = container.get(field)
    if isinstance(value, Mapping):
        supported = value.get("supported")
        return supported if isinstance(supported, bool) else None
    return None


def _get_json(
    io: DiscoveryIO,
    request: Request,
    purpose: str,
    budget: Optional[Dict[str, Any]] = None,
) -> Dict[str, Any]:
    if budget and time.monotonic() > budget["deadline"]:
        raise RouterError("上游%s超时" % purpose, 504)
    try:
        with io.open_url(request, timeout=30) as response:
            raw = io.read_limited(
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
        raise RouterError(
            io.http_error_message("上游%s失败" % purpose, exc), exc.code
        ) from exc
    except (OSError, ValueError) as exc:
        raise RouterError("上游%s失败: %s" % (purpose, exc), 502) from exc
    if not isinstance(value, dict):
        raise RouterError("上游%s返回格式无效" % purpose, 502)
    return value


def model_metadata(
    io: DiscoveryIO, provider: Dict[str, Any], upstream_model: str
) -> Dict[str, Any]:
    """Read model limits where the upstream exposes a standard metadata API."""

    parsed = urlparse(provider.get("base_url", ""))
    if parsed.hostname != "generativelanguage.googleapis.com":
        raise RouterError("该 Provider 没有可自动读取的模型上限，请手动填写", 400)
    key = api_key(provider)
    if not key:
        raise RouterError(
            "API key is not configured for provider: %s" % provider["id"], 503
        )
    base = provider["base_url"].rstrip("/")
    if base.endswith("/openai"):
        base = base[: -len("/openai")]
    request = Request(
        base + "/models/" + quote(upstream_model, safe=""),
        headers={"x-goog-api-key": key},
    )
    try:
        with io.open_url(request, timeout=30) as response:
            value = json.loads(
                io.read_limited(
                    response, MAX_DISCOVERY_BODY_BYTES, "模型信息响应"
                ).decode("utf-8")
            )
    except HTTPError as exc:
        raise RouterError(
            io.http_error_message("上游模型信息查询失败", exc), exc.code
        ) from exc
    except (OSError, ValueError) as exc:
        raise RouterError("上游模型信息查询失败: %s" % exc, 502) from exc
    input_limit = value.get("inputTokenLimit")
    output_limit = value.get("outputTokenLimit")
    if not positive_int(input_limit) or not positive_int(output_limit):
        raise RouterError("上游未返回有效的上下文上限，请手动填写", 502)
    supports_reasoning, reasoning_levels = advertised_reasoning(value)
    supports_summaries = advertised_reasoning_summaries(value)
    return {
        "model": upstream_model,
        "context_window": positive_int(input_limit),
        "input_token_limit": positive_int(input_limit),
        "output_token_limit": positive_int(output_limit),
        "supports_reasoning": supports_reasoning,
        "supports_reasoning_summaries": supports_summaries,
        "reasoning_levels": reasoning_levels,
    }


def _gemini_models(io: DiscoveryIO, provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    if base.endswith("/openai"):
        base = base[: -len("/openai")]
    headers = io.discovery_headers(provider, True)
    result = []
    page_token = ""
    budget = {"bytes": 0, "deadline": time.monotonic() + MAX_DISCOVERY_SECONDS}
    for _ in range(20):
        url = base + "/models"
        if page_token:
            url += "?pageToken=" + quote(page_token, safe="")
        value = _get_json(
            io, Request(url, headers=headers), "Gemini 模型列表查询", budget
        )
        for item in value.get("models", []):
            if len(result) >= MAX_DISCOVERED_MODELS:
                raise RouterError("上游模型列表超过安全上限", 502)
            if not isinstance(item, dict):
                continue
            methods = item.get("supportedGenerationMethods")
            if (
                isinstance(methods, list)
                and methods
                and "generateContent" not in methods
            ):
                continue
            model_id = _model_id(item.get("name"))
            if not model_id:
                continue
            supports_reasoning, reasoning_levels = advertised_reasoning(item)
            supports_summaries = advertised_reasoning_summaries(item)
            raw_input = item.get("inputModalities")
            if raw_input is None:
                raw_input = item.get("supportedInputModalities")
            raw_output = item.get("outputModalities")
            if raw_output is None:
                raw_output = item.get("supportedOutputModalities")
            input_limit = positive_int(item.get("inputTokenLimit"))
            output_limit = positive_int(item.get("outputTokenLimit"))
            result.append(
                {
                    "upstream_id": model_id,
                    "display_name": _model_text(
                        item.get("displayName"), model_id, "显示名称"
                    ),
                    "description": _model_text(item.get("description"), "", "描述"),
                    "context_window": input_limit,
                    "max_input_tokens": input_limit,
                    "output_limit": output_limit,
                    "supports_reasoning": supports_reasoning,
                    "supports_reasoning_summaries": supports_summaries,
                    "reasoning_levels": reasoning_levels,
                    "input_modalities": normalize_input_modalities(raw_input),
                    "output_modalities": normalize_output_modalities(raw_output),
                    "supports_image_detail_original": False,
                    "capability_sources": {
                        "supports_reasoning": {
                            "source": "advertised"
                            if supports_reasoning is not None
                            else "unknown"
                        },
                        "supports_reasoning_summaries": {
                            "source": "advertised"
                            if supports_summaries is not None
                            else "unknown"
                        },
                        "reasoning_levels": {
                            "source": "advertised" if reasoning_levels else "unknown"
                        },
                        "input_modalities": {
                            "source": input_modalities_metadata_source(raw_input)
                        },
                        "output_modalities": {
                            "source": output_modalities_metadata_source(raw_output)
                        },
                        "supports_image_detail_original": {"source": "unknown"},
                        "context_window": {
                            "source": "advertised" if input_limit else "unknown"
                        },
                        "max_input_tokens": {
                            "source": "advertised" if input_limit else "unknown"
                        },
                        "output_limit": {
                            "source": "advertised" if output_limit else "unknown"
                        },
                    },
                    "created_at": created_timestamp(
                        item.get("created")
                        or item.get("created_at")
                        or item.get("updated_at")
                    ),
                }
            )
        page_token = value.get("nextPageToken") or ""
        if (
            not isinstance(page_token, str)
            or len(page_token.encode("utf-8")) > MAX_DISCOVERY_TOKEN_BYTES
        ):
            raise RouterError("上游分页标记过大", 502)
        if not page_token:
            break
    return enrich_discovered_models(provider, result)


def _generic_models(io: DiscoveryIO, provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    for suffix in ("/chat/completions", "/responses"):
        if base.endswith(suffix):
            base = base[: -len(suffix)]
            break
    budget = {"bytes": 0, "deadline": time.monotonic() + MAX_DISCOVERY_SECONDS}
    value = _get_json(
        io,
        Request(base + "/models", headers=io.discovery_headers(provider, False)),
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
        context = positive_int(
            item.get("context_window")
            or item.get("context_length")
            or item.get("inputTokenLimit")
        )
        architecture = item.get("architecture")
        architecture = architecture if isinstance(architecture, dict) else {}
        raw_input = architecture.get("input_modalities")
        raw_output = architecture.get("output_modalities")
        raw_image_detail = item.get(
            "supports_image_detail_original",
            architecture.get("supports_image_detail_original"),
        )
        supports_image_detail = (
            raw_image_detail if isinstance(raw_image_detail, bool) else False
        )
        supports_reasoning, reasoning_levels = advertised_reasoning(item)
        supports_summaries = advertised_reasoning_summaries(item)
        parameters = item.get("supported_parameters")
        parameter_set = {
            value.strip().lower()
            for value in parameters
            if isinstance(value, str)
        } if isinstance(parameters, list) else set()
        capabilities = {}
        capability_sources = {}
        if "tools" in parameter_set:
            capabilities["structured_tools"] = True
            capability_sources["structured_tools"] = {"source": "advertised"}
        if "parallel_tool_calls" in parameter_set:
            capabilities["parallel_tools"] = True
            capability_sources["parallel_tools"] = {"source": "advertised"}
        if "structured_outputs" in parameter_set or "response_format" in parameter_set:
            capabilities["structured_output"] = True
            capability_sources["structured_output"] = {"source": "advertised"}
        raw_streaming = item.get("streaming")
        if isinstance(raw_streaming, bool):
            capabilities["streaming"] = raw_streaming
            capability_sources["streaming"] = {"source": "advertised"}
        output_limit = 0
        for output_key in ("output_limit", "max_tokens", "max_output_tokens"):
            output_limit = positive_int(item.get(output_key))
            if output_limit:
                break
        if not output_limit and isinstance(item.get("top_provider"), dict):
            output_limit = positive_int(
                item["top_provider"].get("max_completion_tokens")
            )
        max_input = positive_int(item.get("max_input_tokens"))
        entry = {
            "upstream_id": model_id,
            "display_name": _model_text(
                item.get("display_name") or item.get("name"), model_id, "显示名称"
            ),
            "description": _model_text(item.get("description"), "", "描述"),
            "context_window": context,
            "max_input_tokens": max_input,
            "output_limit": output_limit,
            "supports_reasoning": supports_reasoning,
            "supports_reasoning_summaries": supports_summaries,
            "reasoning_levels": reasoning_levels,
            "input_modalities": normalize_input_modalities(raw_input),
            "output_modalities": normalize_output_modalities(raw_output),
            "supports_image_detail_original": supports_image_detail,
            "capability_sources": {
                "supports_reasoning": {
                    "source": "advertised"
                    if supports_reasoning is not None
                    else "unknown"
                },
                "supports_reasoning_summaries": {
                    "source": "advertised"
                    if supports_summaries is not None
                    else "unknown"
                },
                "reasoning_levels": {
                    "source": "advertised" if reasoning_levels else "unknown"
                },
                "input_modalities": {
                    "source": input_modalities_metadata_source(raw_input)
                },
                "output_modalities": {
                    "source": output_modalities_metadata_source(raw_output)
                },
                "supports_image_detail_original": {
                    "source": "advertised"
                    if isinstance(raw_image_detail, bool)
                    else "unknown"
                },
                "context_window": {
                    "source": "advertised" if context else "unknown"
                },
                "max_input_tokens": {
                    "source": "advertised" if max_input else "unknown"
                },
                "output_limit": {
                    "source": "advertised" if output_limit else "unknown"
                },
            },
            "created_at": created_timestamp(
                item.get("created")
                or item.get("created_at")
                or item.get("updated_at")
            ),
        }
        if capabilities:
            entry["capabilities"] = capabilities
        entry["capability_sources"].update(capability_sources)
        result.append(entry)
    return enrich_discovered_models(provider, result)


def _anthropic_models(io: DiscoveryIO, provider: Dict[str, Any]) -> list:
    base = provider["base_url"].rstrip("/")
    for suffix in ("/messages", "/chat/completions", "/responses"):
        if base.endswith(suffix):
            base = base[: -len(suffix)]
            break
    headers = io.anthropic_headers(provider)
    result = []
    budget = {"bytes": 0, "deadline": time.monotonic() + MAX_DISCOVERY_SECONDS}
    url = base + "/models?limit=1000"
    for _ in range(20):
        value = _get_json(
            io, Request(url, headers=headers), "Anthropic 模型列表查询", budget
        )
        for item in value.get("data", []):
            if len(result) >= MAX_DISCOVERED_MODELS:
                raise RouterError("上游模型列表超过安全上限", 502)
            if not isinstance(item, dict):
                continue
            model_id = _model_id(item.get("id"))
            if not model_id:
                continue
            caps = item.get("capabilities")
            caps = caps if isinstance(caps, dict) else {}
            thinking = _nested_supported(caps, "thinking")
            effort = _nested_supported(caps, "effort")
            image = _nested_supported(caps, "image_input")
            pdf = _nested_supported(caps, "pdf_input")
            structured_output = _nested_supported(caps, "structured_outputs")
            explicit_reasoning = [
                value for value in (thinking, effort) if value is not None
            ]
            supports_reasoning = (
                any(explicit_reasoning) if explicit_reasoning else None
            )
            supports_summaries = advertised_reasoning_summaries(item)
            reasoning_levels = []
            if effort:
                effort_obj = caps.get("effort")
                effort_obj = effort_obj if isinstance(effort_obj, Mapping) else {}
                for level in ("low", "medium", "high", "xhigh", "max"):
                    if _nested_supported(effort_obj, level):
                        reasoning_levels.append(level)
            capabilities = {}
            capability_sources = {}
            if structured_output is not None:
                capabilities["structured_output"] = structured_output
                capability_sources["structured_output"] = {"source": "advertised"}
            modality_evidence = image is not None or pdf is not None
            raw_input = ["text"]
            if image:
                raw_input.append("image")
            if pdf:
                raw_input.append("pdf")
            max_input = positive_int(item.get("max_input_tokens"))
            max_output = positive_int(item.get("max_tokens"))
            created_at = created_timestamp(item.get("created_at"))
            entry = {
                "upstream_id": model_id,
                "display_name": _model_text(
                    item.get("display_name"), model_id, "显示名称"
                ),
                "description": "",
                "context_window": max_input,
                "max_input_tokens": max_input,
                "output_limit": max_output,
                "supports_reasoning": supports_reasoning,
                "supports_reasoning_summaries": supports_summaries,
                "reasoning_levels": reasoning_levels,
                "input_modalities": normalize_input_modalities(raw_input),
                "output_modalities": normalize_output_modalities(None),
                "supports_image_detail_original": False,
                "capability_sources": {
                    "supports_reasoning": {
                        "source": "advertised"
                        if supports_reasoning is not None
                        else "unknown"
                    },
                    "supports_reasoning_summaries": {
                        "source": "advertised"
                        if supports_summaries is not None
                        else "unknown"
                    },
                    "reasoning_levels": {
                        "source": "advertised" if reasoning_levels else "unknown"
                    },
                    "input_modalities": {
                        "source": "advertised" if modality_evidence else "unknown"
                    },
                    "output_modalities": {
                        "source": output_modalities_metadata_source(None)
                    },
                    "supports_image_detail_original": {"source": "unknown"},
                    "context_window": {
                        "source": "advertised" if max_input else "unknown"
                    },
                    "max_input_tokens": {
                        "source": "advertised" if max_input else "unknown"
                    },
                    "output_limit": {
                        "source": "advertised" if max_output else "unknown"
                    },
                },
                "created_at": created_at,
            }
            if capabilities:
                entry["capabilities"] = capabilities
                entry["capability_sources"].update(capability_sources)
            result.append(entry)
        after_id = value.get("last_id")
        if not value.get("has_more") or not after_id:
            break
        url = base + "/models?limit=1000&after_id=" + quote(
            str(after_id), safe=""
        )
    return enrich_discovered_models(provider, result)


def discover_models(io: DiscoveryIO, provider: Dict[str, Any]) -> list:
    """Fetch safe model metadata without making one request per model."""

    if provider.get("protocol") == "anthropic_messages" or (
        provider.get("protocol") == "auto"
        and provider.get("auth_mode") == "anthropic_api_key"
    ):
        return _anthropic_models(io, provider)
    parsed = urlparse(provider.get("base_url", ""))
    if parsed.hostname == "generativelanguage.googleapis.com":
        return _gemini_models(io, provider)
    return _generic_models(io, provider)
