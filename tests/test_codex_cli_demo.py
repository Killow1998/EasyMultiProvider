"""Opt-in end-to-end test using the real Codex CLI and a fixed demo model."""

import json
import os
import shutil
import subprocess
import sys
import tempfile
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.catalog import write_catalog
from easy_multi_provider.config import normalize, save
from easy_multi_provider.server import AppState, make_handler
from easy_multi_provider.tool_bridge import ExternalTools


ensure_test_master_key()


FIXED_REPLY = "EASY_MULTIPROVIDER_DEMO_OK"


def _event(name, payload, sequence):
    value = dict(payload)
    value.setdefault("type", name)
    value["sequence_number"] = sequence
    return "event: %s\ndata: %s\n\n" % (name, json.dumps(value))


def _fixed_response_stream(model):
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    created_at = int(time.time())
    item = {
        "id": message_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": FIXED_REPLY, "annotations": []}],
    }
    base = {
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": "in_progress",
        "error": None,
        "incomplete_details": None,
        "instructions": None,
        "model": model,
        "output": [],
        "parallel_tool_calls": True,
        "tool_choice": "auto",
        "tools": [],
        "usage": None,
    }
    events = [
        _event("response.created", {"response": base}, 0),
        _event(
            "response.output_item.added",
            {"output_index": 0, "item": dict(item, status="in_progress", content=[])},
            1,
        ),
        _event(
            "response.content_part.added",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            },
            2,
        ),
        _event(
            "response.output_text.delta",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "delta": FIXED_REPLY,
                "logprobs": [],
            },
            3,
        ),
        _event(
            "response.output_text.done",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "text": FIXED_REPLY,
                "logprobs": [],
            },
            4,
        ),
        _event(
            "response.content_part.done",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": item["content"][0],
            },
            5,
        ),
        _event("response.output_item.done", {"output_index": 0, "item": item}, 6),
    ]
    completed = dict(base)
    completed.update(
        {
            "status": "completed",
            "completed_at": created_at,
            "output": [item],
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 1,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 2,
            },
        }
    )
    events.append(_event("response.completed", {"response": completed}, 7))
    return "".join(events).encode("utf-8")


def _tool_response_stream(model, tool_name="exec", wire_name=None, tool_arguments=None):
    response_id = "resp_" + uuid.uuid4().hex
    call_id = "call_" + uuid.uuid4().hex
    arguments = json.dumps(
        {
            "input": (
                'const r = await tools.exec_command({cmd:"echo EMP_RUNTIME_TOOL_OK", workdir:".", '
                'yield_time_ms:10000, max_output_tokens:1000}); text(r.output)'
            )
        },
        separators=(",", ":"),
    )
    if tool_name != "exec":
        arguments = json.dumps({"cmd": "echo EMP_RUNTIME_TOOL_OK"} if tool_name == "exec_command"
                               else {"command": "echo EMP_RUNTIME_TOOL_OK"})
    if tool_arguments is not None:
        arguments = json.dumps(tool_arguments)
    item = {
        "id": call_id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": wire_name or tool_name,
        "arguments": arguments,
    }
    response = {
        "id": response_id,
        "object": "response",
        "status": "completed",
        "model": model,
        "output": [item],
    }
    events = [
        _event(
            "response.created",
            {"response": dict(response, status="in_progress", output=[])},
            0,
        ),
        _event(
            "response.output_item.added",
            {"output_index": 0, "item": dict(item, status="in_progress", arguments="")},
            1,
        ),
        _event(
            "response.function_call_arguments.delta",
            {"item_id": call_id, "output_index": 0, "delta": arguments},
            2,
        ),
        _event(
            "response.function_call_arguments.done",
            {"item_id": call_id, "output_index": 0, "arguments": arguments},
            3,
        ),
        _event("response.output_item.done", {"output_index": 0, "item": item}, 4),
        _event("response.completed", {"response": response}, 5),
    ]
    return "".join(events).encode("utf-8")


class FixedResponsesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        self.server.seen_models.append(body.get("model"))
        payload = _fixed_response_stream(body.get("model", "fixed-model"))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)


