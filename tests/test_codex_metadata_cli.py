"""Opt-in official CLI acceptance of native control metadata and tool followups."""
import copy
import gzip
from contextlib import nullcontext
import json
import os
from pathlib import Path
import queue
import subprocess
import tempfile
import threading
import time
import unittest
import zlib
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch

from easy_multi_provider.catalog import write_catalog
from easy_multi_provider.config import normalize, save
from easy_multi_provider.integration import IntegrationManager
from easy_multi_provider.server import AppState, make_handler
from tests.support import ensure_test_master_key
from tests.rust_e2e_support import EmpProcess
from tests.test_codex_cli_demo import FIXED_REPLY, _fixed_response_stream, _tool_response_stream


TURN_STATE = "fixture-native-turn-state"


def _decode_request(raw, encoding):
    for name in reversed(encoding.split(",")):
        name = name.strip().lower()
        if name == "zstd":
            import zstandard
            raw = zstandard.ZstdDecompressor().decompress(raw)
        elif name == "gzip":
            raw = gzip.decompress(raw)
        elif name == "deflate":
            raw = zlib.decompress(raw)
        elif name != "identity":
            raise AssertionError("unexpected fixture request encoding")
    return json.loads(raw)


def _find_command_tool(tools, namespace=None):
    for tool in tools:
        if not isinstance(tool, dict):
            continue
        if tool.get("type") == "namespace":
            found = _find_command_tool(tool.get("tools", []), tool.get("name"))
            if found:
                return found
        elif tool.get("type") == "function" and tool.get("name") in (
            "exec_command", "shell_command", "exec",
        ):
            return tool["name"], namespace, "function"
        elif tool.get("type") == "custom" and tool.get("name") == "exec":
            return tool["name"], namespace, "custom"
    return None


def _native_tool_stream(model, name, namespace, kind):
    payload = _tool_response_stream(model, name)
    if not namespace and kind == "function":
        return payload
    events = []
    for line in payload.decode().splitlines():
        if not line.startswith("data: "):
            continue
        event = json.loads(line[6:])
        items = [event.get("item")] + event.get("response", {}).get("output", [])
        for item in items:
            if isinstance(item, dict) and item.get("type") == "function_call":
                if namespace:
                    item["namespace"] = namespace
                if kind == "custom":
                    item["type"] = "custom_tool_call"
                    arguments = item.pop("arguments", "")
                    item["input"] = json.loads(arguments)["input"] if arguments else ""
        if kind == "custom":
            event["type"] = event["type"].replace("function_call_arguments", "custom_tool_call_input")
            if "delta" in event:
                event["delta"] = json.loads(event["delta"])["input"]
            if "arguments" in event:
                event["input"] = json.loads(event.pop("arguments"))["input"]
        events.append("data: " + json.dumps(event) + "\n\n")
    return "".join(events).encode()


def _server_search_stream(payload):
    items = [
        {"type": "tool_search_call", "id": "fixture-server-search-item",
         "call_id": "fixture-server-search-call", "execution": "server",
         "arguments": {"query": "fixture"}, "status": "completed"},
        {"type": "tool_search_output", "id": "fixture-server-search-output",
         "call_id": "fixture-server-search-call", "execution": "server",
         "status": "completed", "tools": []},
    ]
    events = []
    for line in payload.decode().splitlines():
        if not line.startswith("data: "):
            continue
        event = json.loads(line[6:])
        if isinstance(event.get("output_index"), int):
            event["output_index"] += len(items)
        if event.get("type") == "response.completed":
            event["response"]["output"] = items + event["response"]["output"]
        events.append(event)
        if event.get("type") == "response.created":
            for index, item in enumerate(items):
                for kind in ("response.output_item.added", "response.output_item.done"):
                    events.append({"type": kind, "output_index": index, "item": item})
    return "".join("data: " + json.dumps(event) + "\n\n" for event in events).encode()


