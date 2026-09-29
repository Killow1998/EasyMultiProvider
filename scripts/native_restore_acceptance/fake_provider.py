from __future__ import annotations

import base64
import http.server
import json
import re
import threading
import time
from typing import Any

from .common import json_value_sha256, sha256
from .constants import (
    BUILD_PROMPTS,
    CALL_AFTER_COMPACTION,
    CALL_BEFORE_COMPACTION,
    CHILD_SUFFIX,
    CREATED_FLOW_PROMPTS,
    EARLY_CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    MCP_NAMESPACE,
    MCP_TOOL_NAME,
    MODEL,
    OPAQUE,
    OPAQUE_ID,
    PARENT_SUFFIX,
    PROBE_PROMPTS,
    SUMMARY,
    TOOL_ARGUMENTS,
)

LOCAL_MARKERS = (
    PARENT_SUFFIX,
    CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    EARLY_CHILD_SUFFIX,
    CREATED_FLOW_PROMPTS["early_child"],
    CREATED_FLOW_PROMPTS["late_child"],
    CREATED_FLOW_PROMPTS["grandchild"],
)


class FakeResponsesProvider:
    def __init__(self) -> None:
        self.lock = threading.Lock()
        self.reject_emp1 = False
        self.requests: list[dict[str, Any]] = []
        self.sequence = 0

        owner = self

        class Handler(http.server.BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, _format: str, *_args: Any) -> None:
                return

            def do_POST(self) -> None:  # noqa: N802
                length = int(self.headers.get("Content-Length", "0"))
                if length <= 0 or length > 16 * 1024 * 1024:
                    self.send_error(413)
                    return
                try:
                    body = json.loads(self.rfile.read(length))
                except (UnicodeDecodeError, json.JSONDecodeError):
                    self.send_error(400)
                    return
                prompt = owner.classify_prompt(body)
                record = owner.summarize_request(
                    body,
                    prompt,
                    has_emp_request_id=self.headers.get("X-EMP-Request-ID") is not None,
                )
                record["http_method"] = "POST"
                record["http_status"] = None
                record["provider_error_code"] = None
                with owner.lock:
                    owner.requests.append(record)
                    owner.sequence += 1
                    sequence = owner.sequence
                    reject = owner.reject_emp1 and record["has_emp1"]
                if reject:
                    payload = json.dumps(
                        {
                            "error": {
                                "message": "native acceptance endpoint rejects EMP compaction markers",
                                "type": "invalid_request_error",
                                "code": "emp1_rejected",
                            }
                        }
                    ).encode("utf-8")
                    record["http_status"] = 400
                    record["provider_error_code"] = "emp1_rejected"
                    self.send_response(400)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(payload)))
                    self.end_headers()
                    self.wfile.write(payload)
                    return
                response = owner.response(body, sequence, prompt)
                record["http_status"] = 200
                if body.get("stream") is True:
                    payload = owner.sse(response)
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Cache-Control", "no-cache")
                else:
                    payload = json.dumps(response, separators=(",", ":")).encode("utf-8")
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                self.send_header("Content-Length", str(len(payload)))
                self.end_headers()
                self.wfile.write(payload)

        self.server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.server.daemon_threads = True
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.base_url = f"http://127.0.0.1:{self.server.server_port}/v1"

    def stop(self) -> None:
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    @staticmethod
    def classify_prompt(body: Any) -> str:
        serialized = json.dumps(body, ensure_ascii=False)
        if "CONTEXT CHECKPOINT COMPACTION" in serialized:
            return "external_compaction"
        matches = []
        for prompts in (PROBE_PROMPTS, CREATED_FLOW_PROMPTS, BUILD_PROMPTS):
            for key, value in prompts.items():
                position = FakeResponsesProvider.last_exact_token_position(serialized, value)
                if position >= 0:
                    matches.append((position, key))
        return max(matches)[1] if matches else "setup"

    @staticmethod
    def exact_token_positions(serialized: str, token: str) -> list[int]:
        pattern = re.compile(
            rf"(?<![A-Za-z0-9_]){re.escape(token)}(?![A-Za-z0-9_])",
        )
        return [match.start() for match in pattern.finditer(serialized)]

    @staticmethod
    def last_exact_token_position(serialized: str, token: str) -> int:
        positions = FakeResponsesProvider.exact_token_positions(serialized, token)
        return positions[-1] if positions else -1

    @staticmethod
    def local_marker_projection(serialized: str) -> list[str]:
        occurrences = [
            (position, marker_index, marker)
            for marker_index, marker in enumerate(LOCAL_MARKERS)
            for position in FakeResponsesProvider.exact_token_positions(serialized, marker)
        ]
        return [marker for _, _, marker in sorted(occurrences)]

    @staticmethod
    def has_exact_token(serialized: str, token: str) -> bool:
        return FakeResponsesProvider.last_exact_token_position(serialized, token) >= 0

    def summarize_request(
        self,
        body: Any,
        prompt: str,
        *,
        has_emp_request_id: bool = False,
    ) -> dict[str, Any]:
        serialized = json.dumps(body, ensure_ascii=False)
        calls: list[dict[str, Any]] = []
        outputs: list[dict[str, Any]] = []
        opaque_items: list[dict[str, Any]] = []
        emp1_compaction_ids: list[str] = []
        item_types: list[str] = []
        summary_seen = SUMMARY in serialized

        def walk(value: Any) -> None:
            if isinstance(value, dict):
                kind = value.get("type")
                if isinstance(kind, str) and kind in {
                    "compaction",
                    "message",
                    "reasoning",
                    "function_call",
                    "function_call_output",
                }:
                    item_types.append(kind)
                if kind == "compaction" and isinstance(value.get("encrypted_content"), str):
                    content = value["encrypted_content"]
                    if content.startswith("emp1:"):
                        item_id = value.get("id")
                        emp1_compaction_ids.append(item_id if isinstance(item_id, str) else "")
                if kind == "function_call":
                    arguments = value.get("arguments")
                    name = value.get("name")
                    namespace = value.get("namespace")
                    canonical_name = (
                        f"{namespace}__{name}"
                        if isinstance(namespace, str) and namespace and isinstance(name, str)
                        else name
                    )
                    calls.append(
                        {
                            "item_id": value.get("id"),
                            "call_id": value.get("call_id"),
                            "name": canonical_name,
                            "arguments_sha256": (
                                sha256(arguments.encode("utf-8"))
                                if isinstance(arguments, str)
                                else json_value_sha256(arguments)
                            ),
                        }
                    )
                if kind == "function_call_output":
                    output = value.get("output")
                    outputs.append(
                        {
                            "item_id": value.get("id"),
                            "call_id": value.get("call_id"),
                            "name": value.get("name"),
                            "output_sha256": (
                                sha256(output.encode("utf-8"))
                                if isinstance(output, str)
                                else json_value_sha256(output)
                            ),
                        }
                    )
                if kind == "reasoning" and (
                    value.get("id") == OPAQUE_ID or value.get("encrypted_content") == OPAQUE
                ):
                    content = value.get("encrypted_content")
                    opaque_items.append(
                        {
                            "item_id": value.get("id"),
                            "encrypted_content_sha256": (
                                sha256(content.encode("utf-8"))
                                if isinstance(content, str)
                                else None
                            ),
                        }
                    )
                for child in value.values():
                    walk(child)
            elif isinstance(value, list):
                for child in value:
                    walk(child)

        walk(body)
        prompt_phase = (
            "post_restore_probe"
            if prompt in PROBE_PROMPTS
            else "external_compaction"
            if prompt == "external_compaction"
            else "pre_restore_created_flow"
            if prompt in CREATED_FLOW_PROMPTS
            else "pre_restore_build"
            if prompt in BUILD_PROMPTS
            else "setup"
        )
        return {
            "prompt": prompt,
            "phase": prompt_phase,
            "request_body_bytes": len(serialized.encode("utf-8")),
            "source": "emp_upstream" if has_emp_request_id else "codex_direct",
            "emp_request_id_present": has_emp_request_id,
            "has_emp1": bool(emp1_compaction_ids),
            "emp1_compaction_ids": sorted(emp1_compaction_ids),
            "compaction_prompt_present": "CONTEXT CHECKPOINT COMPACTION" in serialized,
            "summary_seen": summary_seen,
            "portable_summary_seen": "emp1:" + base64.urlsafe_b64encode(SUMMARY.encode("utf-8")).decode("ascii").rstrip("=") in serialized,
            "opaque_items": sorted(opaque_items, key=lambda item: str(item.get("item_id"))),
            "local_suffixes_seen": [
                suffix
                for suffix in LOCAL_MARKERS
                if FakeResponsesProvider.has_exact_token(serialized, suffix)
            ],
            "local_marker_projection": FakeResponsesProvider.local_marker_projection(serialized),
            "item_types": sorted(set(item_types)),
            "function_calls": sorted(calls, key=lambda item: str(item.get("item_id"))),
            "function_outputs": sorted(outputs, key=lambda item: str(item.get("item_id"))),
        }

    @staticmethod
    def response(request: Any, sequence: int, prompt: str) -> dict[str, Any]:
        response_id = f"resp_emp_acceptance_{sequence:03d}"
        item_id = f"msg_emp_acceptance_{sequence:03d}"
        output: list[dict[str, Any]]
        if prompt == "external_compaction":
            text = SUMMARY + "\nPreserve the fixture tool result and local branch suffixes."
            output = [
                {
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": text, "annotations": []}],
                }
            ]
            usage = None
        elif prompt in {"tool_before_compaction", "tool_after_compaction"}:
            call_id = CALL_BEFORE_COMPACTION if prompt == "tool_before_compaction" else CALL_AFTER_COMPACTION
            if not FakeResponsesProvider.has_tool_output(request.get("input", []), call_id):
                output = [
                    {
                        "id": f"fc_{sequence:03d}",
                        "type": "function_call",
                        "status": "completed",
                        "call_id": call_id,
                        "name": MCP_TOOL_NAME,
                        "namespace": MCP_NAMESPACE,
                        "arguments": TOOL_ARGUMENTS,
                    }
                ]
                usage = {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
            else:
                output = [
                    {
                        "id": item_id,
                        "type": "message",
                        "status": "completed",
                        "role": "assistant",
                        "content": [{"type": "output_text", "text": "native fixture accepted", "annotations": []}],
                    }
                ]
                usage = {"input_tokens": 1, "output_tokens": 1, "total_tokens": 2}
        else:
            output = [
                {
                    "id": item_id,
                    "type": "message",
                    "status": "completed",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "native fixture accepted",
                            "annotations": [],
                        }
                    ],
                }
            ]
            total = 8192 if prompt == "compaction_threshold" else 2
            usage = {"input_tokens": total - 1, "output_tokens": 1, "total_tokens": total}
        return {
            "id": response_id,
            "object": "response",
            "created_at": int(time.time()),
            "status": "completed",
            "model": request.get("model", MODEL),
            "output": output,
            "parallel_tool_calls": True,
            "usage": usage,
        }

    @staticmethod
    def has_tool_output(value: Any, call_id: str) -> bool:
        if isinstance(value, dict):
            if value.get("type") == "function_call_output" and value.get("call_id") == call_id:
                return True
            return any(FakeResponsesProvider.has_tool_output(item, call_id) for item in value.values())
        if isinstance(value, list):
            return any(FakeResponsesProvider.has_tool_output(item, call_id) for item in value)
        return False

    @staticmethod
    def sse(response: dict[str, Any]) -> bytes:
        events: list[dict[str, Any]] = [
            {
                "type": "response.created",
                "response": {
                    "id": response.get("id"),
                    "status": "in_progress",
                    "model": response.get("model"),
                },
            }
        ]
        for output_index, item in enumerate(response.get("output", [])):
            item_type = item.get("type")
            item_id = item.get("id")
            if item_type == "function_call":
                added = {
                    "id": item_id,
                    "type": "function_call",
                    "status": "in_progress",
                    "call_id": item.get("call_id"),
                    "name": item.get("name"),
                    "namespace": item.get("namespace"),
                    "arguments": "",
                }
                events.append(
                    {
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": added,
                    }
                )
                arguments = str(item.get("arguments", ""))
                events.extend(
                    [
                        {
                            "type": "response.function_call_arguments.delta",
                            "output_index": output_index,
                            "item_id": item_id,
                            "delta": arguments,
                        },
                        {
                            "type": "response.function_call_arguments.done",
                            "output_index": output_index,
                            "item_id": item_id,
                            "arguments": arguments,
                        },
                    ]
                )
            elif item_type == "message":
                added = {
                    "id": item_id,
                    "type": "message",
                    "status": "in_progress",
                    "role": item.get("role", "assistant"),
                    "content": [],
                }
                events.append(
                    {
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": added,
                    }
                )
                for content_index, content in enumerate(item.get("content", [])):
                    if content.get("type") == "output_text":
                        events.append(
                            {
                                "type": "response.output_text.delta",
                                "output_index": output_index,
                                "content_index": content_index,
                                "item_id": item_id,
                                "delta": content.get("text", ""),
                            }
                        )
            else:
                added = dict(item)
                added["status"] = "in_progress"
                events.append(
                    {
                        "type": "response.output_item.added",
                        "output_index": output_index,
                        "item": added,
                    }
                )
            events.append(
                {
                    "type": "response.output_item.done",
                    "output_index": output_index,
                    "item": item,
                }
            )
        events.append({"type": "response.completed", "response": response})
        payload = "".join(
            "event: "
            + event["type"]
            + "\ndata: "
            + json.dumps(event, separators=(",", ":"))
            + "\n\n"
            for event in events
        )
        return payload.encode("utf-8")