class ToolResponsesHandler(FixedResponsesHandler):
    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        self.server.seen_models.append(body.get("model"))
        source = body.get("input")
        source = source if isinstance(source, list) else []
        saw_tool_output = any(
            isinstance(item, dict) and item.get("type") == "function_call_output"
            for item in source
        )
        self.server.request_count += 1
        tool_names = {tool.get("name") for tool in body.get("tools", [])
                      if isinstance(tool, dict) and tool.get("type") == "function"}
        tool_name, wire_name = None, None
        for candidate in ("exec_command", "shell_command", "exec"):
            # Official runtimes may advertise plain tools or the functions
            # namespace. Exercise the same real command in either contract.
            alias = ExternalTools()._name(candidate, "functions")
            if candidate in tool_names or alias in tool_names:
                tool_name, wire_name = candidate, candidate if candidate in tool_names else alias
                break
        self.server.saw_exec_tool = self.server.saw_exec_tool or tool_name is not None
        self.server.saw_tool_output = self.server.saw_tool_output or saw_tool_output
        self.server.saw_successful_tool = self.server.saw_successful_tool or any(
            isinstance(item, dict) and item.get("type") == "function_call_output"
            and "EMP_RUNTIME_TOOL_OK" in str(item.get("output", ""))
            for item in source
        )
        self.server.tool_outputs = [str(item.get("output", ""))[:2000] for item in source
                                    if isinstance(item, dict) and item.get("type") == "function_call_output"]
        payload = (
            _fixed_response_stream(body.get("model", "fixed-model"))
            if saw_tool_output
            else _tool_response_stream(body.get("model", "fixed-model"), tool_name or "exec", wire_name)
        )
        if self.server.discovery:
            search_tool = next((tool for tool in body.get("tools", [])
                                if "Tool discovery" in tool.get("description", "")), None)
            loaded_tool = next((tool for tool in body.get("tools", [])
                                if "Lookup the fixture calendar" in tool.get("description", "")), None)
            if self.server.request_count == 1 and search_tool:
                self.server.saw_search_tool = True
                payload = _tool_response_stream(body["model"], wire_name=search_tool["name"],
                                                tool_arguments={"query": "fixture calendar lookup", "limit": 1})
            elif loaded_tool and self.server.request_count == 2:
                self.server.saw_loaded_tool = True
                payload = _tool_response_stream(body["model"], wire_name=loaded_tool["name"], tool_arguments={})
            else:
                payload = _fixed_response_stream(body["model"])
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)