class _CodexAppServerStdio:
    """Small JSON-RPC client for the official app-server stdio transport."""

    def __init__(self, command, environment, cwd):
        self.process = subprocess.Popen(
            command,
            cwd=cwd,
            env=environment,
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )
        self.lines = queue.Queue()
        self.stderr_lines = []
        self.messages = []
        self.request_id = 0
        self.stdout_reader = threading.Thread(target=self._read_stdout, daemon=True)
        self.stderr_reader = threading.Thread(target=self._read_stderr, daemon=True)
        self.stdout_reader.start()
        self.stderr_reader.start()

    def _read_stdout(self):
        for line in self.process.stdout:
            self.lines.put(line)
        self.lines.put(None)

    def _read_stderr(self):
        self.stderr_lines.extend(self.process.stderr)

    def _receive(self, timeout):
        try:
            line = self.lines.get(timeout=timeout)
        except queue.Empty:
            raise AssertionError("Codex app-server response timed out: " + "".join(self.stderr_lines[-40:])) from None
        if line is None:
            raise AssertionError(
                "Codex app-server exited early: " + "".join(self.stderr_lines[-40:])
            )
        try:
            return json.loads(line)
        except ValueError:
            raise AssertionError(
                "Codex app-server emitted a non-JSON stdout line: " + line[:1000]
            ) from None

    def notify(self, method, params=None):
        message = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self.process.stdin.write(json.dumps(message, separators=(",", ":")) + "\n")
        self.process.stdin.flush()

    def request(self, method, params):
        self.request_id += 1
        request_id = self.request_id
        self.process.stdin.write(
            json.dumps(
                {"jsonrpc": "2.0", "id": request_id, "method": method, "params": params},
                separators=(",", ":"),
            )
            + "\n"
        )
        self.process.stdin.flush()
        deadline = time.monotonic() + 60
        while True:
            message = self._receive(max(0.01, deadline - time.monotonic()))
            if message.get("id") == request_id:
                if "error" in message:
                    raise AssertionError(
                        f"Codex app-server {method} failed: {message['error']}"
                    )
                return message.get("result", {})
            self.messages.append(message)

    @staticmethod
    def _terminal(message, thread_id, turn_id):
        if message.get("method") not in {"turn/completed", "turn/failed"}:
            return False
        params = message.get("params", {})
        turn = params.get("turn", {})
        return params.get("threadId") == thread_id and turn.get("id") == turn_id

    def wait_for_turn(self, thread_id, turn_id):
        deadline = time.monotonic() + 90
        for index, message in enumerate(self.messages):
            if self._terminal(message, thread_id, turn_id):
                return self.messages.pop(index)["params"]["turn"]
        while True:
            message = self._receive(max(0.01, deadline - time.monotonic()))
            if self._terminal(message, thread_id, turn_id):
                return message["params"]["turn"]
            self.messages.append(message)

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.stdout_reader.join(timeout=2)
        self.stderr_reader.join(timeout=2)
        for stream in (self.process.stdin, self.process.stdout, self.process.stderr):
            if stream is not None:
                stream.close()


class _AppServerSwitchUpstream(BaseHTTPRequestHandler):
    @staticmethod
    def _tool_call_id(payload):
        completed = next(
            json.loads(line[6:])
            for line in payload.decode().splitlines()
            if line.startswith("data: ")
            and json.loads(line[6:]).get("type") == "response.completed"
        )
        return completed["response"]["output"][0]["call_id"]

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = _decode_request(raw, self.headers.get("Content-Encoding", "identity"))
        headers = {name.lower(): value for name, value in self.headers.items()}
        self.server.observations.append({"path": self.path, "body": body, "headers": headers})
        authorization = headers.get("authorization")
        if getattr(self.server, "subagent_tree", False) and body.get("generate") is not False:
            if headers.get("x-openai-subagent"):
                if authorization != "Bearer app-server-caller-secret":
                    self.server.fixture_error = "subagent used unexpected authorization: " + repr(authorization)
                    self.send_error(401, "unexpected subagent fixture credential")
                    return
                reply_text = "SAFE_SUBAGENT_CHILD_RESULT"
                self.server.child_response_sent = True
                payload = _fixed_response_stream(body.get("model", "fixture-model")).replace(
                    FIXED_REPLY.encode(), reply_text.encode()
                )
            elif authorization == "Bearer destination-secret":
                if not self.server.spawn_call_sent:
                    spawn_tool = next(
                        (
                            tool for tool in body.get("tools", [])
                            if isinstance(tool, dict)
                            and tool.get("description", "").splitlines()[:1]
                            == ["collaboration.spawn_agent"]
                        ),
                        None,
                    )
                    if spawn_tool is None:
                        self.server.fixture_error = "parent request did not advertise collaboration.spawn_agent"
                        payload = _fixed_response_stream(body.get("model", "fixture-model"))
                    else:
                        payload = _tool_response_stream(
                            body.get("model", "fixture-model"),
                            tool_name="spawn_agent",
                            wire_name=spawn_tool["name"],
                            tool_arguments={
                                "task_name": "emp_fixture_child",
                                "message": (
                                    "Return exactly SAFE_SUBAGENT_CHILD_RESULT. "
                                    "Do not use tools or read or write files."
                                ),
                                "model": "native/model",
                                "reasoning_effort": "low",
                                "fork_turns": "none",
                            },
                        )
                        self.server.spawn_call_id = self._tool_call_id(payload)
                        self.server.spawn_tool_name = spawn_tool["name"]
                        self.server.spawn_call_sent = True
                elif not self.server.wait_call_sent:
                    wait_tool = next(
                        (
                            tool for tool in body.get("tools", [])
                            if isinstance(tool, dict)
                            and tool.get("description", "").splitlines()[:1]
                            == ["collaboration.wait_agent"]
                        ),
                        None,
                    )
                    if wait_tool is None:
                        self.server.fixture_error = "parent request did not advertise collaboration.wait_agent"
                        payload = _fixed_response_stream(body.get("model", "fixture-model"))
                    else:
                        payload = _tool_response_stream(
                            body.get("model", "fixture-model"),
                            tool_name="wait_agent",
                            wire_name=wait_tool["name"],
                            tool_arguments={"timeout_ms": 10000},
                        )
                        self.server.wait_call_id = self._tool_call_id(payload)
                        self.server.wait_tool_name = wait_tool["name"]
                        self.server.wait_call_sent = True
                else:
                    reply_text = "PARENT_SUBAGENT_TREE_COMPLETED"
                    payload = _fixed_response_stream(body.get("model", "fixture-model")).replace(
                        FIXED_REPLY.encode(), reply_text.encode()
                    )
            else:
                self.server.fixture_error = "parent used unexpected authorization: " + repr(authorization)
                self.send_error(401, "unexpected parent fixture credential")
                return
        elif authorization == "Bearer app-server-caller-secret":
            reply_text = "NATIVE_TURN_HISTORY_MARKER"
        else:
            if authorization == "Bearer destination-secret":
                reply_text = "EXTERNAL_TURN_COMPLETED"
            else:
                self.server.fixture_error = "unexpected upstream authorization: " + repr(authorization)
                self.send_error(401, "unexpected fixture credential")
                return
        if not (getattr(self.server, "subagent_tree", False) and body.get("generate") is not False):
            payload = _fixed_response_stream(body.get("model", "fixture-model")).replace(
                FIXED_REPLY.encode(), reply_text.encode()
            )
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


