"""Opt-in official CLI acceptance of native control metadata and tool followups."""
import gzip
from contextlib import nullcontext
import json
import os
from pathlib import Path
import subprocess
import tempfile
import threading
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
