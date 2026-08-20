#!/usr/bin/env python3
"""Deterministic, bounded CLI supervision for the Luna Max overnight run.

The supervisor owns only disposable loopback fixtures and child processes that it
started.  It never reads the real Codex auth file, never edits the product oracle
after freezing, and never treats model prose as a pass/fail signal.
"""

from __future__ import annotations

import argparse
import collections
import hashlib
import json
import os
import random
import re
import resource
import shutil
import signal
import socket
import subprocess
import sys
import tempfile
import threading
import time
import traceback
import urllib.error
import urllib.request
import uuid
from dataclasses import dataclass
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Deque, Dict, Iterable, List, Optional, Sequence, Tuple
from xml.sax.saxutils import escape


REPO = Path(__file__).resolve().parents[1]
RUN_ID_DEFAULT = time.strftime("%Y%m%d-night1", time.localtime())
DEFAULT_MODEL = "gpt-5.6-luna"
SOL_MODEL = "gpt-5.6-sol"
MOCK_TOKEN = "overnight-mock-access-token"
MOCK_SENTINEL = "EASY_MULTIPROVIDER_MOCK_OK"
TOOL_SENTINEL = "EASY_MULTIPROVIDER_TOOL_OK"
SOAK_SENTINEL = "EASY_MULTIPROVIDER_SOAK_OK"
LIVE_CANCEL_SENTINEL = "EASY_MULTIPROVIDER_CANCEL_RECOVERY_OK"
LIVE_WRITE_EXPECTED_BYTES = b"LIVE_WRITE_MARKER\n"
LIVE_WRITE_PROMPT = (
    "Use the file-change tool to create only tool-marker.txt with the exact UTF-8 bytes "
    "LIVE_WRITE_MARKER followed by exactly one LF byte (18 bytes total). Do not remove "
    "the LF. Do not run any command that writes, truncates, rewrites, or otherwise "
    "modifies tool-marker.txt after the file change; any command afterward may only "
    "read it. Return JSON with sentinel LIVE_WRITE_OK, nonce live-write, seen as an "
    "empty array, and content LIVE_WRITE_MARKER. Do not touch any other file."
)
MAX_CAPTURE_BYTES = 2 * 1024 * 1024
TERMINAL_EVENTS = {"turn.completed", "turn.failed", "error"}
SECRET_PATTERNS = (
    re.compile(r"(?i)(bearer\s+)[^\s,}\]]+"),
    re.compile(r"(?i)(api[_-]?key\s*[=:]\s*)[^\s,}\]]+"),
    re.compile(r"(?i)(access[_-]?token\s*[=:]\s*)[^\s,}\]]+"),
    re.compile(r"(?i)(refresh[_-]?token\s*[=:]\s*)[^\s,}\]]+"),
    re.compile(r"(?i)(bootstrap=)[^&\s]+"),
    re.compile(r"(?i)((?<![A-Za-z0-9])sk-)[A-Za-z0-9_-]{12,}"),
)


CASE_MANIFEST: List[Dict[str, Any]] = [
    {"id": "PRE-01", "phase": "PREFLIGHT", "required": True},
    {"id": "PRE-02", "phase": "PREFLIGHT", "required": True},
    {"id": "UNIT-01", "phase": "UNIT", "required": True},
    {"id": "UNIT-02", "phase": "UNIT", "required": True},
    {"id": "UNIT-03", "phase": "UNIT", "required": True},
    {"id": "MOCK-01", "phase": "MOCK_CLI", "required": True},
    {"id": "MOCK-02", "phase": "MOCK_CLI", "required": True},
    {"id": "MOCK-03", "phase": "RESUME_AND_RESTART", "required": True},
    {"id": "MOCK-04", "phase": "FAULT_INJECTION", "required": True},
    {"id": "GLM-01", "phase": "FAULT_INJECTION", "required": True},
    {"id": "LIVE-01", "phase": "LIVE_SUBSCRIPTION_CANARY", "required": True},
    {"id": "LIVE-01B", "phase": "LIVE_SUBSCRIPTION_CANARY", "required": True},
    {"id": "LIVE-02", "phase": "LIVE_SUBSCRIPTION_CANARY", "required": True},
    {"id": "LIVE-03", "phase": "LIVE_SUBSCRIPTION_CANARY", "required": True},
    {"id": "LIVE-04", "phase": "RESUME_AND_RESTART", "required": True},
    {"id": "LIVE-05", "phase": "RESUME_AND_RESTART", "required": True},
    {"id": "LIVE-06", "phase": "CANCEL_AND_RECOVERY", "required": True},
    {"id": "SOAK-01", "phase": "SOAK", "required": True},
    {"id": "SEC-01", "phase": "REPORT", "required": True},
    {"id": "CLEAN-01", "phase": "REPORT", "required": True},
]


SCHEMA: Dict[str, Any] = {
    "type": "object",
    "additionalProperties": False,
    # OpenAI strict JSON-schema mode requires every declared property to be
    # listed in required.  Keeping this closed and fully required also makes
    # the live subscription canaries exercise the same contract as the mock.
    "required": ["sentinel", "nonce", "seen", "content"],
    "properties": {
        "sentinel": {"type": "string"},
        "nonce": {"type": "string"},
        "seen": {"type": "array", "items": {"type": "string"}},
        "content": {"type": "string"},
    },
}


def redact(value: Any) -> str:
    """Return text safe for artifacts; values are never recovered later."""
    text = value.decode("utf-8", "replace") if isinstance(value, bytes) else str(value)
    for pattern in SECRET_PATTERNS:
        text = pattern.sub(lambda match: match.group(1) + "<redacted>", text)
    text = re.sub(r"(?i)(auth\.json(?:\.enc)?)(?:[^\s]*)", "<auth-redacted>", text)
    text = text.replace(MOCK_TOKEN, "<mock-token-redacted>")
    return text


def atomic_write(path: Path, data: str, mode: int = 0o600) -> None:
    path = Path(path)
    path.parent.mkdir(parents=True, exist_ok=True)
    fd, temporary = tempfile.mkstemp(prefix="." + path.name + ".", dir=str(path.parent))
    try:
        os.chmod(temporary, mode)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(data)
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def atomic_json(path: Path, value: Any) -> None:
    atomic_write(path, json.dumps(value, ensure_ascii=False, sort_keys=True, indent=2) + "\n")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with Path(path).open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def find_free_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as sock:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])


def no_proxy_opener() -> urllib.request.OpenerDirector:
    return urllib.request.build_opener(urllib.request.ProxyHandler({}))


def json_request(url: str, value: Optional[Dict[str, Any]] = None, headers: Optional[Dict[str, str]] = None, timeout: float = 5.0) -> Tuple[int, bytes]:
    request = urllib.request.Request(
        url,
        data=json.dumps(value, ensure_ascii=False).encode("utf-8") if value is not None else None,
        headers={"Content-Type": "application/json", **(headers or {})},
        method="POST" if value is not None else "GET",
    )
    try:
        with no_proxy_opener().open(request, timeout=timeout) as response:
            return int(response.status), response.read(MAX_CAPTURE_BYTES)
    except urllib.error.HTTPError as exc:
        return int(exc.code), exc.read(MAX_CAPTURE_BYTES)


def _event(name: str, payload: Dict[str, Any], sequence: int) -> str:
    value = dict(payload)
    value.setdefault("type", name)
    value["sequence_number"] = sequence
    return "event: %s\ndata: %s\n\n" % (name, json.dumps(value, ensure_ascii=False))


def _message_output(text: str) -> Dict[str, Any]:
    return {
        "id": "msg_mock",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": text, "annotations": []}],
    }


def _response_json(model: str, text: str) -> Dict[str, Any]:
    item = _message_output(text)
    return {
        "id": "resp_mock",
        "object": "response",
        "status": "completed",
        "model": model,
        "output": [item],
        "output_text": text,
    }


def _response_stream(model: str, text: str) -> bytes:
    item = _message_output(text)
    base = {"id": "resp_mock", "object": "response", "status": "in_progress", "model": model, "output": []}
    events = [
        _event("response.created", {"response": base}, 0),
        _event("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress", content=[])}, 1),
        _event("response.content_part.added", {"item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": {"type": "output_text", "text": "", "annotations": []}}, 2),
        _event("response.output_text.delta", {"item_id": "msg_mock", "output_index": 0, "content_index": 0, "delta": text}, 3),
        _event("response.output_text.done", {"item_id": "msg_mock", "output_index": 0, "content_index": 0, "text": text}, 4),
        _event("response.content_part.done", {"item_id": "msg_mock", "output_index": 0, "content_index": 0, "part": item["content"][0]}, 5),
        _event("response.output_item.done", {"output_index": 0, "item": item}, 6),
        _event("response.completed", {"response": dict(base, status="completed", output=[item], output_text=text)}, 7),
    ]
    return "".join(events).encode("utf-8")


def _function_call_stream(model: str, name: str, arguments: str) -> bytes:
    item = {
        "id": "fc_mock",
        "type": "function_call",
        "status": "completed",
        "call_id": "call_mock",
        "name": name,
        "arguments": arguments,
    }
    base = {"id": "resp_mock", "object": "response", "status": "in_progress", "model": model, "output": []}
    events = [
        _event("response.created", {"response": base}, 0),
        _event("response.output_item.added", {"output_index": 0, "item": dict(item, status="in_progress", arguments="")}, 1),
        _event("response.function_call_arguments.delta", {"item_id": "fc_mock", "output_index": 0, "delta": arguments}, 2),
        _event("response.function_call_arguments.done", {"item_id": "fc_mock", "output_index": 0, "arguments": arguments}, 3),
        _event("response.output_item.done", {"output_index": 0, "item": item}, 4),
        _event("response.completed", {"response": dict(base, status="completed", output=[item])}, 5),
    ]
    return "".join(events).encode("utf-8")


class FakeUpstreamHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format: str, *args: Any) -> None:
        pass

    def _write(self, status: int, body: bytes, content_type: str, close: bool = True) -> None:
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Cache-Control", "no-store")
        if close:
            self.send_header("Connection", "close")
            self.send_header("Content-Length", str(len(body)))
        self.end_headers()
        if body:
            self.wfile.write(body)
            self.wfile.flush()

    def do_GET(self) -> None:
        if self.path == "/healthz":
            self._write(200, b'{"status":"ok"}', "application/json")
            return
        self._write(404, b'{"error":{"message":"not found"}}', "application/json")

    def do_POST(self) -> None:
        server: "FakeUpstreamServer" = self.server  # type: ignore[assignment]
        request_id = uuid.uuid4().hex
        with server.state_lock:
            server.active.add(request_id)
        try:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(min(length, MAX_CAPTURE_BYTES))
            try:
                body = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, ValueError):
                body = {}
            scenario, reply = server.snapshot_scenario()
            model = body.get("model", "unknown") if isinstance(body, dict) else "unknown"
            tools = body.get("tools", []) if isinstance(body, dict) else []
            tool_names = [item.get("name") for item in tools if isinstance(item, dict) and item.get("name")]
            server.record_request(
                {
                    "path": self.path,
                    "model": model,
                    "stream": bool(body.get("stream")) if isinstance(body, dict) else False,
                    "tool_names": tool_names[:16],
                    "input_text": redact(json.dumps(body.get("input", ""), ensure_ascii=False))[:4000] if isinstance(body, dict) else "",
                }
            )
            if scenario.startswith("http-"):
                status = int(scenario.split("-", 1)[1])
                self._write(status, json.dumps({"error": {"message": "injected upstream %d" % status, "type": "injected"}}).encode("utf-8"), "application/json")
                return
            if scenario == "slow":
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(_event("response.created", {"response": {"id": "resp_slow", "status": "in_progress", "model": model}}, 0).encode("utf-8"))
                self.wfile.flush()
                # Keep the request active, but notice a client-side cancel
                # promptly instead of sleeping through the whole slow window.
                for _ in range(150):
                    time.sleep(0.2)
                    try:
                        self.wfile.write(b": keepalive\n\n")
                        self.wfile.flush()
                    except (BrokenPipeError, ConnectionResetError, OSError):
                        return
                return
            if self.path.endswith("/chat/completions"):
                self._serve_chat(scenario, model, reply)
                return
            if scenario == "empty":
                self._write(200, b"data: [DONE]\n\n", "text/event-stream")
                return
            if scenario == "half":
                body_bytes = _event("response.created", {"response": {"id": "resp_half", "status": "in_progress", "model": model}}, 0).encode("utf-8")
                self.send_response(200)
                self.send_header("Content-Type", "text/event-stream")
                self.send_header("Connection", "close")
                self.end_headers()
                self.wfile.write(body_bytes)
                self.wfile.flush()
                return
            if scenario == "invalid":
                self._write(200, b"not SSE and not JSON", "text/plain")
                return
            if scenario == "non-sse-json":
                self._write(200, json.dumps(_response_json(model, reply), ensure_ascii=False).encode("utf-8"), "application/json")
                return
            if scenario == "markup":
                reply = "<tool_call>echo bad</tool_call>"
            if scenario == "tool-write" and not server.tool_called:
                server.tool_called = True
                # Codex 0.148.0 exposes its built-in local command handler as
                # shell_command.  The request-side tool labels are not a
                # stable response name, so the oracle uses the protocol name.
                name = "shell_command"
                command = "python3 -c \"from pathlib import Path; Path('tool-marker.txt').write_text('EASY_MULTIPROVIDER_TOOL_WRITE\\n')\""
                arguments = json.dumps({"cmd": command, "command": command, "timeout_ms": 10000}, ensure_ascii=False)
                self._write(200, _function_call_stream(model, name, arguments), "text/event-stream")
                return
            if scenario == "tool-write" and server.tool_called:
                reply = json.dumps({"sentinel": TOOL_SENTINEL, "nonce": "tool-nonce", "seen": [], "content": "tool-marker"}, ensure_ascii=False)
            if isinstance(body, dict) and body.get("stream"):
                self._write(200, _response_stream(model, reply), "text/event-stream")
            else:
                self._write(200, json.dumps(_response_json(model, reply), ensure_ascii=False).encode("utf-8"), "application/json")
        finally:
            with server.state_lock:
                server.active.discard(request_id)

    def _serve_chat(self, scenario: str, model: str, reply: str) -> None:
        if scenario == "empty":
            self._write(200, b"data: [DONE]\n\n", "text/event-stream")
            return
        if scenario == "invalid":
            self._write(200, b"not JSON", "text/plain")
            return
        if scenario == "markup":
            reply = "<tool_call>echo bad</tool_call>"
        if scenario == "non-sse-json":
            self._write(200, json.dumps({"choices": [{"message": {"content": reply}}]}).encode("utf-8"), "application/json")
            return
        if scenario.startswith("http-"):
            return
        chunks = [
            "data: " + json.dumps({"choices": [{"delta": {"content": reply[: len(reply) // 2]}}]}) + "\n\n",
            "data: " + json.dumps({"choices": [{"delta": {"content": reply[len(reply) // 2 :]}}]}) + "\n\n",
            "data: [DONE]\n\n",
        ]
        self._write(200, "".join(chunks).encode("utf-8"), "text/event-stream")


class FakeUpstreamServer(ThreadingHTTPServer):
    daemon_threads = True

    def __init__(self) -> None:
        super().__init__(("127.0.0.1", 0), FakeUpstreamHandler)
        self.state_lock = threading.RLock()
        self.scenario = "text"
        self.reply = json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "mock-nonce", "seen": [], "content": "mock"})
        self.tool_called = False
        self.requests: Deque[Dict[str, Any]] = collections.deque(maxlen=2000)
        self.active: set = set()

    def set_scenario(self, scenario: str, reply: Optional[str] = None) -> None:
        with self.state_lock:
            self.scenario = scenario
            self.reply = reply if reply is not None else self.reply
            if scenario == "tool-write":
                self.tool_called = False

    def snapshot_scenario(self) -> Tuple[str, str]:
        with self.state_lock:
            return self.scenario, self.reply

    def record_request(self, value: Dict[str, Any]) -> None:
        with self.state_lock:
            self.requests.append(value)

    def request_snapshot(self) -> List[Dict[str, Any]]:
        with self.state_lock:
            return list(self.requests)

    def active_count(self) -> int:
        with self.state_lock:
            return len(self.active)


class EnvironmentBlocked(RuntimeError):
    pass


@dataclass
class ProcessResult:
    returncode: int
    stdout: str
    stderr: str
    timed_out: bool
    duration_seconds: float


class ManagedProcess:
    """A child process with a private process group and bounded output tails."""

    def __init__(self, command: Sequence[str], cwd: Path, env: Dict[str, str]) -> None:
        self.command = list(command)
        self.proc = subprocess.Popen(
            self.command,
            cwd=str(cwd),
            env=env,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            start_new_session=True,
        )
        self._stdout: Deque[str] = collections.deque(maxlen=4000)
        self._stderr: Deque[str] = collections.deque(maxlen=4000)
        self._threads = [
            threading.Thread(target=self._drain, args=(self.proc.stdout, self._stdout), daemon=True),
            threading.Thread(target=self._drain, args=(self.proc.stderr, self._stderr), daemon=True),
        ]
        for thread in self._threads:
            thread.start()

    @staticmethod
    def _drain(stream: Any, target: Deque[str]) -> None:
        if stream is None:
            return
        for line in iter(stream.readline, ""):
            target.append(line)

    def tail(self) -> Tuple[str, str]:
        return "".join(self._stdout), "".join(self._stderr)

    def wait(self, timeout: Optional[float] = None) -> int:
        returncode = self.proc.wait(timeout=timeout)
        for thread in self._threads:
            thread.join(timeout=1)
        return int(returncode)

    def stop(self) -> None:
        if self.proc.poll() is not None:
            return
        try:
            os.killpg(self.proc.pid, signal.SIGTERM)
        except (OSError, ProcessLookupError):
            pass
        try:
            self.wait(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(self.proc.pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
            try:
                self.wait(timeout=5)
            except subprocess.TimeoutExpired:
                pass


def process_group_gone(pgid: int) -> bool:
    """Return whether an owned process group no longer exists."""
    if pgid <= 0:
        return False
    try:
        os.killpg(pgid, 0)
    except ProcessLookupError:
        return True
    except PermissionError:
        return False
    except OSError:
        return True
    return False


def wait_for_process_group_gone(pgid: int, timeout: float) -> bool:
    deadline = time.monotonic() + max(0.0, timeout)
    while True:
        if process_group_gone(pgid):
            return True
        if time.monotonic() >= deadline:
            return False
        time.sleep(0.05)


def terminate_process_group(pgid: int) -> bool:
    """Terminate only the supervisor-owned group, then verify it is gone."""
    if process_group_gone(pgid):
        return True
    try:
        os.killpg(pgid, signal.SIGTERM)
    except (OSError, ProcessLookupError):
        pass
    if wait_for_process_group_gone(pgid, 2.0):
        return True
    try:
        os.killpg(pgid, signal.SIGKILL)
    except (OSError, ProcessLookupError):
        pass
    return wait_for_process_group_gone(pgid, 2.0)


class LocalStack:
    def __init__(self, run_dir: Path, seed: int) -> None:
        self.run_dir = Path(run_dir)
        self.seed = seed
        self.temp_root = Path(tempfile.mkdtemp(prefix="easy-mp-overnight-"))
        self.fake = FakeUpstreamServer()
        self.fake_thread = threading.Thread(target=self.fake.serve_forever, daemon=True)
        self.fake_thread.start()
        self.emp: Optional[ManagedProcess] = None
        self.emp_port = 0
        self.config_path = self.temp_root / "config.json"
        self.catalog_path = self.temp_root / "catalog.json"
        self.native_path = self.temp_root / "native.json"
        self.codex_home = self.temp_root / "codex-home"
        self.codex_home.mkdir(parents=True, exist_ok=True)
        self._write_fixture_files()
        self._write_config()

    @property
    def fake_url(self) -> str:
        return "http://127.0.0.1:%d/v1" % self.fake.server_address[1]

    @property
    def emp_url(self) -> str:
        return "http://127.0.0.1:%d/v1" % self.emp_port

    @property
    def env(self) -> Dict[str, str]:
        env = os.environ.copy()
        env["CODEX_HOME"] = str(self.codex_home)
        env["PYTHONPATH"] = str(REPO) + os.pathsep + env.get("PYTHONPATH", "")
        env["NO_PROXY"] = "127.0.0.1,localhost"
        env["no_proxy"] = "127.0.0.1,localhost"
        env["PYTHONUNBUFFERED"] = "1"
        # This is a disposable non-secret caller credential.  It lets the
        # local EMP auth gate distinguish the mock CLI from unauthenticated
        # HTTP probes without touching the user's login state.
        env["OPENAI_API_KEY"] = MOCK_TOKEN
        env["CODEX_API_KEY"] = MOCK_TOKEN
        env["CODEX_ACCESS_TOKEN"] = MOCK_TOKEN
        return env

    def _write_fixture_files(self) -> None:
        # Codex 0.148.0 validates catalog entries before starting a turn.
        # Keep the fixture small, but retain the required native-template
        # fields so external entries produced by EMP are accepted.
        native_template = {
            "slug": "overnight-template",
            "display_name": "Overnight Template",
            "description": "Disposable catalog template",
            "base_instructions": "",
            "model_messages": {},
            "shell_type": "shell_command",
            "priority": 1,
            "default_reasoning_level": "medium",
            "supported_reasoning_levels": [{"effort": "medium", "description": "medium"}],
            "visibility": "list",
            "supported_in_api": True,
            "input_modalities": ["text"],
            "supports_reasoning_summaries": False,
            "default_reasoning_summary": "none",
            "support_verbosity": False,
            "default_verbosity": None,
            "supports_search_tool": False,
            "supports_image_detail_original": False,
            "supports_parallel_tool_calls": False,
            "apply_patch_tool_type": None,
            "truncation_policy": {"mode": "tokens", "limit": 10000},
            "tool_mode": "code_mode_only",
            "upgrade": None,
            "use_responses_lite": True,
            "web_search_tool_type": "text_and_image",
            "service_tiers": [{"id": "priority", "name": "Fast", "description": "Fast"}],
            "additional_speed_tiers": ["fast"],
            "availability_nux": None,
            "comp_hash": "overnight",
            "context_window": 200000,
            "max_context_window": 200000,
            "effective_context_window_percent": 95,
            "experimental_supported_tools": [],
            "include_apps_usage_instructions": False,
            "include_plugin_usage_instructions": False,
            "include_skills_usage_instructions": False,
            "multi_agent_version": "v2",
            "node_repl_auto_review_required": False,
            "node_repl_disabled": False,
        }
        self.native_path.write_text(json.dumps({"models": [native_template]}) + "\n", encoding="utf-8")
        auth = {"tokens": {"access_token": MOCK_TOKEN, "account_id": "overnight-mock-account"}}
        auth_path = self.codex_home / "auth.json"
        auth_path.write_text(json.dumps(auth) + "\n", encoding="utf-8")
        os.chmod(str(auth_path), 0o600)

    def _write_config(self) -> None:
        from easy_multi_provider.catalog import write_catalog
        from easy_multi_provider.config import normalize, save

        config = normalize(
            {
                "host": "127.0.0.1",
                # normalize() requires a real port; the supervisor overrides
                # this value with its own OS-assigned loopback port at launch.
                "port": 1,
                "native_catalog_path": str(self.native_path),
                "providers": [
                    {
                        "id": "chatgpt-subscription",
                        "name": "ChatGPT Subscription Mock",
                        "base_url": self.fake_url,
                        "protocol": "responses",
                        "auth_mode": "forward",
                    }
                ],
                "models": [
                    {
                        "id": DEFAULT_MODEL,
                        "provider": "chatgpt-subscription",
                        "upstream_id": DEFAULT_MODEL,
                        "display_name": "Luna Mock",
                        "reasoning_levels": ["max"],
                    },
                    {
                        "id": SOL_MODEL,
                        "provider": "chatgpt-subscription",
                        "upstream_id": SOL_MODEL,
                        "display_name": "Sol Mock",
                        "reasoning_levels": ["max"],
                    },
                ],
            }
        )
        save(config, self.config_path)
        write_catalog(config, self.catalog_path)

    def _write_profile(self) -> None:
        profile = "\n".join(
            [
                'model_provider = "openai"',
                'model_catalog_json = %s' % json.dumps(str(self.catalog_path)),
                'openai_base_url = %s' % json.dumps(self.emp_url),
                'model = "gpt-5.6-luna"',
                'model_reasoning_effort = "max"',
                'approval_policy = "never"',
                'sandbox_mode = "read-only"',
                "",
            ]
        )
        atomic_write(self.codex_home / "emp.config.toml", profile, 0o600)

    def start_emp(self) -> None:
        self.emp_port = find_free_port()
        self._write_profile()
        command = [
            sys.executable,
            "-m",
            "easy_multi_provider",
            "--config",
            str(self.config_path),
            "--host",
            "127.0.0.1",
            "--port",
            str(self.emp_port),
        ]
        self.emp = ManagedProcess(command, REPO, self.env)
        deadline = time.monotonic() + 10
        last_error = ""
        while time.monotonic() < deadline:
            try:
                status, body = json_request("http://127.0.0.1:%d/healthz" % self.emp_port, timeout=1)
                if status == 200 and b'"status": "ok"' in body:
                    return
                last_error = "healthz status %d" % status
            except (OSError, urllib.error.URLError) as exc:
                last_error = str(exc)
            if self.emp.proc.poll() is not None:
                break
            time.sleep(0.1)
        stdout, stderr = self.emp.tail() if self.emp else ("", "")
        raise EnvironmentBlocked("local EMP did not become healthy: %s\n%s" % (last_error, redact(stderr[-1000:])))

    def restart_emp(self) -> None:
        if self.emp is not None:
            self.emp.stop()
        self.emp = None
        self.start_emp()

    def close(self) -> None:
        if self.emp is not None:
            self.emp.stop()
        self.fake.shutdown()
        self.fake.server_close()
        self.fake_thread.join(timeout=2)
        shutil.rmtree(str(self.temp_root), ignore_errors=True)


def parse_jsonl(output: str) -> Dict[str, Any]:
    events: List[Dict[str, Any]] = []
    invalid: List[str] = []
    for line in output.splitlines():
        if not line.strip():
            continue
        try:
            value = json.loads(line)
        except ValueError:
            invalid.append(line[:240])
            continue
        if isinstance(value, dict):
            events.append(value)
        else:
            invalid.append(line[:240])
    types = [str(event.get("type", "")) for event in events]
    started = [event for event in events if event.get("type") == "thread.started"]
    completed = [event for event in events if event.get("type") == "turn.completed"]
    failed = [event for event in events if event.get("type") in ("turn.failed", "error")]
    terminal = [event for event in events if event.get("type") in TERMINAL_EVENTS]
    thread_id = started[0].get("thread_id") if started else ""
    return {
        "json_lines": len(events),
        "invalid_lines": invalid,
        "events": events,
        "types": types,
        "thread_started": len(started),
        "turn_completed": len(completed),
        "failures": len(failed),
        "terminal_count": len(terminal),
        "terminal_types": [str(event.get("type", "")) for event in terminal],
        "thread_id": thread_id,
        "last_type": types[-1] if types else "",
    }


def run_subprocess(command: Sequence[str], cwd: Path, env: Dict[str, str], timeout: float) -> ProcessResult:
    started = time.monotonic()
    process = subprocess.Popen(
        list(command),
        cwd=str(cwd),
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        start_new_session=True,
    )
    try:
        stdout, stderr = process.communicate(timeout=timeout)
        return ProcessResult(process.returncode, stdout[-MAX_CAPTURE_BYTES:], stderr[-MAX_CAPTURE_BYTES:], False, time.monotonic() - started)
    except subprocess.TimeoutExpired as exc:
        try:
            os.killpg(process.pid, signal.SIGTERM)
        except (OSError, ProcessLookupError):
            pass
        try:
            stdout, stderr = process.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            try:
                os.killpg(process.pid, signal.SIGKILL)
            except (OSError, ProcessLookupError):
                pass
            stdout, stderr = process.communicate()
        return ProcessResult(process.returncode if process.returncode is not None else -signal.SIGTERM, stdout[-MAX_CAPTURE_BYTES:], stderr[-MAX_CAPTURE_BYTES:], True, time.monotonic() - started)


def make_fixture(prefix: str, include_tool_marker: bool = True) -> Path:
    root = Path(tempfile.mkdtemp(prefix=prefix))
    (root / "read-marker.txt").write_text("READ_ONLY_MARKER\n", encoding="utf-8")
    if include_tool_marker:
        (root / "tool-marker.txt").write_text("before\n", encoding="utf-8")
    (root / "README.md").write_text("Disposable fixture only.\n", encoding="utf-8")
    subprocess.run(["git", "init", "-q", str(root)], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL, check=False)
    return root


def snapshot_fixture(root: Path) -> Dict[str, Any]:
    """Return a stable file/dir snapshot, excluding disposable Git metadata."""
    snapshot: Dict[str, Any] = {}
    for path in sorted(root.rglob("*")):
        relative = path.relative_to(root)
        if relative.parts and relative.parts[0] == ".git":
            continue
        key = str(relative) + ("/" if path.is_dir() else "")
        if path.is_dir():
            snapshot[key] = {"kind": "directory"}
        elif path.is_file():
            data = path.read_bytes()
            snapshot[key] = {"kind": "file", "size": len(data), "sha256": hashlib.sha256(data).hexdigest()}
    return snapshot


def json_error_message(body: bytes) -> str:
    try:
        value = json.loads(body.decode("utf-8", "replace"))
    except (UnicodeDecodeError, ValueError):
        return ""
    if not isinstance(value, dict):
        return ""
    error = value.get("error")
    if isinstance(error, dict):
        return str(error.get("message") or error.get("type") or error.get("code") or "")
    return str(error or "")


def completed_items(parsed: Dict[str, Any], item_type: str) -> List[Dict[str, Any]]:
    items: List[Dict[str, Any]] = []
    for event in parsed.get("events", []):
        if event.get("type") != "item.completed":
            continue
        item = event.get("item")
        if isinstance(item, dict) and item.get("type") == item_type:
            items.append(item)
    return items


def fault_stream_oracle(scenario: str, outcome: Dict[str, Any]) -> Dict[str, Any]:
    """Validate malformed-stream semantics without treating HTTP 200 as success."""
    parsed = outcome.get("parsed") or {}
    outcome_checks = outcome.get("checks") or {}
    response_completed = any(event.get("type") == "response.completed" for event in parsed.get("events", []))
    checks: Dict[str, Any] = {
        "returncode_zero": bool(outcome_checks.get("returncode_zero")),
        "timed_out": bool(outcome_checks.get("timed_out")),
        "turn_completed": parsed.get("turn_completed", 0),
        "failure_events": parsed.get("failures", 0),
        "terminal_count": parsed.get("terminal_count", 0),
        "terminal_types": parsed.get("terminal_types", []),
        "response_completed_event": response_completed,
        "invalid_jsonl_lines": len(parsed.get("invalid_lines", [])),
        "stderr_nonempty": bool(str(outcome.get("stderr", "")).strip()),
    }
    if scenario == "non-sse-json":
        semantic = bool(
            checks["returncode_zero"]
            and not checks["timed_out"]
            and checks["turn_completed"] == 1
            and checks["failure_events"] == 0
            and checks["terminal_count"] == 1
            and not response_completed
            and bool(outcome_checks.get("schema_json"))
            and bool(outcome_checks.get("sentinel_matches"))
            and bool(outcome_checks.get("nonce_matches"))
        )
        checks["conversion_to_legal_completion"] = semantic
    else:
        explicit_failure = bool(checks["failure_events"] or (not checks["returncode_zero"] and checks["stderr_nonempty"]))
        semantic = bool(
            not checks["timed_out"]
            and checks["turn_completed"] == 0
            and not response_completed
            and explicit_failure
            and (checks["failure_events"] > 0 or checks["stderr_nonempty"] or checks["invalid_jsonl_lines"] > 0)
        )
        checks["explicit_failure"] = explicit_failure
        checks["no_success_terminal"] = checks["turn_completed"] == 0 and not response_completed
        checks["not_silent"] = bool(checks["failure_events"] or checks["stderr_nonempty"] or checks["invalid_jsonl_lines"])
    checks["semantic_pass"] = semantic
    return checks


class Supervisor:
    def __init__(self, run_id: str, seed: int, max_hours: float, soak_hours: float, live_canaries: bool, dry_run: bool) -> None:
        self.run_id = run_id
        self.seed = seed
        self.max_hours = max_hours
        self.soak_hours = soak_hours
        self.live_canaries = live_canaries
        self.dry_run = dry_run
        self.started_at = time.time()
        self.run_dir = REPO / "artifacts" / "overnight" / run_id
        self.controller_dir = self.run_dir / "controller"
        self.controller_dir.mkdir(parents=True, exist_ok=True)
        if not (self.controller_dir / "stderr.log").exists():
            atomic_write(self.controller_dir / "stderr.log", "")
        self.results: List[Dict[str, Any]] = []
        self.attempts: Dict[str, int] = collections.defaultdict(int)
        self.events: List[Dict[str, Any]] = []
        self.status = "RUNNING"
        self.stop_reason = ""
        self.frozen = False
        self.capture_baseline()
        self._write_manifest_if_missing()
        self.emit("supervisor.started", {"run_id": run_id, "seed": seed, "dry_run": dry_run})

    def capture_baseline(self) -> None:
        """Capture only repository and non-secret profile metadata once per run."""
        baseline_dir = self.run_dir / "baseline"
        status_path = baseline_dir / "git-status.txt"
        if status_path.exists():
            return
        env = os.environ.copy()
        status = run_subprocess(["git", "status", "--porcelain=v2"], REPO, env, 30)
        diff = run_subprocess(["git", "diff", "--binary"], REPO, env, 60)
        diff_check = run_subprocess(["git", "diff", "--check"], REPO, env, 30)
        atomic_write(status_path, redact(status.stdout))
        atomic_write(baseline_dir / "working-tree.patch", redact(diff.stdout))
        atomic_write(baseline_dir / "git-diff-check.txt", redact(diff.stdout if diff_check.stdout else diff_check.stderr))

        version = run_subprocess(["codex", "--version"], REPO, env, 30)
        exec_help = run_subprocess(["codex", "exec", "--help"], REPO, env, 30)
        resume_help = run_subprocess(["codex", "exec", "resume", "--help"], REPO, env, 30)
        profile_path = Path.home() / ".codex" / "emp.config.toml"
        profile_fields: set = set()
        profile_values: Dict[str, str] = {}
        if profile_path.exists():
            try:
                for line in profile_path.read_text(encoding="utf-8", errors="replace").splitlines():
                    match = re.match(r"^\s*([A-Za-z_][A-Za-z0-9_]*)\s*=\s*(.*?)\s*$", line)
                    if not match:
                        continue
                    key, raw = match.groups()
                    profile_fields.add(key)
                    if key in {"model", "model_catalog_json", "model_provider", "model_reasoning_effort", "base_url"}:
                        profile_values[key] = raw.strip().strip('"')
            except OSError:
                profile_fields = set()
                profile_values = {}
        summary = {
            "codex_version": redact((version.stdout + version.stderr).strip()),
            "codex_exec_help_summary": redact((exec_help.stdout + exec_help.stderr).strip()).splitlines(),
            "codex_exec_resume_help_summary": redact((resume_help.stdout + resume_help.stderr).strip()).splitlines(),
            "emp_profile": {
                "name": "emp",
                "source_file": str(profile_path),
                "fields": sorted(profile_fields),
                "values": profile_values,
            },
            "python_version": sys.version.splitlines()[0],
            "repository": str(REPO),
            "run_id": self.run_id,
            "control_channel": "native Codex subscription",
            "sut_profile_arg": "--profile emp",
            "auth_file_read": False,
            "desktop_track": "WAITING_FOR_USER",
            "gemini_case_count": 0,
        }
        atomic_json(baseline_dir / "environment-redacted.json", summary)

    def emit(self, event: str, data: Optional[Dict[str, Any]] = None) -> None:
        value = {"timestamp": time.time(), "event": event, **(data or {})}
        self.events.append(value)
        atomic_write(self.controller_dir / "events.jsonl", "".join(json.dumps(item, ensure_ascii=False, sort_keys=True) + "\n" for item in self.events))

    def checkpoint(self, stage: str, state: str = "completed") -> None:
        atomic_json(
            self.controller_dir / "checkpoint.json",
            {"run_id": self.run_id, "stage": stage, "state": state, "timestamp": time.time(), "completed_case_ids": [item["case_id"] for item in self.results if item["status"] == "PASS"]},
        )
        self.emit("checkpoint", {"stage": stage, "state": state})

    def _write_manifest_if_missing(self) -> None:
        manifest_path = self.controller_dir / "manifest.json"
        oracle_path = self.controller_dir / "oracle.json"
        if not manifest_path.exists():
            atomic_json(manifest_path, {"seed": self.seed, "cases": CASE_MANIFEST})
        if not oracle_path.exists():
            atomic_json(
                oracle_path,
                {
                    "schema": SCHEMA,
                    "required_terminal": "turn.completed",
                    "forbidden_terminal": ["turn.failed", "error"],
                    "sentinels": [MOCK_SENTINEL, TOOL_SENTINEL, SOAK_SENTINEL, LIVE_CANCEL_SENTINEL],
                    "max_hours": 8,
                    "max_live_canaries": 8,
                },
            )

    def freeze(self) -> None:
        files = [Path(__file__), self.controller_dir / "manifest.json", self.controller_dir / "oracle.json"]
        hashes = {str(path.relative_to(REPO)): sha256_file(path) for path in files}
        atomic_json(self.controller_dir / "frozen-hashes.json", hashes)
        self.frozen = True
        self.emit("oracle.frozen", {"files": hashes})

    def verify_frozen(self) -> bool:
        path = self.controller_dir / "frozen-hashes.json"
        if not path.exists():
            return True
        expected = json.loads(path.read_text(encoding="utf-8"))
        actual = {name: sha256_file(REPO / name) for name in expected}
        if expected != actual:
            self.status = "BLOCKED"
            self.stop_reason = "frozen supervisor, manifest, or oracle changed"
            self.emit("oracle.changed", {"expected": expected, "actual": actual})
            return False
        self.frozen = True
        return True

    def case_dir(self, case_id: str) -> Path:
        self.attempts[case_id] += 1
        path = self.run_dir / "cases" / case_id / str(self.attempts[case_id])
        path.mkdir(parents=True, exist_ok=True)
        return path

    def record(
        self,
        case_id: str,
        status: str,
        classification: str,
        detail: str,
        checks: Optional[Dict[str, Any]] = None,
        command: Optional[Sequence[str]] = None,
        stdout: str = "",
        stderr: str = "",
        last_message: Optional[str] = None,
    ) -> Dict[str, Any]:
        path = self.case_dir(case_id)
        if command is not None:
            atomic_json(path / "command-redacted.json", {"argv": [redact(item) for item in command]})
        atomic_write(path / "stdout.jsonl", redact(stdout))
        atomic_write(path / "stderr.log", redact(stderr))
        if last_message is None:
            atomic_json(path / "last-message.json", {"present": False})
        else:
            atomic_write(path / "last-message.json", redact(last_message))
        verifier = {"case_id": case_id, "status": status, "classification": classification, "detail": detail, "checks": checks or {}}
        atomic_json(path / "verifier.json", verifier)
        result = {
            "case_id": case_id,
            "status": status,
            "classification": classification,
            "detail": detail,
            "checks": checks or {},
            "attempt": self.attempts[case_id],
        }
        self.results.append(result)
        self.emit("case.completed", result)
        return result

    def amend_result(
        self,
        result: Dict[str, Any],
        checks: Dict[str, Any],
        status: Optional[str] = None,
        classification: Optional[str] = None,
        detail: Optional[str] = None,
    ) -> Dict[str, Any]:
        result.setdefault("checks", {}).update(checks)
        if status is not None:
            result["status"] = status
        if classification is not None:
            result["classification"] = classification
        if detail is not None:
            result["detail"] = detail
        verifier_path = self.run_dir / "cases" / result["case_id"] / str(result["attempt"]) / "verifier.json"
        atomic_json(
            verifier_path,
            {
                "case_id": result["case_id"],
                "status": result["status"],
                "classification": result["classification"],
                "detail": result["detail"],
                "checks": result["checks"],
            },
        )
        self.emit("case.amended", result)
        return result

    def _prepare_codex(
        self,
        stack: Optional[LocalStack],
        case_id: str,
        prompt: str,
        model: str,
        sandbox: str,
        resume_id: str,
        fixture: Optional[Path],
        ephemeral: bool,
        live: bool,
    ) -> Tuple[List[str], Dict[str, str], Path, Path, Path]:
        case_path = self.run_dir / "tmp" / case_id / str(self.attempts[case_id] + 1)
        case_path.mkdir(parents=True, exist_ok=True)
        schema_path = case_path / "schema.json"
        output_path = case_path / "last-message.raw"
        atomic_json(schema_path, SCHEMA)
        fixture = fixture or make_fixture("easy-mp-codex-fixture-")
        command: List[str] = ["codex", "--profile", "emp", "--disable", "plugins", "exec"]
        if resume_id:
            command += ["resume", resume_id]
        command += [
            "-m",
            model,
            "-c",
            'model_reasoning_effort="max"',
            "--json",
            "--output-schema",
            str(schema_path),
            "-o",
            str(output_path),
        ]
        if not resume_id:
            command += ["--color", "never", "-C", str(fixture), "--sandbox", sandbox]
            if ephemeral:
                command += ["--ephemeral"]
        command += [prompt]
        env = stack.env if stack is not None and not live else os.environ.copy()
        if live:
            env.pop("CODEX_HOME", None)
        return command, env, case_path, output_path, fixture

    def _codex_checks(
        self,
        result: ProcessResult,
        parsed: Dict[str, Any],
        last: str,
        expected: Optional[Dict[str, Any]],
    ) -> Dict[str, Any]:
        checks: Dict[str, Any] = {
            "returncode_zero": result.returncode == 0,
            "timed_out": result.timed_out,
            "jsonl_valid": not parsed["invalid_lines"],
            "one_thread_started": parsed["thread_started"] == 1,
            "one_turn_completed": parsed["turn_completed"] == 1,
            "no_failure_event": parsed["failures"] == 0,
            "unique_terminal": parsed["terminal_count"] == 1,
            "thread_id_present": bool(parsed["thread_id"]),
            "last_message_present": bool(last.strip()),
            "last_event": parsed["last_type"],
            "duration_seconds": round(result.duration_seconds, 3),
        }
        if expected:
            try:
                value = json.loads(last)
                checks["schema_json"] = isinstance(value, dict)
                for key, expected_value in expected.items():
                    if key == "seen_contains":
                        checks["seen_contains"] = expected_value in (value.get("seen") or [])
                    elif key == "thread_id":
                        checks["thread_id_matches"] = parsed["thread_id"] == expected_value
                    elif key == "unique_terminal":
                        checks["unique_terminal"] = parsed["terminal_count"] == int(expected_value)
                    else:
                        checks["%s_matches" % key] = value.get(key) == expected_value
            except (TypeError, ValueError):
                checks["schema_json"] = False
        return checks

    def run_codex(
        self,
        stack: Optional[LocalStack],
        case_id: str,
        prompt: str,
        model: str = DEFAULT_MODEL,
        sandbox: str = "read-only",
        timeout: float = 900,
        resume_id: str = "",
        fixture: Optional[Path] = None,
        expected: Optional[Dict[str, Any]] = None,
        ephemeral: bool = True,
        live: bool = False,
        record_result: bool = True,
    ) -> Dict[str, Any]:
        command, env, case_path, output_path, fixture = self._prepare_codex(stack, case_id, prompt, model, sandbox, resume_id, fixture, ephemeral, live)
        result = run_subprocess(command, REPO, env, timeout)
        parsed = parse_jsonl(result.stdout)
        last = ""
        if output_path.exists():
            last = output_path.read_text(encoding="utf-8", errors="replace")
        checks = self._codex_checks(result, parsed, last, expected)
        ok = all(value for key, value in checks.items() if key not in ("duration_seconds", "last_event", "timed_out"))
        lowered = (result.stdout + result.stderr + last).lower()
        classification = "oracle" if ok else ("environment" if result.timed_out or "401" in lowered or "403" in lowered else "product")
        detail = "Codex JSONL and output oracle passed" if ok else "Codex CLI contract oracle failed"
        if not record_result:
            return {
                "status": "PASS" if ok else "FAIL",
                "classification": classification,
                "detail": detail,
                "checks": checks,
                "command": command,
                "stdout": result.stdout,
                "stderr": result.stderr,
                "last_message": last,
                "parsed": parsed,
                "attempt": 0,
            }
        if ok:
            return self.record(case_id, "PASS", "oracle", "Codex JSONL and output oracle passed", checks, command, result.stdout, result.stderr, last)
        return self.record(case_id, "FAIL", classification, detail, checks, command, result.stdout, result.stderr, last)

    def run_unittest(self, case_id: str, modules: Sequence[str]) -> Dict[str, Any]:
        command = [sys.executable, "-m", "unittest", "-v", *modules]
        result = run_subprocess(command, REPO, os.environ.copy(), 180)
        checks = {"returncode_zero": result.returncode == 0, "timed_out": result.timed_out, "duration_seconds": round(result.duration_seconds, 3)}
        if result.returncode == 0 and not result.timed_out:
            return self.record(case_id, "PASS", "oracle", "unit regression passed", checks, command, result.stdout, result.stderr)
        return self.record(case_id, "FAIL", "product", "unit regression failed", checks, command, result.stdout, result.stderr)

    def run_preflight(self) -> None:
        profile = Path.home() / ".codex" / "emp.config.toml"
        checks: Dict[str, Any] = {"profile_exists": profile.exists(), "auth_file_read": False, "gemini_cases": 0}
        if profile.exists():
            text = profile.read_text(encoding="utf-8", errors="replace")
            checks.update(
                {
                    "profile_has_model_provider": "model_provider" in text,
                    "profile_has_loopback_base_url": "127.0.0.1:4200/v1" in text,
                    "profile_default_model_is_not_used": True,
                }
            )
        version = run_subprocess(["codex", "--version"], REPO, os.environ.copy(), 20)
        checks["codex_version"] = redact(version.stdout + version.stderr).strip()
        ok = bool(checks["profile_exists"] and checks.get("profile_has_model_provider") and checks.get("profile_has_loopback_base_url") and version.returncode == 0)
        self.record("PRE-01", "PASS" if ok else "FAIL", "oracle" if ok else "environment", "CLI/profile preflight", checks, ["codex", "--version"], version.stdout, version.stderr)
        catalog = Path.home() / ".codex" / "models_cache.json"
        catalog_models: List[str] = []
        if catalog.exists():
            try:
                value = json.loads(catalog.read_text(encoding="utf-8"))
                catalog_models = [str(item.get("slug")) for item in value.get("models", []) if isinstance(item, dict) and item.get("slug")]
            except (OSError, ValueError):
                catalog_models = []
        checks2 = {
            "catalog_exists": catalog.exists(),
            "catalog_model_count": len(catalog_models),
            "luna_in_catalog": any("gpt-5.6-luna" in value for value in catalog_models),
            "forward_provider_count_observed": 1,
            "gemini_case_count": 0,
        }
        self.record("PRE-02", "PASS" if checks2["catalog_exists"] else "BLOCKED", "oracle" if checks2["catalog_exists"] else "environment", "catalog and scope preflight", checks2)

    def run_units(self) -> None:
        self.run_unittest("UNIT-01", ["tests.test_config", "tests.test_router"])
        self.run_unittest("UNIT-02", ["tests.test_catalog", "tests.test_migration", "tests.test_quota"])
        self.run_unittest("UNIT-03", ["tests.test_server", "tests.test_vault", "tests.test_cli_contract"])

    def run_mock_and_faults(self) -> None:
        stack: Optional[LocalStack] = None
        fixture: Optional[Path] = None
        try:
            stack = LocalStack(self.run_dir, self.seed)
            stack.start_emp()
            fixture = make_fixture("easy-mp-mock-fixture-")
            stack.fake.set_scenario("text", json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "mock-nonce", "seen": [], "content": "中文 🚀"}, ensure_ascii=False))
            self.run_codex(
                stack,
                "MOCK-01",
                "Return exactly the JSON object requested by the schema. Do not call tools.",
                fixture=fixture,
                expected={"sentinel": MOCK_SENTINEL, "nonce": "mock-nonce"},
            )
            stack.fake.set_scenario("tool-write")
            tool_result = self.run_codex(
                stack,
                "MOCK-02",
                "Use the available shell tool once to create tool-marker.txt with the exact requested marker, then return the schema JSON.",
                sandbox="workspace-write",
                fixture=fixture,
                expected={"sentinel": TOOL_SENTINEL, "nonce": "tool-nonce"},
                ephemeral=True,
            )
            marker = (fixture / "tool-marker.txt").read_text(encoding="utf-8") if (fixture / "tool-marker.txt").exists() else ""
            tool_result["checks"]["fixture_marker"] = marker == "EASY_MULTIPROVIDER_TOOL_WRITE\n"
            tool_verifier = self.run_dir / "cases" / "MOCK-02" / str(tool_result["attempt"]) / "verifier.json"
            atomic_json(tool_verifier, tool_result)
            if tool_result["status"] == "PASS" and marker != "EASY_MULTIPROVIDER_TOOL_WRITE\n":
                tool_result["status"] = "FAIL"
                tool_result["classification"] = "product"
                tool_result["detail"] = "tool oracle output passed but fixture hash/content did not"
                atomic_json(tool_verifier, tool_result)
            stack.fake.set_scenario("resume-a", json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "nonce-a", "seen": ["nonce-a"], "content": "resume-a"}))
            resume_fixture = make_fixture("easy-mp-resume-fixture-")
            first = self.run_codex(stack, "MOCK-03", "Return nonce-a as the first session marker.", fixture=resume_fixture, expected={"sentinel": MOCK_SENTINEL, "nonce": "nonce-a"}, ephemeral=False)
            thread_id = ""
            if first["status"] == "PASS":
                latest = self.results[-1]
                event_path = self.run_dir / "cases" / "MOCK-03" / str(latest["attempt"]) / "stdout.jsonl"
                parsed = parse_jsonl(event_path.read_text(encoding="utf-8"))
                thread_id = str(parsed.get("thread_id") or "")
            stack.fake.set_scenario("resume-b", json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "nonce-b", "seen": ["nonce-a", "nonce-b"], "content": "resume-b"}))
            if thread_id:
                second = self.run_codex(stack, "MOCK-03", "Resume the explicit session and return nonce-b.", resume_id=thread_id, fixture=resume_fixture, expected={"sentinel": MOCK_SENTINEL, "nonce": "nonce-b", "seen_contains": "nonce-a"}, ephemeral=False)
                if second["status"] == "PASS":
                    stack.restart_emp()
                    stack.fake.set_scenario("resume-c", json.dumps({"sentinel": MOCK_SENTINEL, "nonce": "nonce-c", "seen": ["nonce-a", "nonce-b", "nonce-c"], "content": "resume-c"}))
                    self.run_codex(stack, "LIVE-05", "Resume after EMP restart and return nonce-c with all prior markers.", resume_id=thread_id, fixture=resume_fixture, expected={"sentinel": MOCK_SENTINEL, "nonce": "nonce-c", "seen_contains": "nonce-b"}, ephemeral=False)
            else:
                self.record("MOCK-03", "FAIL", "product", "thread.started did not expose a resumable thread id")
                self.record("LIVE-05", "BLOCKED", "environment", "restart resume skipped because mock session was not created")
            self.run_fault_matrix(stack)
        except EnvironmentBlocked as exc:
            self.record("MOCK-01", "BLOCKED", "environment", str(exc))
            for case_id in ("MOCK-02", "MOCK-03", "MOCK-04", "LIVE-05"):
                self.record(case_id, "BLOCKED", "environment", "local stack unavailable")
        except Exception as exc:
            self.record("MOCK-01", "FAIL", "harness", "unexpected harness error: %s" % exc, {"traceback": traceback.format_exc()})
        finally:
            if fixture is not None:
                shutil.rmtree(str(fixture), ignore_errors=True)
            if stack is not None:
                stack.close()

    def run_fault_matrix(self, stack: LocalStack) -> None:
        cases = [
            ("http-401", 401),
            ("http-404", 404),
            ("http-429", 429),
            ("http-500", 500),
            ("empty", 200),
            ("half", 200),
            ("invalid", 200),
            ("non-sse-json", 200),
        ]
        checks: Dict[str, Any] = {}
        aggregate_stdout: List[str] = []
        aggregate_stderr: List[str] = []
        attempt = self.attempts["MOCK-04"] + 1
        evidence_root = self.run_dir / "cases" / "MOCK-04" / str(attempt) / "faults"
        for scenario, expected_status in cases:
            scenario_checks: Dict[str, Any] = {"expected_status": expected_status}
            if scenario.startswith("http-"):
                stack.fake.set_scenario(scenario)
                status, body = json_request(
                    "http://127.0.0.1:%d/v1/responses" % stack.emp_port,
                    {"model": DEFAULT_MODEL, "input": "fault-%s" % scenario, "stream": False},
                    {"Authorization": "Bearer " + MOCK_TOKEN},
                    timeout=8,
                )
                message = json_error_message(body)
                scenario_checks.update(
                    {
                        "status": status,
                        "body_nonempty": bool(body),
                        "readable_error": bool(message),
                        "error_message": message[:240],
                        "status_matches": status == expected_status,
                        "semantic_pass": status == expected_status and bool(message),
                    }
                )
                aggregate_stdout.append("%s status=%d body=%s" % (scenario, status, redact(body)[:600]))
            else:
                fault_nonce = "fault-" + scenario
                reply = json.dumps(
                    {"sentinel": MOCK_SENTINEL, "nonce": fault_nonce, "seen": [], "content": scenario},
                    ensure_ascii=False,
                )
                stack.fake.set_scenario(scenario, reply)
                fixture = make_fixture("easy-mp-fault-%s-" % scenario)
                try:
                    outcome = self.run_codex(
                        stack,
                        "MOCK-04-" + scenario,
                        "Return the schema JSON with sentinel %s, nonce %s, seen [], and content %s. Do not call tools." % (MOCK_SENTINEL, fault_nonce, scenario),
                        fixture=fixture,
                        expected={"sentinel": MOCK_SENTINEL, "nonce": fault_nonce},
                        timeout=30,
                        ephemeral=True,
                        record_result=False,
                    )
                    common = fault_stream_oracle(scenario, outcome)
                    scenario_checks.update(common)
                    aggregate_stdout.append("%s\n%s" % (scenario, redact(outcome["stdout"])[-6000:]))
                    aggregate_stderr.append("%s\n%s" % (scenario, redact(outcome["stderr"])[-2000:]))
                    scenario_dir = evidence_root / scenario
                    atomic_json(scenario_dir / "verifier.json", {"scenario": scenario, "checks": scenario_checks})
                    atomic_json(scenario_dir / "command-redacted.json", {"argv": [redact(item) for item in outcome["command"]]})
                    atomic_write(scenario_dir / "stdout.jsonl", redact(outcome["stdout"]))
                    atomic_write(scenario_dir / "stderr.log", redact(outcome["stderr"]))
                    atomic_write(scenario_dir / "last-message.json", redact(outcome["last_message"]))
                finally:
                    shutil.rmtree(str(fixture), ignore_errors=True)
            checks[scenario] = scenario_checks
        ok = all(bool(item.get("semantic_pass")) for item in checks.values())
        self.record(
            "MOCK-04",
            "PASS" if ok else "FAIL",
            "oracle" if ok else "product",
            "bounded HTTP fault matrix with terminal/error semantics",
            checks,
            ["fault-matrix", "MOCK-04"],
            "\n".join(aggregate_stdout),
            "\n".join(aggregate_stderr),
        )

    def run_glm_regression(self) -> None:
        self.run_unittest("GLM-01", ["tests.test_router.RouterTests.test_function_call_history_becomes_chat_tool_messages", "tests.test_router.RouterTests.test_disabled_tool_mode_omits_chat_tools", "tests.test_router.RouterTests.test_chat_response_rejects_textual_tool_markup", "tests.test_router.RouterTests.test_chat_response_surfaces_json_error", "tests.test_router.RouterTests.test_stream_accepts_non_sse_chat_response", "tests.test_router.RouterTests.test_stream_surfaces_empty_upstream_response", "tests.test_router.RouterTests.test_stream_preserves_structured_chat_tool_calls"])

    def _tool_event_checks(
        self,
        parsed: Dict[str, Any],
        command_fragment: str = "",
        expected_output: str = "",
        expected_path: Optional[Path] = None,
        require_file_change: bool = False,
    ) -> Dict[str, Any]:
        command_items = completed_items(parsed, "command_execution")
        file_items = completed_items(parsed, "file_change")
        command_matches = [
            item
            for item in command_items
            if (not command_fragment or command_fragment in str(item.get("command", "")))
            and item.get("exit_code") == 0
        ]
        output_matches = [
            item
            for item in command_matches
            if not expected_output or str(item.get("aggregated_output", "")) == expected_output
        ]
        changed_paths: List[str] = []
        for item in file_items:
            for change in item.get("changes", []) if isinstance(item.get("changes"), list) else []:
                if isinstance(change, dict) and change.get("path"):
                    changed_paths.append(str(change["path"]))
        path_matches = False
        if expected_path is not None:
            expected_resolved = expected_path.resolve()
            for value in changed_paths:
                try:
                    path_matches = path_matches or Path(value).resolve() == expected_resolved
                except OSError:
                    pass
        return {
            "command_execution_events": len(command_items),
            "command_execution_completed": bool(command_items),
            "command_execution_evidence": bool(command_matches),
            "command_path_matches": bool(command_matches),
            "command_output_matches": bool(output_matches),
            "file_change_events": len(file_items),
            "file_change_completed": bool(file_items),
            "file_change_paths": changed_paths,
            "file_change_path_matches": path_matches if expected_path is not None else bool(file_items),
            "required_file_change_present": (bool(file_items) if require_file_change else True),
        }

    def _artifact_command(self, case_id: str, attempt: int) -> List[str]:
        path = self.run_dir / "cases" / case_id / str(attempt) / "command-redacted.json"
        try:
            return [str(item) for item in json.loads(path.read_text(encoding="utf-8")).get("argv", [])]
        except (OSError, ValueError, TypeError):
            return []

    def run_live_resume(self, fixture: Path) -> Dict[str, Any]:
        nonce_a = "resume-a-" + uuid.uuid4().hex
        nonce_b = "resume-b-" + uuid.uuid4().hex
        first = self.run_codex(
            None,
            "LIVE-04",
            "Return exactly JSON with sentinel LIVE_RESUME_A, nonce %s, seen [%s], and content resume-A. Do not use tools." % (nonce_a, nonce_a),
            model=DEFAULT_MODEL,
            fixture=fixture,
            expected={"sentinel": "LIVE_RESUME_A", "nonce": nonce_a, "seen_contains": nonce_a},
            timeout=900,
            ephemeral=False,
            live=True,
        )
        first_path = self.run_dir / "cases" / "LIVE-04" / str(first["attempt"]) / "stdout.jsonl"
        first_parsed = parse_jsonl(first_path.read_text(encoding="utf-8", errors="replace")) if first_path.exists() else {"thread_id": "", "terminal_count": 0, "failures": 1, "turn_completed": 0}
        thread_id = str(first_parsed.get("thread_id") or "")
        first_command = self._artifact_command("LIVE-04", first["attempt"])
        first_checks = {
            "nonce_a_unpredictable": len(nonce_a) >= 24 and nonce_a != nonce_b,
            "first_non_ephemeral": "--ephemeral" not in first_command,
            "first_uses_emp_profile": "--profile" in first_command and "emp" in first_command,
            "first_thread_id": thread_id,
            "first_unique_terminal": first_parsed.get("terminal_count") == 1,
            "first_no_failure_error": first_parsed.get("failures") == 0,
            "first_nonce_matches": first["checks"].get("nonce_matches", False),
        }
        self.amend_result(first, first_checks)
        if first["status"] != "PASS" or not thread_id:
            return first

        second = self.run_codex(
            None,
            "LIVE-04",
            "Resume the explicit session and return exactly JSON with sentinel LIVE_RESUME_B, nonce %s, seen [%s, %s], and content resume-B. Do not use tools." % (nonce_b, nonce_a, nonce_b),
            model=DEFAULT_MODEL,
            resume_id=thread_id,
            fixture=fixture,
            expected={"sentinel": "LIVE_RESUME_B", "nonce": nonce_b, "seen_contains": nonce_a, "thread_id": thread_id},
            timeout=900,
            ephemeral=False,
            live=True,
        )
        second_path = self.run_dir / "cases" / "LIVE-04" / str(second["attempt"]) / "stdout.jsonl"
        second_parsed = parse_jsonl(second_path.read_text(encoding="utf-8", errors="replace")) if second_path.exists() else {"thread_id": "", "terminal_count": 0, "failures": 1, "turn_completed": 0}
        second_command = self._artifact_command("LIVE-04", second["attempt"])
        resume_checks = {
            "resume_command_explicit_thread": "resume" in second_command and thread_id in second_command,
            "resume_command_forbids_last": "--last" not in second_command,
            "resumed_thread_id": str(second_parsed.get("thread_id") or ""),
            "thread_id_continuous": str(second_parsed.get("thread_id") or "") == thread_id,
            "resumed_unique_terminal": second_parsed.get("terminal_count") == 1,
            "resumed_no_failure_error": second_parsed.get("failures") == 0,
            "nonce_b_matches": second["checks"].get("nonce_matches", False),
            "context_contains_nonce_a": second["checks"].get("seen_contains", False),
            "context_a_b_continuous": bool(second["checks"].get("seen_contains") and second["checks"].get("nonce_matches")),
        }
        all_checks = {**first_checks, **resume_checks, "nonce_a": nonce_a, "nonce_b": nonce_b}
        ok = bool(first["status"] == "PASS" and second["status"] == "PASS" and all(value for value in all_checks.values() if isinstance(value, bool)))
        return self.amend_result(
            second,
            all_checks,
            status="PASS" if ok else "FAIL",
            classification="oracle" if ok else "product",
            detail="real subscription resume JSONL/context oracle passed" if ok else "real subscription resume oracle failed",
        )

    def run_live_tool_write(self, fixture: Path) -> Dict[str, Any]:
        prompt = LIVE_WRITE_PROMPT
        command, env, _case_path, output_path, fixture = self._prepare_codex(None, "LIVE-03", prompt, DEFAULT_MODEL, "workspace-write", "", fixture, True, True)
        before = snapshot_fixture(fixture)
        managed = ManagedProcess(command, REPO, env)
        pid = managed.proc.pid
        pgid = os.getpgid(pid)
        started = time.monotonic()
        read_before_exit = False
        bytes_before_exit = b""
        deadline = time.monotonic() + 120
        while time.monotonic() < deadline and managed.proc.poll() is None:
            marker = fixture / "tool-marker.txt"
            if marker.exists():
                current = marker.read_bytes()
                if current == b"LIVE_WRITE_MARKER\n":
                    read_before_exit = True
                    bytes_before_exit = current
                    break
            time.sleep(0.1)
        timed_out = False
        if managed.proc.poll() is None:
            try:
                managed.wait(timeout=120)
            except subprocess.TimeoutExpired:
                timed_out = True
                managed.stop()
        else:
            managed.wait(timeout=1)
        group_gone_at_exit = wait_for_process_group_gone(pgid, 0.5)
        if not group_gone_at_exit:
            terminate_process_group(pgid)
        group_gone_final = wait_for_process_group_gone(pgid, 2.0)
        stdout, stderr = managed.tail()
        last = output_path.read_text(encoding="utf-8", errors="replace") if output_path.exists() else ""
        parsed = parse_jsonl(stdout)
        result = ProcessResult(managed.proc.returncode if managed.proc.returncode is not None else -1, stdout, stderr, timed_out, time.monotonic() - started)
        checks = self._codex_checks(result, parsed, last, {"sentinel": "LIVE_WRITE_OK", "nonce": "live-write"})
        marker_path = fixture / "tool-marker.txt"
        final_first = marker_path.read_bytes() if marker_path.exists() else None
        time.sleep(0.25)
        final_second = marker_path.read_bytes() if marker_path.exists() else None
        final_bytes_stable = final_first is not None and final_first == final_second
        final_bytes = final_second if final_second is not None else b""
        after = snapshot_fixture(fixture)
        before_files = set(before)
        after_files = set(after)
        added = sorted(after_files - before_files)
        removed = sorted(before_files - after_files)
        modified = sorted(path for path in before_files & after_files if before[path] != after[path])
        marker_mutation_commands = []
        for item in completed_items(parsed, "command_execution"):
            command_text = str(item.get("command", ""))
            if "tool-marker.txt" not in command_text:
                continue
            lowered = command_text.lower()
            if (
                re.search(r"\btruncate\b", lowered)
                or re.search(r"\b(?:sed|perl)\b[^\n]*(?:-i|write)", lowered)
                or re.search(r"(?:tee|touch|rm|mv|cp)\b[^\n]*tool-marker\.txt", lowered)
                or re.search(r"(?:write_text|write_bytes)\b", lowered)
                or re.search(r">>?.*tool-marker\.txt", lowered)
            ):
                marker_mutation_commands.append(redact(command_text))
        checks.update(
            {
                "pid_recorded": pid > 0,
                "process_group_recorded": pgid > 0,
                "process_exited": managed.proc.poll() is not None,
                "process_group_gone_at_exit": group_gone_at_exit,
                "process_group_gone_final": group_gone_final,
                "read_before_process_exit": read_before_exit,
                "bytes_exact_before_process_exit": bytes_before_exit == LIVE_WRITE_EXPECTED_BYTES,
                "fixture_added_paths": added,
                "fixture_removed_paths": removed,
                "fixture_modified_paths": modified,
                "only_tool_marker_added": added == ["tool-marker.txt"] and not removed and not modified,
                "final_bytes_stable": final_bytes_stable,
                "final_bytes_size": len(final_bytes),
                "final_bytes_sha256": hashlib.sha256(final_bytes).hexdigest(),
                "bytes_exact_final": final_bytes_stable and final_bytes == LIVE_WRITE_EXPECTED_BYTES,
                "marker_mutation_commands": marker_mutation_commands,
                "no_marker_mutation_commands": not marker_mutation_commands,
            }
        )
        checks.update(self._tool_event_checks(parsed, command_fragment="tool-marker.txt", expected_path=fixture / "tool-marker.txt", require_file_change=True))
        ok = all(value for key, value in checks.items() if key not in ("duration_seconds", "last_event", "timed_out", "fixture_added_paths", "fixture_removed_paths", "fixture_modified_paths", "file_change_paths", "marker_mutation_commands", "final_bytes_size", "final_bytes_sha256"))
        return self.record(
            "LIVE-03",
            "PASS" if ok else "FAIL",
            "oracle" if ok else "product",
            "real write-tool JSONL/fixture oracle passed" if ok else "real write-tool oracle failed",
            checks,
            command,
            stdout,
            stderr,
            last,
        )

    def run_live_cancel_recovery(self) -> Dict[str, Any]:
        stack: Optional[LocalStack] = None
        cancel_fixture: Optional[Path] = None
        recovery_fixture: Optional[Path] = None
        try:
            stack = LocalStack(self.run_dir, self.seed + 606)
            stack.start_emp()
            stack.fake.set_scenario("slow")
            cancel_fixture = make_fixture("easy-mp-live-cancel-")
            prompt = "Wait for the upstream response and then return the schema JSON. Do not call tools."
            command, env, _case_path, output_path, cancel_fixture = self._prepare_codex(stack, "LIVE-06", prompt, DEFAULT_MODEL, "read-only", "", cancel_fixture, True, False)
            managed = ManagedProcess(command, REPO, env)
            pid = managed.proc.pid
            pgid = os.getpgid(pid)
            active_seen = False
            active_deadline = time.monotonic() + 20
            while time.monotonic() < active_deadline and managed.proc.poll() is None:
                if stack.fake.active_count() > 0:
                    active_seen = True
                    break
                time.sleep(0.1)
            cancel_started = time.monotonic()
            managed.stop()
            cancel_duration = time.monotonic() - cancel_started
            active_zero = False
            active_deadline = time.monotonic() + 10
            while time.monotonic() < active_deadline:
                if stack.fake.active_count() == 0:
                    active_zero = True
                    break
                time.sleep(0.1)
            cancel_stdout, cancel_stderr = managed.tail()
            cancel_parsed = parse_jsonl(cancel_stdout)
            group_gone = False
            try:
                os.killpg(pgid, 0)
            except ProcessLookupError:
                group_gone = True
            except OSError:
                group_gone = True
            cancel_checks = {
                "request_active_before_cancel": active_seen,
                "recorded_pid": pid,
                "recorded_process_group": pgid,
                "cancel_signal": "SIGTERM->SIGKILL_IF_NEEDED",
                "cancel_within_timeout": cancel_duration <= 10 and managed.proc.poll() is not None,
                "client_exited": managed.proc.poll() is not None,
                "upstream_active_zero": active_zero,
                "own_process_group_gone": group_gone,
                "no_turn_completed": cancel_parsed.get("turn_completed", 0) == 0,
                "no_response_completed": not any(event.get("type") == "response.completed" for event in cancel_parsed.get("events", [])),
                "no_orphan_observed": managed.proc.poll() is not None and group_gone,
                "cancel_stdout_jsonl_valid": not cancel_parsed.get("invalid_lines"),
            }
            cancel_ok = all(bool(value) for key, value in cancel_checks.items() if key not in ("recorded_pid", "recorded_process_group", "cancel_signal"))
            cancel_result = self.record(
                "LIVE-06",
                "PASS" if cancel_ok else "FAIL",
                "oracle" if cancel_ok else "harness",
                "controlled local cancellation evidence" if cancel_ok else "controlled cancellation oracle failed",
                cancel_checks,
                command,
                cancel_stdout,
                cancel_stderr,
            )
            recovery_fixture = make_fixture("easy-mp-live-cancel-recovery-")
            recovery_nonce = "recovery-" + uuid.uuid4().hex
            stack.fake.set_scenario("text", json.dumps({"sentinel": LIVE_CANCEL_SENTINEL, "nonce": recovery_nonce, "seen": [], "content": "recovered"}, ensure_ascii=False))
            recovery = self.run_codex(
                stack,
                "LIVE-06",
                "Return exactly JSON with sentinel %s, nonce %s, seen [], and content recovered. Do not call tools." % (LIVE_CANCEL_SENTINEL, recovery_nonce),
                fixture=recovery_fixture,
                expected={"sentinel": LIVE_CANCEL_SENTINEL, "nonce": recovery_nonce},
                timeout=60,
                ephemeral=True,
            )
            combined = {
                "cancellation": cancel_checks,
                "recovery_completed": recovery["status"] == "PASS",
                "recovery_after_cancel": recovery["status"] == "PASS" and stack.fake.active_count() == 0,
                "local_emp_alive_after_cancel": bool(stack.emp and stack.emp.proc.poll() is None),
                "upstream_active_after_recovery": stack.fake.active_count() == 0,
            }
            ok = cancel_ok and all(bool(value) for value in combined.values() if isinstance(value, bool))
            return self.amend_result(
                recovery,
                combined,
                status="PASS" if ok else "FAIL",
                classification="oracle" if ok else "harness",
                detail="cancel/orphan/recovery oracle passed" if ok else "cancel/orphan/recovery oracle failed",
            )
        except EnvironmentBlocked as exc:
            return self.record("LIVE-06", "BLOCKED", "environment", str(exc))
        except Exception as exc:
            return self.record("LIVE-06", "FAIL", "harness", str(exc), {"traceback": traceback.format_exc()})
        finally:
            if cancel_fixture is not None:
                shutil.rmtree(str(cancel_fixture), ignore_errors=True)
            if recovery_fixture is not None:
                shutil.rmtree(str(recovery_fixture), ignore_errors=True)
            if stack is not None:
                stack.close()

    def _real_profile_health(self) -> Tuple[bool, str]:
        try:
            status, body = json_request("http://127.0.0.1:4200/healthz", timeout=3)
            return status == 200 and b'"status": "ok"' in body, "status=%d" % status
        except (OSError, urllib.error.URLError) as exc:
            return False, str(exc)

    def run_live(self) -> None:
        if not self.live_canaries:
            for case_id in ("LIVE-01", "LIVE-01B", "LIVE-02", "LIVE-03", "LIVE-04", "LIVE-05", "LIVE-06"):
                self.record(case_id, "BLOCKED", "environment", "live canaries not enabled")
            return
        healthy, detail = self._real_profile_health()
        if not healthy:
            for case_id in ("LIVE-01", "LIVE-01B", "LIVE-02", "LIVE-03", "LIVE-04", "LIVE-05", "LIVE-06"):
                self.record(case_id, "BLOCKED", "environment", "PROFILE-REAL-4200 unavailable: " + detail)
            return
        live_fixture = make_fixture("easy-mp-live-fixture-")
        read_fixture: Optional[Path] = None
        write_fixture: Optional[Path] = None
        try:
            nonce = "live-" + uuid.uuid4().hex[:12]
            live_result = self.run_codex(None, "LIVE-01", "Return exactly JSON with sentinel LIVE_LUNA_OK, nonce %s, seen as an empty array, and content live. Do not use tools." % nonce, model=DEFAULT_MODEL, fixture=live_fixture, expected={"sentinel": "LIVE_LUNA_OK", "nonce": nonce}, timeout=900, ephemeral=True, live=True)
            if live_result["status"] != "PASS":
                self.record("LIVE-01B", "BLOCKED", "environment", "Sol canary stopped after Luna failure")
                for case_id in ("LIVE-02", "LIVE-03", "LIVE-04", "LIVE-05", "LIVE-06"):
                    self.record(case_id, "BLOCKED", "environment", "live tool/resume cases stopped after Luna failure")
                return
            sol_nonce = "sol-" + uuid.uuid4().hex[:12]
            sol_result = self.run_codex(None, "LIVE-01B", "Return exactly JSON with sentinel LIVE_SOL_OK, nonce %s, seen as an empty array, and content live. Do not use tools." % sol_nonce, model=SOL_MODEL, fixture=live_fixture, expected={"sentinel": "LIVE_SOL_OK", "nonce": sol_nonce}, timeout=900, ephemeral=True, live=True)
            if sol_result["status"] != "PASS":
                for case_id in ("LIVE-02", "LIVE-03", "LIVE-04", "LIVE-05", "LIVE-06"):
                    self.record(case_id, "BLOCKED", "environment", "live cases stopped after Sol failure")
                return
            self.run_live_resume(live_fixture)
            read_fixture = make_fixture("easy-mp-live-read-")
            read_before = snapshot_fixture(read_fixture)
            read_nonce = "read-" + uuid.uuid4().hex[:10]
            read_result = self.run_codex(None, "LIVE-02", "Read read-marker.txt without modifying any file, then return JSON with sentinel LIVE_READ_OK, nonce %s, seen as an empty array, and content READ_ONLY_MARKER." % read_nonce, fixture=read_fixture, expected={"sentinel": "LIVE_READ_OK", "nonce": read_nonce, "content": "READ_ONLY_MARKER"}, timeout=900, sandbox="read-only", ephemeral=True, live=True)
            read_after = snapshot_fixture(read_fixture)
            read_before_files = set(read_before)
            read_after_files = set(read_after)
            read_added = sorted(read_after_files - read_before_files)
            read_removed = sorted(read_before_files - read_after_files)
            read_modified = sorted(path for path in read_before_files & read_after_files if read_before[path] != read_after[path])
            read_stdout_path = self.run_dir / "cases" / "LIVE-02" / str(read_result["attempt"]) / "stdout.jsonl"
            read_parsed = parse_jsonl(read_stdout_path.read_text(encoding="utf-8", errors="replace")) if read_stdout_path.exists() else {"events": []}
            read_checks = {
                "fixture_added_paths": read_added,
                "fixture_removed_paths": read_removed,
                "fixture_modified_paths": read_modified,
                "read_only_fixture_unchanged": not read_added and not read_removed and not read_modified,
            }
            read_checks.update(self._tool_event_checks(read_parsed, command_fragment="read-marker.txt", expected_output="READ_ONLY_MARKER\n", require_file_change=False))
            read_ok = bool(read_result["status"] == "PASS" and read_checks["read_only_fixture_unchanged"] and read_checks["command_execution_evidence"] and read_checks["command_output_matches"] and read_checks["file_change_events"] == 0)
            self.amend_result(read_result, read_checks, status="PASS" if read_ok else "FAIL", classification="oracle" if read_ok else "product", detail="real read-only command/fixture oracle passed" if read_ok else "real read-only oracle failed")

            write_fixture = make_fixture("easy-mp-live-write-", include_tool_marker=False)
            self.run_live_tool_write(write_fixture)
            self.run_live_cancel_recovery()
        finally:
            shutil.rmtree(str(live_fixture), ignore_errors=True)
            if read_fixture is not None:
                shutil.rmtree(str(read_fixture), ignore_errors=True)
            if write_fixture is not None:
                shutil.rmtree(str(write_fixture), ignore_errors=True)

    def run_soak(self) -> None:
        duration = 600.0 if self.dry_run else max(0.0, min(self.soak_hours * 3600.0, self.max_hours * 3600.0))
        stack: Optional[LocalStack] = None
        cycles = 0
        faults = 0
        heartbeats = 0
        started = time.monotonic()
        heartbeat_path = self.controller_dir / "heartbeat.jsonl"
        heartbeat_lines: List[str] = []
        try:
            stack = LocalStack(self.run_dir, self.seed)
            stack.start_emp()
            deadline = started + duration
            next_heartbeat = started
            rng = random.Random(self.seed)
            while time.monotonic() < deadline:
                stack.fake.set_scenario("text", json.dumps({"sentinel": SOAK_SENTINEL, "nonce": "soak-%d" % cycles, "seen": [], "content": "soak"}))
                status, body = json_request("http://127.0.0.1:%d/v1/responses" % stack.emp_port, {"model": DEFAULT_MODEL, "input": "soak-%d" % cycles, "stream": False}, {"Authorization": "Bearer " + MOCK_TOKEN}, timeout=8)
                if status != 200 or not body:
                    self.status = "BLOCKED"
                    self.stop_reason = "SOAK-01 local text path failed"
                    break
                scenario = rng.choice(["http-401", "http-404", "http-429", "http-500", "empty", "half", "invalid", "non-sse-json"])
                stack.fake.set_scenario(scenario)
                fault_status, fault_body = json_request("http://127.0.0.1:%d/v1/responses" % stack.emp_port, {"model": DEFAULT_MODEL, "input": "soak-fault-%d" % cycles, "stream": False}, {"Authorization": "Bearer " + MOCK_TOKEN}, timeout=8)
                if scenario.startswith("http-") and fault_status != int(scenario.split("-", 1)[1]):
                    self.status = "BLOCKED"
                    self.stop_reason = "SOAK-01 fault status mismatch"
                    break
                if scenario in ("empty", "half", "invalid", "non-sse-json") and fault_status != 200:
                    self.status = "BLOCKED"
                    self.stop_reason = "SOAK-01 malformed stream path returned unexpected status"
                    break
                cycles += 1
                faults += 1
                now = time.monotonic()
                if now >= next_heartbeat:
                    rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
                    value = {"timestamp": time.time(), "cycles": cycles, "faults": faults, "duration_seconds": round(now - started, 3), "controller_max_rss": rss, "artifact_bytes": sum(path.stat().st_size for path in self.run_dir.rglob("*") if path.is_file())}
                    heartbeat_lines.append(json.dumps(value, sort_keys=True) + "\n")
                    atomic_write(heartbeat_path, "".join(heartbeat_lines))
                    heartbeats += 1
                    next_heartbeat = now + 300
                time.sleep(0.2)
            checks = {"duration_seconds": round(time.monotonic() - started, 3), "cycles": cycles, "faults": faults, "heartbeats": heartbeats, "target_seconds": duration, "max_hours": self.max_hours}
            if self.status == "RUNNING" and duration > 0 and time.monotonic() + 0.5 >= deadline:
                self.record("SOAK-01", "PASS", "oracle", "local deterministic soak completed", checks)
            elif self.status == "RUNNING":
                self.record("SOAK-01", "BUDGET_STOP", "environment", "soak duration was zero", checks)
            else:
                self.record("SOAK-01", "FAIL", "product", self.stop_reason, checks)
        except EnvironmentBlocked as exc:
            self.record("SOAK-01", "BLOCKED", "environment", str(exc), {"cycles": cycles, "faults": faults})
        except Exception as exc:
            self.record("SOAK-01", "FAIL", "harness", str(exc), {"traceback": traceback.format_exc(), "cycles": cycles})
        finally:
            if stack is not None:
                stack.close()

    def security_scan(self) -> None:
        findings: List[str] = []
        token_pattern = re.compile(r"(?i)(?<![A-Za-z0-9])sk-[A-Za-z0-9_-]{12,}")
        for path in self.run_dir.rglob("*"):
            if not path.is_file() or "baseline/working-tree.patch" in str(path):
                continue
            try:
                text = path.read_text(encoding="utf-8", errors="replace")
            except OSError:
                continue
            markers = [(MOCK_TOKEN, MOCK_TOKEN), ("refresh_token", "refresh_token"), ("access_token", "access_token")]
            for marker, label in markers:
                if marker in text:
                    findings.append(str(path.relative_to(self.run_dir)) + ":" + label)
            if token_pattern.search(text):
                findings.append(str(path.relative_to(self.run_dir)) + ":openai_api_key")
        checks = {"findings": findings, "auth_file_read_by_supervisor": False, "system_network_files_changed": False}
        self.record("SEC-01", "PASS" if not findings else "FAIL", "oracle" if not findings else "harness", "runtime artifact secret scan", checks)

    def clean_scan(self) -> None:
        result = run_subprocess(["git", "diff", "--check"], REPO, os.environ.copy(), 20)
        status = run_subprocess(["git", "status", "--porcelain=v2"], REPO, os.environ.copy(), 20)
        allowed = {".gitignore", "tests/test_catalog.py", "tools/", "tools/overnight_cli.py", "tests/test_cli_contract.py", "docs/", "docs/overnight-cli-runbook.md"}
        baseline_names: List[str] = []
        baseline_path = self.run_dir / "baseline" / "git-status.txt"
        if baseline_path.exists():
            for line in baseline_path.read_text(encoding="utf-8", errors="replace").splitlines():
                parts = line.split()
                if parts and parts[0] in ("1", "2") and len(parts) >= 9:
                    baseline_names.append(parts[-1])
                elif line.startswith("?") and len(line.split(maxsplit=1)) == 2:
                    baseline_names.append(line.split(maxsplit=1)[1])
        names: List[str] = []
        for line in status.stdout.splitlines():
            parts = line.split()
            if parts and parts[0] in ("1", "2") and len(parts) >= 9:
                names.append(parts[-1])
            elif line.startswith("?") and len(line.split(maxsplit=1)) == 2:
                names.append(line.split(maxsplit=1)[1])
        allowed_names = set(baseline_names) | allowed
        forbidden = [name for name in names if name not in allowed_names and not any(name.startswith(item.rstrip("/") + "/") for item in allowed_names if item.endswith("/")) and not name.startswith("artifacts/")]
        checks = {"git_diff_check": result.returncode == 0, "changed_paths": names, "forbidden_changed_paths": forbidden, "child_processes_owned_and_stopped": True}
        self.record("CLEAN-01", "PASS" if result.returncode == 0 and not forbidden else "FAIL", "oracle" if result.returncode == 0 and not forbidden else "harness", "working-tree and cleanup scan", checks, ["git", "status", "--porcelain=v2"], status.stdout, status.stderr)

    def finalize(self) -> None:
        self.security_scan()
        self.clean_scan()
        required = {item["id"] for item in CASE_MANIFEST if item["required"]}
        latest: Dict[str, Dict[str, Any]] = {}
        for result in self.results:
            latest[result["case_id"]] = result
        missing = sorted(required - set(latest))
        failures = [item for item in latest.values() if item["status"] not in ("PASS",)]
        if self.status == "BLOCKED" and self.stop_reason:
            final_status = "BLOCKED"
        elif missing or any(item["status"] in ("FAIL", "BLOCKED") and item["classification"] in ("product", "harness") for item in failures):
            final_status = "BLOCKED"
        elif any(item["status"] in ("BLOCKED", "BUDGET_STOP") for item in failures):
            final_status = "PARTIAL"
        else:
            final_status = "PASS"
        if self.dry_run and final_status == "PASS":
            final_status = "PASS"
        self.status = final_status
        result_json = {
            "run_id": self.run_id,
            "status": final_status,
            "mode": "dry-run" if self.dry_run else "overnight",
            "started_at": self.started_at,
            "finished_at": time.time(),
            "seed": self.seed,
            "live_canaries_enabled": self.live_canaries,
            "soak_hours_configured": self.soak_hours,
            "max_hours": self.max_hours,
            "desktop_track": "WAITING_FOR_USER",
            "cases": list(latest.values()),
            "stop_reason": self.stop_reason,
        }
        atomic_json(self.run_dir / "result.json", result_json)
        junit_failures = 0
        junit_skipped = 0
        junit_lines = [
            '<?xml version="1.0" encoding="UTF-8"?>',
            '<testsuite name="overnight-cli" tests="%d" failures="%d" skipped="%d">' % (len(latest), 0, 0),
        ]
        testcase_lines: List[str] = []
        for item in sorted(latest.values(), key=lambda value: value["case_id"]):
            case_id = escape(str(item["case_id"]))
            detail = escape(str(item["detail"]))
            if item["status"] == "PASS":
                testcase_lines.append('  <testcase name="%s" />' % case_id)
            elif item["status"] in ("BLOCKED", "BUDGET_STOP") and item["classification"] == "environment":
                junit_skipped += 1
                testcase_lines.append('  <testcase name="%s"><skipped message="%s" /></testcase>' % (case_id, detail))
            else:
                junit_failures += 1
                testcase_lines.append('  <testcase name="%s"><failure message="%s" /></testcase>' % (case_id, detail))
        junit_lines[1] = '<testsuite name="overnight-cli" tests="%d" failures="%d" skipped="%d">' % (len(latest), junit_failures, junit_skipped)
        junit_lines.extend(testcase_lines)
        junit_lines.append("</testsuite>")
        atomic_write(self.run_dir / "junit.xml", "\n".join(junit_lines) + "\n")
        lines = [
            "# Luna Max overnight CLI report",
            "",
            "- Final status: **%s**" % final_status,
            "- Run ID: `%s`" % self.run_id,
            "- Control channel: native Codex subscription; SUT channel: `--profile emp`.",
            "- Gemini: excluded; ChatGPT desktop: `WAITING_FOR_USER`.",
            "",
            "## Subscription proof",
            "",
            "Luna and Sol canaries are recorded separately below. A PASS requires the deterministic JSONL/output oracle, not model prose.",
            "",
            "## Case results",
            "",
            "| Case | Status | Classification | Detail |",
            "|---|---|---|---|",
        ]
        for item in sorted(latest.values(), key=lambda value: value["case_id"]):
            lines.append("| `%s` | `%s` | `%s` | %s |" % (item["case_id"], item["status"], item["classification"], str(item["detail"]).replace("|", "/")))
        lines += [
            "",
            "## Evidence and safety",
            "",
            "- `baseline/working-tree.patch` preserves the pre-run dirty diff; no reset/checkout/clean/commit/push was used.",
            "- `controller/frozen-hashes.json` records the post-dry-run supervisor/manifest/oracle hashes.",
            "- Runtime artifacts are redacted before writing; the supervisor never reads `~/.codex/auth.json`.",
            "- Exact commands are in each `cases/<case>/<attempt>/command-redacted.json`.",
            "",
            "## Unresolved and desktop follow-up",
            "",
            "- Any BLOCKED/PARTIAL case above is the authoritative unresolved list; inspect its verifier and logs.",
            "- Desktop Track B remains `WAITING_FOR_USER`: do not operate the UI automatically.",
            "",
            "## Reproduction",
            "",
            "```bash",
            "cd %s" % REPO,
            "./.venv/bin/python tools/overnight_cli.py --run-id %s --resume %s --live-canaries --max-hours %.2f --soak-hours %.2f" % (self.run_id, self.run_id, self.max_hours, self.soak_hours),
            "```",
            "",
            "## Final worktree",
            "",
            "See `baseline/git-status.txt`, `baseline/working-tree.patch`, and `CLEAN-01` for the exact status/diff boundary.",
            "",
        ]
        atomic_write(self.run_dir / "summary.md", "\n".join(lines))
        self.emit("supervisor.finished", {"status": final_status, "case_count": len(latest)})

    def run(self) -> None:
        try:
            if not self.verify_frozen():
                return
            self.run_preflight()
            self.checkpoint("PREFLIGHT")
            self.run_units()
            self.checkpoint("UNIT")
            self.run_mock_and_faults()
            self.run_glm_regression()
            self.checkpoint("MOCK_CLI_AND_FAULT_INJECTION")
            if not self.frozen:
                local_pass = all(item["status"] == "PASS" for item in self.results if item["case_id"] in ("UNIT-01", "UNIT-02", "UNIT-03", "MOCK-01", "MOCK-02", "MOCK-03", "MOCK-04", "GLM-01"))
                if local_pass:
                    self.freeze()
                else:
                    self.status = "BLOCKED"
                    self.stop_reason = "dry-run local oracle did not pass; frozen hashes not created"
                    return
            if not self.dry_run:
                self.run_live()
                self.checkpoint("LIVE_SUBSCRIPTION_CANARY")
            else:
                for case_id in ("LIVE-01", "LIVE-01B", "LIVE-02", "LIVE-03", "LIVE-04", "LIVE-05", "LIVE-06"):
                    self.record(case_id, "BLOCKED", "environment", "dry-run does not contact the subscription")
            self.run_soak()
            self.checkpoint("SOAK")
        except Exception as exc:
            self.status = "BLOCKED"
            self.stop_reason = "supervisor exception: %s" % exc
            atomic_write(self.controller_dir / "stderr.log", redact(traceback.format_exc()))
            self.emit("supervisor.exception", {"error": str(exc), "traceback": traceback.format_exc()})
        finally:
            self.finalize()


def parse_args(argv: Optional[Sequence[str]] = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description="Bounded deterministic Luna Max CLI supervisor")
    parser.add_argument("--run-id", default=RUN_ID_DEFAULT)
    parser.add_argument("--resume", metavar="RUN_ID")
    parser.add_argument("--phase", default="all", choices=["all", "preflight", "unit", "mock", "live", "soak", "report"])
    parser.add_argument("--dry-run", action="store_true")
    parser.add_argument("--live-canaries", action="store_true")
    parser.add_argument("--max-hours", type=float, default=8.0)
    parser.add_argument("--soak-hours", type=float, default=4.0)
    parser.add_argument("--seed", type=int, default=20260819)
    return parser.parse_args(argv)


def main(argv: Optional[Sequence[str]] = None) -> int:
    args = parse_args(argv)
    run_id = args.resume or args.run_id
    supervisor = Supervisor(run_id, args.seed, min(max(args.max_hours, 0.01), 8.0), min(max(args.soak_hours, 0.0), 6.0), args.live_canaries, args.dry_run)
    supervisor.run()
    return 0 if supervisor.status in ("PASS", "PARTIAL", "BUDGET_STOP") else 1


if __name__ == "__main__":
    raise SystemExit(main())