def _fixture_payload(upstream, body, headers):
    upstream.requests.append((body, headers))
    source = body.get("input", [])
    outputs = [item for item in source if isinstance(item, dict)
               and item.get("type") in {"function_call_output", "custom_tool_call_output"}] if isinstance(source, list) else []
    if upstream.failure_code and body.get("generate") is not False:
        event = {"type": "response.failed", "response": {
            "id": "fixture-failed-response", "status": "failed", "model": body["model"],
            "error": {"code": upstream.failure_code, "message": "fixture policy explanation"},
        }}
        payload = ("data: " + json.dumps(event) + "\n\n").encode()
    elif outputs:
        upstream.tool_outputs.extend(outputs)
        payload = _fixed_response_stream(body["model"])
    elif body.get("generate") is False:
        payload = _fixed_response_stream(body["model"])
    else:
        declared_tools = list(body.get("tools") or [])
        if isinstance(source, list):
            for item in source:
                if isinstance(item, dict) and item.get("type") == "additional_tools":
                    declared_tools.extend(item.get("tools") or [])
        tool = _find_command_tool(declared_tools)
        if tool is None:
            upstream.fixture_error = "official native catalog advertised no supported command tool: " + json.dumps([
                {"type": item.get("type"), "name": item.get("name"), "keys": list(item),
                 "nested": [{"type": child.get("type"), "name": child.get("name"), "keys": list(child)}
                            for child in item.get("tools", []) if isinstance(child, dict)]}
                for item in declared_tools if isinstance(item, dict)
            ]) + " request keys=" + repr(list(body))
            payload = _fixed_response_stream(body["model"])
        else:
            name, namespace, kind = tool
            payload = _native_tool_stream(body["model"], name, namespace, kind)
    if getattr(upstream, "server_search", False) and body.get("generate") is not False:
        payload = _server_search_stream(payload)
    if getattr(upstream, "malformed_events", False):
        payload = b"data: {not json}\n\ndata: []\n\ndata: null\n\n" + payload
    return payload

class _NativeMetadataUpstream(BaseHTTPRequestHandler):
    def do_GET(self):
        self.server.websocket_attempts += 1
        self.send_error(404, "fixture WS unsupported")

    protocol_version = "HTTP/1.1"

    def log_message(self, *args):
        pass

    def do_POST(self):
        raw = self.rfile.read(int(self.headers["Content-Length"]))
        body = _decode_request(raw, self.headers.get("Content-Encoding", "identity"))
        headers = {name.lower(): value for name, value in self.headers.items()}
        payload = _fixture_payload(self.server, body, headers)
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("x-codex-turn-state", TURN_STATE)
        self.send_header("openai-model", self.server.reported_model or body["model"])
        self.send_header("x-reasoning-included", "true")
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)