@unittest.skipUnless(
    os.environ.get("EASY_MP_RUN_CODEX_CLI") == "1",
    "set EASY_MP_RUN_CODEX_CLI=1 to run the real Codex CLI demo",
)
class CodexCliDemoTests(unittest.TestCase):
    def test_real_codex_cli_uses_temporary_demo_model(self):
        self._run_cli(False)

    def test_real_codex_searches_loads_and_executes_deferred_tool(self):
        self._run_cli(True)

    def _run_cli(self, discovery):
        codex = os.environ.get("EMP_CODEX_TEST_BINARY") or shutil.which("codex")
        self.assertIsNotNone(codex, "codex CLI is not installed")
        native_catalog = Path(os.environ.get("EMP_CODEX_TEST_CATALOG") or
                              Path.home() / ".codex" / "models_cache.json")
        self.assertTrue(native_catalog.exists(), "Codex native model cache is unavailable")

        fake_server = ThreadingHTTPServer(("127.0.0.1", 0), ToolResponsesHandler)
        fake_server.seen_models = []
        fake_server.request_count = 0
        fake_server.saw_exec_tool = False
        fake_server.saw_tool_output = False
        fake_server.saw_successful_tool = False
        fake_server.tool_outputs = []
        fake_server.discovery = discovery
        fake_server.saw_search_tool = False
        fake_server.saw_loaded_tool = False
        fake_thread = threading.Thread(target=fake_server.serve_forever, daemon=True)
        fake_thread.start()
        router_server = None
        router_thread = None
        try:
            with tempfile.TemporaryDirectory(prefix="easy-mp-codex-demo-") as directory:
                root = Path(directory)
                config_path = root / "config.json"
                catalog_path = root / "catalog.json"
                output_path = root / "last-message.txt"
                isolated_home = root / "codex-home"
                isolated_home.mkdir()
                runtime_temp = root / "runtime-temp"
                runtime_temp.mkdir()
                environment = dict(os.environ, CODEX_HOME=str(isolated_home),
                                   TMPDIR=str(runtime_temp), TEMP=str(runtime_temp), TMP=str(runtime_temp),
                                   OPENAI_API_KEY="fixture-only", CODEX_API_KEY="fixture-only",
                                   HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1",
                                   ALL_PROXY="http://127.0.0.1:1", NO_PROXY="127.0.0.1,localhost,::1")
                config = normalize(
                    {
                        "host": "127.0.0.1",
                        "port": 4200,
                        "native_catalog_path": str(native_catalog),
                        "providers": [
                            {
                                "id": "demo",
                                "name": "Fixed Demo",
                                "base_url": "http://127.0.0.1:%d/v1" % fake_server.server_address[1],
                                "protocol": "responses",
                                "auth_mode": "api_key",
                                "api_key": "local-demo-key",
                            }
                        ],
                        "models": [
                            {
                                "id": "demo/fixed",
                                "provider": "demo",
                                "upstream_id": "fixed-model",
                                "display_name": "Fixed Demo Model",
                                "reasoning_levels": ["medium"],
                            }
                        ],
                    }
                )
                save(config, config_path)
                write_catalog(config, catalog_path)

                state = AppState(config_path)
                base_handler = make_handler(state)

                class TrackingHandler(base_handler):
                    def _serve_responses_websocket(self):
                        self.server.websocket_upgrades += 1
                        return super()._serve_responses_websocket()

                    def _websocket_events(self, metadata, result):
                        self.server.websocket_requests += 1
                        yield from super()._websocket_events(metadata, result)

                router_server = ThreadingHTTPServer(("127.0.0.1", 0), TrackingHandler)
                router_server.websocket_upgrades = 0
                router_server.websocket_requests = 0
                router_thread = threading.Thread(target=router_server.serve_forever, daemon=True)
                router_thread.start()
                router_url = "http://127.0.0.1:%d/v1" % router_server.server_address[1]
                fixture_provider = (
                    '{name="OpenAI", base_url=%s, wire_api="responses", '
                    'env_key="OPENAI_API_KEY", supports_websockets=true, '
                    'http_headers={Cookie=%s}}'
                ) % (json.dumps(router_url), json.dumps("emp_session=" + state.session_token))

                command = [
                    codex,
                    "exec",
                    "--ignore-user-config",
                    "--ephemeral",
                    "--skip-git-repo-check",
                    "--color",
                    "never",
                    "--output-last-message",
                    str(output_path),
                    "-m",
                    "demo/fixed",
                    "-c",
                    'model_provider="fixture"',
                    "-c",
                    'model_catalog_json=' + json.dumps(str(catalog_path)),
                    "-c",
                    'model_providers.fixture=' + fixture_provider,
                    "-c",
                    'model_reasoning_effort="medium"',
                    "-c",
                    'approval_policy="never"',
                    "-c",
                    'sandbox_mode="read-only"',
                    "-c",
                    'web_search="disabled"',
                    "Return the model response without modification.",
                ]
                if discovery:
                    fixture = Path(__file__).parent / "fixtures" / "mcp_tool_search.py"
                    mcp = '{command=%s, args=[%s]}' % (json.dumps(sys.executable), json.dumps(str(fixture.resolve())))
                    command[-1:-1] = ["-c", "mcp_servers.fixture=" + mcp]
                try:
                    completed = subprocess.run(
                        command, env=environment, cwd=str(root), text=True,
                        stdin=subprocess.DEVNULL, stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=60,
                    )
                except subprocess.TimeoutExpired as exc:
                    self.fail("Codex fixture timed out after %s upstream calls\nstdout:\n%s\nstderr:\n%s"
                              % (fake_server.request_count, exc.stdout, exc.stderr))
                self.assertEqual(
                    completed.returncode,
                    0,
                    "Codex CLI failed\nstdout:\n%s\nstderr:\n%s"
                    % (completed.stdout, completed.stderr),
                )
                self.assertEqual(output_path.read_text(encoding="utf-8").strip(), FIXED_REPLY)
                execution_details = "Codex fixture output:\n%s\n%s\nTool results: %s" % (
                    completed.stdout, completed.stderr, fake_server.tool_outputs)
                expected_count = 3 if discovery else 2
                self.assertEqual(fake_server.seen_models, ["fixed-model"] * expected_count, execution_details)
                self.assertEqual(fake_server.request_count, expected_count)
                if discovery:
                    self.assertTrue(fake_server.saw_search_tool)
                    self.assertTrue(fake_server.saw_loaded_tool)
                else:
                    self.assertTrue(fake_server.saw_exec_tool)
                self.assertTrue(fake_server.saw_tool_output)
                self.assertTrue(fake_server.saw_successful_tool, execution_details)
                self.assertGreater(router_server.websocket_upgrades, 0)
                self.assertGreater(router_server.websocket_requests, 0)
        finally:
            if router_server is not None:
                router_server.shutdown()
                router_server.server_close()
            fake_server.shutdown()
            fake_server.server_close()


if __name__ == "__main__":
    unittest.main()
