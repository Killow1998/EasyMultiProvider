"""Tiny stdio MCP server used by the real Codex acceptance flow."""

from __future__ import annotations

import json
import os
import sys
from pathlib import Path
from typing import Any


def reply(message_id: Any, result: Any = None, error: Any = None) -> None:
    message: dict[str, Any] = {"jsonrpc": "2.0", "id": message_id}
    if error is not None:
        message["error"] = error
    else:
        message["result"] = result
    sys.stdout.buffer.write(json.dumps(message, separators=(",", ":")).encode("utf-8") + b"\n")
    sys.stdout.buffer.flush()


def main() -> int:
    if len(sys.argv) != 2:
        return 2
    log_path = Path(sys.argv[1])
    for raw in sys.stdin.buffer:
        try:
            message = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            continue
        if not isinstance(message, dict) or "id" not in message:
            continue
        method = message.get("method")
        request_id = message["id"]
        if method == "initialize":
            reply(
                request_id,
                {
                    "protocolVersion": message.get("params", {}).get("protocolVersion", "2024-11-05"),
                    "capabilities": {"tools": {"listChanged": False}},
                    "serverInfo": {"name": "fixture", "version": "1.0.0"},
                },
            )
        elif method == "tools/list":
            reply(
                request_id,
                {
                    "tools": [
                        {
                            "name": "fixture_tool",
                            "description": "Return the deterministic EMP portability fixture result.",
                            "inputSchema": {
                                "type": "object",
                                "properties": {"fixture": {"type": "boolean"}},
                                "required": ["fixture"],
                                "additionalProperties": False,
                            },
                        }
                    ]
                },
            )
        elif method == "tools/call":
            params = message.get("params")
            if not isinstance(params, dict) or params.get("name") != "fixture_tool":
                reply(request_id, error={"code": -32602, "message": "unknown fixture tool"})
                continue
            arguments = params.get("arguments", {})
            log_path.parent.mkdir(parents=True, exist_ok=True)
            with log_path.open("ab", buffering=0) as stream:
                stream.write(json.dumps({"name": params["name"], "arguments": arguments}, separators=(",", ":")).encode("utf-8") + b"\n")
                os.fsync(stream.fileno())
            reply(
                request_id,
                {
                    "content": [{"type": "text", "text": "fixture tool completed"}],
                    "isError": False,
                },
            )
        elif method == "ping":
            reply(request_id, {})
        else:
            reply(request_id, error={"code": -32601, "message": "method not implemented"})
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