@unittest.skipUnless(
    os.environ.get("EMP_CODEX_TEST_BINARY") and os.environ.get("EMP_CODEX_TEST_CATALOG"),
    "official Codex fixture binary/catalog not configured",
)
class CodexMetadataCliTests(unittest.TestCase):
    def test_native_http_fallback_turn_state_reaches_cli_tool_followup(self):
        self._run_fixture()

    def test_native_builtin_catalog_turn_state_reaches_cli_tool_followup(self):
        self._run_fixture(native_builtin=True)

    def test_native_actual_upstream_reroute_still_reaches_cli(self):
        self._run_fixture(reported_model="gpt-5.2")

    def test_native_upstream_websocket_metadata_and_tool_followup(self):
        self._run_fixture(native_ws=True)

    def test_native_upstream_websocket_actual_reroute_is_preserved(self):
        self._run_fixture(native_ws=True, reported_model="gpt-5.2")

    def test_direct_official_sse_skips_malformed_events_and_keeps_tool_flow(self):
        self._run_fixture(direct_upstream=True, malformed_events=True)

    def test_native_sse_matches_official_malformed_event_tolerance(self):
        self._run_fixture(malformed_events=True)

    def test_direct_official_websocket_skips_malformed_events_and_keeps_tool_flow(self):
        self._run_fixture(direct_upstream=True, native_ws=True, malformed_events=True)

    def test_native_websocket_matches_official_malformed_event_tolerance(self):
        self._run_fixture(native_ws=True, malformed_events=True)

    def test_direct_official_sse_accepts_native_server_search_output(self):
        self._run_fixture(direct_upstream=True, server_search=True)

    def test_native_sse_preserves_native_server_search_output(self):
        self._run_fixture(server_search=True)

    def test_direct_official_websocket_accepts_native_server_search_output(self):
        self._run_fixture(direct_upstream=True, native_ws=True, server_search=True)

    def test_native_websocket_preserves_native_server_search_output(self):
        self._run_fixture(native_ws=True, server_search=True)

    def test_native_invalid_prompt_is_reported_without_generation_replay(self):
        self._run_fixture("invalid_prompt")

    def test_native_bio_policy_is_reported_without_generation_replay(self):
        self._run_fixture("bio_policy")

    @unittest.skipUnless(
        os.environ.get("EMP_RUST_BINARY"),
        "set EMP_RUST_BINARY for the official app-server/Rust EMP switch fixture",
    )
    def test_app_server_switches_native_to_external_on_one_thread(self):
        self._run_app_server_model_switch(
            first_model="native/model",
            second_model="responses/model",
            expected_second_marker="NATIVE_TURN_HISTORY_MARKER",
        )

    @unittest.skipUnless(
        os.environ.get("EMP_RUST_BINARY"),
        "set EMP_RUST_BINARY for the official app-server/Rust EMP switch fixture",
    )
    def test_app_server_switches_external_to_native_on_one_thread(self):
        self._run_app_server_model_switch(
            first_model="responses/model",
            second_model="native/model",
            expected_second_marker="EXTERNAL_TURN_COMPLETED",
        )

    @unittest.skipUnless(
        os.environ.get("EMP_RUST_BINARY"),
        "set EMP_RUST_BINARY for the official app-server/Rust EMP switch fixture",
    )
    def test_app_server_subagent_tree_uses_native_child_route(self):
        self._run_app_server_model_switch(
            first_model="responses/model",
            second_model=None,
            expected_second_marker=None,
            subagent_tree=True,
        )

    def _run_app_server_model_switch(
        self, *, first_model, second_model, expected_second_marker, subagent_tree=False
    ):
        ensure_test_master_key()
        rust_binary = str(Path(os.environ["EMP_RUST_BINARY"]).resolve(strict=True))
        version = subprocess.run(
            [rust_binary, "--version"],
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=10,
            check=True,
        ).stdout.strip()
        self.assertEqual(version, "EMP 0.12.1")
        print("Rust EMP consumer binary:", version, rust_binary)

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), _AppServerSwitchUpstream)
        upstream.observations = []
        upstream.fixture_error = None
        upstream.subagent_tree = subagent_tree
        upstream.child_response_sent = False
        upstream.spawn_call_sent = False
        upstream.spawn_call_id = None
        upstream.spawn_tool_name = None
        upstream.wait_call_sent = False
        upstream.wait_call_id = None
        upstream.wait_tool_name = None
        upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        upstream_thread.start()
        rust_process = None
        app_server = None
        try:
            with tempfile.TemporaryDirectory(prefix="emp-app-server-switch-") as directory:
                root = Path(directory)
                emp_home = root / "emp-home"
                emp_home.mkdir()
                (emp_home / "auth.json").write_text(
                    json.dumps({"tokens": {
                        "access_token": "fixture-native-token",
                        "account_id": "fixture-native-account",
                    }}),
                    encoding="utf-8",
                )
                codex_home = root / "consumer-codex-home"
                codex_home.mkdir()
                project = root / "project"
                project.mkdir()
                subprocess.run(
                    ["git", "init", "--quiet", str(project)],
                    check=True,
                    capture_output=True,
                    timeout=10,
                )
                native_catalog = root / "native.json"
                native_catalog.write_text('{"models":[]}', encoding="utf-8")
                config_path = root / "emp.json"
                upstream_url = "http://127.0.0.1:%d/v1" % upstream.server_port
                config = normalize({
                    "native_catalog_path": str(native_catalog),
                    "providers": [
                        {
                            "id": "native",
                            "name": "Native fixture",
                            "base_url": upstream_url,
                            "protocol": "responses",
                            "auth_mode": "forward",
                        },
                        {
                            "id": "responses",
                            "name": "External fixture",
                            "base_url": upstream_url,
                            "protocol": "responses",
                            "auth_mode": "api_key",
                            "api_key": "destination-secret",
                        },
                    ],
                    "models": [
                        {
                            "id": "native/model",
                            "provider": "native",
                            "upstream_id": "native-upstream",
                            "enabled": True,
                            "reasoning_levels": ["low"],
                            "context_window": 100000,
                            "output_limit": 4096,
                        },
                        {
                            "id": "responses/model",
                            "provider": "responses",
                            "upstream_id": "external-upstream",
                            "enabled": True,
                            "reasoning_levels": ["low"],
                            "context_window": 100000,
                            "output_limit": 4096,
                        },
                    ],
                })
                save(config, config_path)
                official_catalog = json.loads(
                    Path(os.environ["EMP_CODEX_TEST_CATALOG"]).read_text(encoding="utf-8")
                )
                official_models = {
                    model.get("slug"): model
                    for model in official_catalog.get("models", [])
                    if isinstance(model, dict)
                }
                catalog_models = []
                for source_slug, slug, display_name in (
                    ("gpt-6-astra", "native/model", "Native fixture"),
                    ("gpt-6-sol", "responses/model", "External fixture"),
                ):
                    model = official_models.get(source_slug)
                    self.assertIsNotNone(model, f"Codex catalog is missing {source_slug}")
                    model = copy.deepcopy(model)
                    model["slug"] = slug
                    model["display_name"] = display_name
                    catalog_models.append(model)
                catalog_path = root / "codex-models.json"
                catalog_path.write_text(
                    json.dumps({"models": catalog_models}), encoding="utf-8"
                )
                rust_process = EmpProcess.from_config(
                    [rust_binary], config_path, emp_home
                )

                provider = (
                    '{name="EMP fixture", base_url=%s, wire_api="responses", '
                    'env_key="EMP_APP_SERVER_TEST_KEY", supports_websockets=false, '
                    'request_max_retries=0, stream_max_retries=0, '
                    'http_headers={Cookie=%s}}'
                ) % (
                    json.dumps("http://127.0.0.1:%d/v1" % rust_process.port),
                    json.dumps(rust_process.cookie),
                )
                app_environment = dict(
                    os.environ,
                    CODEX_HOME=str(codex_home),
                    EMP_APP_SERVER_TEST_KEY="app-server-caller-secret",
                    OPENAI_API_KEY="fixture-only",
                    CODEX_API_KEY="fixture-only",
                    HTTP_PROXY="http://127.0.0.1:1",
                    HTTPS_PROXY="http://127.0.0.1:1",
                    ALL_PROXY="http://127.0.0.1:1",
                    NO_PROXY="127.0.0.1,localhost,::1",
                )
                command = [
                    str(Path(os.environ["EMP_CODEX_TEST_BINARY"]).resolve(strict=True)),
                    "app-server",
                    "--stdio",
                    "-c",
                    'model_provider="fixture"',
                    "-c",
                    "model_providers.fixture=" + provider,
                    "-c",
                    "model_catalog_json=" + json.dumps(str(catalog_path)),
                    "-c",
                    'model_reasoning_effort="low"',
                    "-c",
                    'sandbox_mode="read-only"',
                    "-c",
                    'approval_policy="never"',
                    "-c",
                    'web_search="disabled"',
                ]
                app_server = _CodexAppServerStdio(command, app_environment, project)
                app_server.request(
                    "initialize",
                    {"clientInfo": {"name": "EMP app-server acceptance", "version": "1.0"}},
                )
                app_server.notify("initialized", {})
                started = app_server.request(
                    "thread/start",
                    {
                        "cwd": str(project),
                        "model": "native/model",
                        "modelProvider": "fixture",
                        "ephemeral": True,
                    },
                )
                thread_id = started["thread"]["id"]
                first_started = app_server.request(
                    "turn/start",
                    {
                        "threadId": thread_id,
                        "model": first_model,
                        "input": [{
                            "type": "text",
                            "text": "PARENT_SUBAGENT_REQUEST" if subagent_tree
                            else "FIRST_APP_SERVER_TURN",
                        }],
                    },
                )
                first_terminal = app_server.wait_for_turn(
                    thread_id, first_started["turn"]["id"]
                )
                self.assertEqual(first_terminal["status"], "completed", first_terminal)
                if second_model is not None:
                    second_started = app_server.request(
                        "turn/start",
                        {
                            "threadId": thread_id,
                            "model": second_model,
                            "input": [{"type": "text", "text": "SECOND_APP_SERVER_TURN"}],
                        },
                    )
                    second_terminal = app_server.wait_for_turn(
                        thread_id, second_started["turn"]["id"]
                    )
                    self.assertEqual(second_terminal["status"], "completed", second_terminal)

            self.assertIsNone(upstream.fixture_error)
            if subagent_tree:
                self._assert_app_server_subagent_tree(
                    upstream.observations,
                    parent_thread_id=thread_id,
                    spawn_call_id=upstream.spawn_call_id,
                    spawn_tool_name=upstream.spawn_tool_name,
                    wait_call_id=upstream.wait_call_id,
                    wait_tool_name=upstream.wait_tool_name,
                    child_response_sent=upstream.child_response_sent,
                )
                return

            self.assertEqual(len(upstream.observations), 2, upstream.observations)
            requests = [
                record
                for record in upstream.observations
                if record["body"].get("generate") is not False
            ]
            self.assertEqual(len(requests), 2, upstream.observations)
            first_request, second_request = requests
            native_request = next(
                record for record in requests
                if record["body"]["model"] == "native-upstream"
            )
            external_request = next(
                record for record in requests
                if record["body"]["model"] == "external-upstream"
            )
            expected_upstream_by_model = {
                "native/model": "native-upstream",
                "responses/model": "external-upstream",
            }
            expected_auth_by_model = {
                "native/model": "Bearer app-server-caller-secret",
                "responses/model": "Bearer destination-secret",
            }
            self.assertEqual(
                first_request["body"]["model"], expected_upstream_by_model[first_model]
            )
            self.assertEqual(
                second_request["body"]["model"], expected_upstream_by_model[second_model]
            )
            self.assertEqual(
                first_request["headers"].get("authorization"),
                expected_auth_by_model[first_model],
            )
            self.assertEqual(
                second_request["headers"].get("authorization"),
                expected_auth_by_model[second_model],
            )
            self.assertEqual(
                native_request["headers"].get("authorization"),
                "Bearer app-server-caller-secret",
            )
            native_headers = native_request["headers"]
            self.assertEqual(native_headers.get("thread-id"), thread_id)
            self.assertEqual(native_headers.get("session-id"), thread_id)
            turn_metadata = json.loads(native_headers["x-codex-turn-metadata"])
            self.assertEqual(turn_metadata.get("thread_id"), thread_id)
            self.assertNotIn(
                "destination-secret",
                json.dumps(native_headers) + json.dumps(native_request["body"]),
            )
            self.assertEqual(
                external_request["headers"].get("authorization"),
                "Bearer destination-secret",
            )
            external_headers = external_request["headers"]
            for internal_header in (
                "cookie",
                "thread-id",
                "session-id",
                "x-codex-turn-metadata",
                "chatgpt-account-id",
            ):
                self.assertNotIn(
                    internal_header,
                    external_headers,
                    f"external destination received {internal_header}: {external_headers}",
                )
            self.assertNotIn(
                "app-server-caller-secret",
                json.dumps(external_headers) + json.dumps(external_request["body"]),
            )
            rendered_input = json.dumps(second_request["body"].get("input"))
            for marker in (
                "FIRST_APP_SERVER_TURN",
                expected_second_marker,
                "SECOND_APP_SERVER_TURN",
            ):
                self.assertIn(marker, rendered_input, rendered_input)
        finally:
            if app_server is not None:
                app_server.close()
            if rust_process is not None:
                rust_process.close()
            upstream.shutdown()
            upstream.server_close()
            upstream_thread.join(timeout=3)

    def _assert_app_server_subagent_tree(
        self, observations, *, parent_thread_id, spawn_call_id, spawn_tool_name,
        wait_call_id, wait_tool_name, child_response_sent,
    ):
        summary = [
            {
                "model": record["body"].get("model"),
                "generate": record["body"].get("generate"),
                "authorization": record["headers"].get("authorization"),
                "subagent": record["headers"].get("x-openai-subagent"),
                "thread_id": record["headers"].get("thread-id"),
                "parent_thread_id": record["headers"].get("x-codex-parent-thread-id"),
                "input_types": [
                    item.get("type") for item in record["body"].get("input", [])
                    if isinstance(item, dict)
                ],
            }
            for record in observations
        ]
        self.assertIsNotNone(spawn_call_id, summary)
        self.assertIsNotNone(spawn_tool_name, summary)
        self.assertIsNotNone(wait_call_id, summary)
        self.assertIsNotNone(wait_tool_name, summary)
        self.assertTrue(child_response_sent, summary)
        self.assertEqual(len(observations), 4, summary)
        requests = [
            record for record in observations
            if record["body"].get("generate") is not False
        ]
        self.assertEqual(len(requests), 4, summary)
        children = [
            record for record in requests
            if record["headers"].get("x-openai-subagent")
        ]
        parents = [
            record for record in requests
            if not record["headers"].get("x-openai-subagent")
        ]
        self.assertEqual(len(children), 1, summary)
        self.assertEqual(len(parents), 3, summary)
        child = children[0]
        self.assertEqual(child["body"].get("model"), "native-upstream", summary)
        self.assertEqual(
            child["headers"].get("authorization"),
            "Bearer app-server-caller-secret",
            summary,
        )

        def has_tool_output(record, call_id):
            source = record["body"].get("input")
            return isinstance(source, list) and any(
                isinstance(item, dict)
                and item.get("type") == "function_call_output"
                and item.get("call_id") == call_id
                for item in source
            )

        after_wait = [record for record in parents if has_tool_output(record, wait_call_id)]
        self.assertEqual(len(after_wait), 1, summary)
        parent_after_wait = after_wait[0]
        after_spawn = [
            record for record in parents
            if record is not parent_after_wait and has_tool_output(record, spawn_call_id)
        ]
        self.assertEqual(len(after_spawn), 1, summary)
        parent_after_spawn = after_spawn[0]
        starts = [
            record for record in parents
            if record is not parent_after_spawn and record is not parent_after_wait
        ]
        self.assertEqual(len(starts), 1, summary)
        parent_start = starts[0]
        self.assertIn("PARENT_SUBAGENT_REQUEST", json.dumps(parent_start["body"].get("input")), summary)

        child_headers = child["headers"]
        self.assertTrue(child_headers.get("x-openai-subagent"), summary)
        child_thread_id = child_headers.get("thread-id")
        self.assertTrue(child_thread_id, summary)
        self.assertNotEqual(child_thread_id, parent_thread_id, summary)
        self.assertEqual(
            child_headers.get("x-codex-parent-thread-id"), parent_thread_id,
            summary,
        )
        child_turn_metadata = json.loads(child_headers["x-codex-turn-metadata"])
        self.assertEqual(child_turn_metadata.get("thread_id"), child_thread_id, summary)
        self.assertIn(
            "Return exactly SAFE_SUBAGENT_CHILD_RESULT",
            json.dumps(child["body"].get("input")),
            summary,
        )
        self.assertNotIn(
            "destination-secret",
            json.dumps(child_headers) + json.dumps(child["body"]),
        )

        for parent_request in (parent_start, parent_after_spawn, parent_after_wait):
            headers = parent_request["headers"]
            self.assertEqual(parent_request["body"].get("model"), "external-upstream", summary)
            self.assertEqual(
                headers.get("authorization"), "Bearer destination-secret", summary
            )
            for internal_header in (
                "cookie",
                "thread-id",
                "session-id",
                "x-codex-turn-metadata",
                "x-codex-parent-thread-id",
                "x-openai-subagent",
                "chatgpt-account-id",
            ):
                self.assertNotIn(internal_header, headers, summary)
            self.assertNotIn(
                "app-server-caller-secret",
                json.dumps(headers) + json.dumps(parent_request["body"]),
            )

        after_spawn_input = parent_after_spawn["body"].get("input", [])
        self.assertIsInstance(after_spawn_input, list, summary)
        spawn_calls = [
            item for item in after_spawn_input
            if isinstance(item, dict)
            and item.get("type") == "function_call"
            and item.get("call_id") == spawn_call_id
        ]
        spawn_outputs = [
            item for item in after_spawn_input
            if isinstance(item, dict)
            and item.get("type") == "function_call_output"
            and item.get("call_id") == spawn_call_id
        ]
        self.assertEqual(len(spawn_calls), 1, summary)
        self.assertEqual(spawn_calls[0].get("name"), spawn_tool_name, summary)
        self.assertEqual(len(spawn_outputs), 1, summary)
        tool_result = json.loads(spawn_outputs[0]["output"])
        self.assertEqual(tool_result.get("task_name", "").rsplit("/", 1)[-1], "emp_fixture_child", summary)

        after_wait_input = parent_after_wait["body"].get("input", [])
        self.assertIsInstance(after_wait_input, list, summary)
        wait_calls = [
            item for item in after_wait_input
            if isinstance(item, dict)
            and item.get("type") == "function_call"
            and item.get("call_id") == wait_call_id
        ]
        wait_outputs = [
            item for item in after_wait_input
            if isinstance(item, dict)
            and item.get("type") == "function_call_output"
            and item.get("call_id") == wait_call_id
        ]
        self.assertEqual(len(wait_calls), 1, summary)
        self.assertEqual(wait_calls[0].get("name"), wait_tool_name, summary)
        self.assertEqual(len(wait_outputs), 1, summary)
        self.assertIn(
            "SAFE_SUBAGENT_CHILD_RESULT",
            json.dumps([wait_outputs[0], after_wait_input]),
            summary,
        )

    def _run_fixture(self, failure_code=None, *, native_builtin=False, reported_model=None,
                     native_ws=False, direct_upstream=False, malformed_events=False,
                     server_search=False):
        ensure_test_master_key()
        if native_ws:
            from websockets.sync.server import serve

            def handshake(connection, request, response):
                response.headers["x-codex-turn-state"] = TURN_STATE
                response.headers["openai-model"] = reported_model or "gpt-6-astra"
                return response

            def receive(connection):
                upstream.fixture_connections += 1
                raw_headers = list(connection.request.headers.raw_items())
                authorization = [value for name, value in raw_headers if name.lower() == "authorization"]
                if len(authorization) != 1:
                    upstream.fixture_error = "native handshake must have exactly one Authorization header, got %d" % len(authorization)
                    connection.close(1008, "duplicate authentication header")
                    return
                headers = {name.lower(): value for name, value in raw_headers}
                for message in connection:
                    body = json.loads(message)
                    payload = _fixture_payload(upstream, body, headers)
                    if malformed_events or server_search:
                        # Official prewarm can run before the current turn's
                        # sticky OnceLock exists. Supply current stream state
                        # in a frame, as well as the handshake snapshot.
                        connection.send(json.dumps({
                            "type": "response.metadata",
                            "headers": {"x-codex-turn-state": TURN_STATE},
                        }))
                    for line in payload.decode().splitlines():
                        if line.startswith("data: "):
                            connection.send(line[6:])

            upstream = self.enterContext(serve(
                receive, "127.0.0.1", 0, process_response=handshake,
                compression="deflate", ping_interval=None,
            ))
            upstream.server_port = upstream.socket.getsockname()[1]
            upstream.fixture_connections = 0
        else:
            upstream = ThreadingHTTPServer(("127.0.0.1", 0), _NativeMetadataUpstream)
            upstream.daemon_threads = True
            self.addCleanup(upstream.server_close)
        upstream.requests, upstream.tool_outputs, upstream.fixture_error = [], [], None
        upstream.websocket_attempts = 0
        upstream.failure_code = failure_code
        upstream.reported_model = reported_model
        upstream.malformed_events = malformed_events
        upstream.server_search = server_search
        upstream_thread = threading.Thread(target=upstream.serve_forever, daemon=True)
        upstream_thread.start()
        self.addCleanup(upstream_thread.join, 3)
        self.addCleanup(upstream.shutdown)
        with tempfile.TemporaryDirectory(prefix="emp-native-metadata-") as directory:
            root = Path(directory)
            model_id = "gpt-6-astra" if native_builtin or direct_upstream else "native/gpt-6-astra"
            home = root / "codex"
            home.mkdir()
            environment = dict(os.environ, CODEX_HOME=str(home), OPENAI_API_KEY="fixture-only",
                               CODEX_API_KEY="fixture-only", EMP_METADATA_TEST_KEY="fixture-only",
                               HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1",
                               ALL_PROXY="http://127.0.0.1:1", NO_PROXY="127.0.0.1,localhost,::1")
            with patch.dict(os.environ, {"CODEX_HOME": str(home)}):
                config = normalize({
                    "native_catalog_path": str(Path(os.environ["EMP_CODEX_TEST_CATALOG"]).resolve()),
                    "providers": [{"id": "native", "auth_mode": "forward", "protocol": "responses",
                                   "base_url": "http://127.0.0.1:%d/v1" % upstream.server_port}],
                    "models": [{"id": model_id, "provider": "native",
                                "upstream_id": "gpt-6-astra", "reasoning_levels": ["low"]}],
                })
                save(config, root / "emp.json")
                catalog = root / "catalog.json"
                write_catalog(config, catalog)
                rust_process = None
                if os.environ.get("EMP_RUST_BINARY") and not direct_upstream:
                    rust_process = EmpProcess.from_config(
                        [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())], root / "emp.json", home)
                    target_port = rust_process.port
                    session_cookie = rust_process.cookie
                else:
                    manager = IntegrationManager(home / "config.toml", home / ".integration/lease.json")
                    state = AppState(root / "emp.json", integration_manager=manager, catalog_path=catalog)
                    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
                    server.daemon_threads = True
                    threading.Thread(target=server.serve_forever, daemon=True).start()
                    target_port = upstream.server_port if direct_upstream else server.server_port
                    session_cookie = "emp_session=" + state.session_token
                supports_ws = "true" if native_ws or not direct_upstream else "false"
                provider = ('{name="OpenAI", base_url="http://127.0.0.1:%d/v1", '
                            'wire_api="responses", env_key="EMP_METADATA_TEST_KEY", supports_websockets=%s, '
                            'request_max_retries=0, stream_max_retries=1, '
                            'http_headers={"chatgpt-account-id"="fixture-account", Cookie=%s}}') % (
                                target_port, supports_ws, json.dumps(session_cookie))
                command = [str(Path(os.environ["EMP_CODEX_TEST_BINARY"]).resolve()),
                           "exec", "--ignore-user-config", "--ephemeral", "--skip-git-repo-check",
                           "--color", "never", "-m", model_id,
                           "-c", 'model_provider="fixture"', "-c", "model_providers.fixture=" + provider,
                           "-c", "model_catalog_json=" + json.dumps(str(catalog)),
                           "-c", 'model_reasoning_effort="low"', "-c", 'web_search="disabled"',
                           "-c", 'approval_policy="never"', "-c", 'sandbox_mode="read-only"',
                           "--output-last-message", str(root / "reply.txt"), "Say hello."]
                try:
                    if direct_upstream or rust_process is not None:
                        transport_guard = nullcontext(None)
                    elif native_ws:
                        transport_guard = patch(
                            "easy_multi_provider.codex_dispatch.proxy",
                            side_effect=AssertionError("unexpected HTTP fallback from healthy native WS"),
                        )
                    else:
                        transport_guard = nullcontext(None)
                    with transport_guard as connector:
                        result = subprocess.run(command, env=environment, cwd=root, stdin=subprocess.DEVNULL,
                                                capture_output=True, text=True, encoding="utf-8",
                                                errors="replace", timeout=60)
                    if native_ws:
                        self.assertIsNone(upstream.fixture_error)
                        if not direct_upstream and rust_process is None:
                            self.assertFalse(connector.called, "healthy native WS fell back to HTTP")
                        self.assertEqual(upstream.fixture_connections, 1, "native tool followup opened another upstream socket")
                    elif not direct_upstream:
                        self.assertEqual(upstream.websocket_attempts, 1, "native WS fallback probe count")
                    if failure_code:
                        self.assertNotEqual(result.returncode, 0, result.stderr[-4000:])
                        requests = [body for body, _ in upstream.requests
                                    if body.get("generate") is not False]
                        self.assertEqual(len(requests), 1, result.stderr[-4000:])
                        self.assertIn("fixture policy explanation", result.stderr)
                        return
                    self.assertEqual(result.returncode, 0, result.stderr[-4000:])
                    if reported_model:
                        self.assertIn("model rerouted: " + model_id + " -> " + reported_model, result.stderr)
                    else:
                        self.assertNotIn("Your account was flagged", result.stderr)
                    self.assertIsNone(upstream.fixture_error)
                    self.assertEqual((root / "reply.txt").read_text().strip(), FIXED_REPLY)
                    requests = [(body, headers) for body, headers in upstream.requests
                                if body.get("generate") is not False]
                    self.assertEqual(len(requests), 2, result.stderr[-4000:])
                    self.assertNotIn("x-codex-turn-state", requests[0][1])
                    if native_ws:
                        # The real upstream prewarm can teach Codex its sticky
                        # token before the first generation. HTTP-upgrade
                        # fallback below has not completed that prewarm.
                        self.assertTrue(any(body.get("generate") is False for body, _ in upstream.requests))
                    else:
                        self.assertNotIn("x-codex-turn-state", requests[0][0].get("client_metadata", {}))
                    followup, headers = requests[1]
                    # WS clients put sticky routing in frame client_metadata;
                    # HTTP clients use a header. Observe either actual wire form.
                    state_token = headers.get("x-codex-turn-state") or followup.get(
                        "client_metadata", {}).get("x-codex-turn-state")
                    self.assertEqual(state_token, TURN_STATE)
                    self.assertIn("EMP_RUNTIME_TOOL_OK", json.dumps(upstream.tool_outputs))
                finally:
                    if rust_process is not None:
                        rust_process.close()
                    else:
                        server.shutdown()
                        server.server_close()
