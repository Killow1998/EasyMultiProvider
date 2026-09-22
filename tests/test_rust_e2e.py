"""Exercise real Python/Rust EMP processes through their public wire contract.

Run with EMP_RUST_BINARY=/absolute/path/to/EMP python -m unittest
tests.test_rust_e2e -v. No provider, browser or Codex production state is used.
Chat stream scenarios reuse the existing regression inputs and assertions,
replacing only their in-process transport fixture with a real HTTP upstream.
"""

import json
import os
from pathlib import Path
import socket
import subprocess
import sys
import tempfile
import tomlkit
import unittest
from contextlib import ExitStack

import zstandard

from tests import test_chat_projection_regressions as chat_cases
from tests.test_server import _masked_text_frame, _read_text_frame
from tests.test_shared_app_server_runtime import _UnixModelListServer
from tests.rust_e2e_support import ROOT, EmpProcess, Upstream, normalized_ids
from easy_multi_provider.integration import IntegrationManager


@unittest.skipUnless(os.environ.get("EMP_RUST_BINARY"), "set EMP_RUST_BINARY for real-process E2E")
class RustEndToEnd(unittest.TestCase):
    usage = chat_cases.ChatProjectionRegressions.usage
    expected_usage = chat_cases.ChatProjectionRegressions.expected_usage

    @classmethod
    def setUpClass(cls):
        cls.stack = ExitStack()
        cls.addClassCleanup(cls.stack.close)
        temporary = cls.stack.enter_context(tempfile.TemporaryDirectory(prefix="emp-e2e-"))
        root = Path(temporary)
        cls.upstream = Upstream()
        cls.stack.callback(cls.upstream.close)
        cls.backends = []
        for name, command in [
            ("python", [sys.executable, "-m", "easy_multi_provider"]),
            ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
        ]:
            backend = EmpProcess(command, cls.upstream, root / name)
            cls.stack.callback(backend.close)
            cls.backends.append(backend)

    def setUp(self):
        # A failed scenario must not contaminate another scenario's observations.
        while not self.upstream.requests.empty():
            self.upstream.requests.get_nowait()

    def compare_exchange(self, body, payload, *, status=200, content_type="application/json",
                         compressed=False):
        observed = []
        results = []
        for backend in self.backends:
            self.upstream.configure(payload, status, content_type)
            outgoing = body
            headers = {}
            if compressed:
                outgoing = zstandard.ZstdCompressor().compress(json.dumps(body).encode())
                headers = {"Content-Type": "application/json", "Content-Encoding": "zstd"}
            actual_status, actual_headers, raw = backend.request(
                "POST", "/v1/responses", outgoing, headers=headers)
            self.assertEqual(actual_status, status, raw)
            observed.append(self.upstream.requests.get(timeout=5))
            if "text/event-stream" in actual_headers["content-type"]:
                result = [json.loads(line[6:]) for line in raw.splitlines()
                          if line.startswith(b"data: ")]
            else:
                result = json.loads(raw)
            results.append(result)
        self.assertEqual(observed[0][0], observed[1][0])
        self.assertEqual(observed[0][2], observed[1][2])
        self.assertEqual(normalized_ids(results[0]), normalized_ids(results[1]))
        self.assertTrue(self.upstream.requests.empty(), "unexpected retry")
        return results[1]

    def stream(self, chunks):
        wire = ("".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks)
                + "data: [DONE]\n\n").encode()
        return self.compare_exchange(
            {"model": "test/model", "input": "hello", "stream": True},
            wire, content_type="text/event-stream")

    # Existing Python scenario bodies and assertions run unchanged over real sockets.
    test_reasoning_then_answer = (
        chat_cases.ChatProjectionRegressions.test_chat_stream_reasoning_and_answer_have_distinct_items)
    test_reasoning_after_answer = (
        chat_cases.ChatProjectionRegressions.test_chat_stream_reasoning_after_answer_keeps_output_indices)
    test_text_then_tool_call = (
        chat_cases.ChatProjectionRegressions.test_chat_stream_message_closes_before_tool_call)
    test_refusal_and_usage = (
        chat_cases.ChatProjectionRegressions.test_refusal_stream_and_usage_only_tail)

    def test_browser_login_config_and_assets(self):
        for backend in self.backends:
            with self.subTest(port=backend.port):
                status, _, raw = backend.request("GET", "/")
                self.assertEqual(status, 200)
                self.assertEqual(raw, (ROOT / "easy_multi_provider/web/index.html").read_bytes())
                status, _, raw = backend.request("GET", "/api/config", auth=False)
                self.assertEqual(status, 401)
                status, _, raw = backend.request("GET", "/api/config")
                self.assertEqual(status, 200)
                self.assertNotIn(b"e2e-provider-token", raw)
                self.assertNotIn(b"e2e-native-token", raw)
                config = json.loads(raw)
                self.assertEqual(len(config["models"]), 4)
                self.assertEqual(config["emp_version"], "0.11.6")

    def test_existing_service_rejects_a_second_python_or_rust_owner(self):
        for owner in self.backends:
            before = owner.config_path.read_bytes()
            for command in ([sys.executable, "-m", "easy_multi_provider"],
                            [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]):
                with self.subTest(owner=owner.port, contender=command[-1]):
                    result = subprocess.run(
                        command + ["serve", "--config", str(owner.config_path),
                                   "--host", "127.0.0.1", "--port", "0"],
                        env=owner.environment, cwd=ROOT, capture_output=True, timeout=8,
                    )
                    self.assertEqual(result.returncode, 1)
                    self.assertIn(b"another EMP service owns this configuration", result.stderr)
                    self.assertEqual(owner.config_path.read_bytes(), before)
                    self.assertEqual(owner.request("GET", "/healthz")[0], 200)

    def test_offline_doctor_and_restore_match_python_commands(self):
        # Exercise test_integration_cli's native/active/restore/repeated-restore
        # flows through executables instead of importing either CLI dispatcher.
        all_outputs = []
        with tempfile.TemporaryDirectory(prefix="emp-offline-e2e-") as temporary:
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                root = Path(temporary) / name
                root.mkdir()
                home = root / "codex"
                home.mkdir()
                state = root / "offline-state"
                environment = {**os.environ, "CODEX_HOME": str(home), "PYTHONPATH": str(ROOT)}
                config = home / "config.toml"
                config.write_text('# offline preferences\nopenai_base_url = "native"\n')
                outputs = []

                def invoke(operation, as_json=False):
                    result = subprocess.run(
                        command + [operation, "--state-dir", "offline-state"]
                        + (["--json"] if as_json else []),
                        cwd=root, env=environment, capture_output=True, text=True, timeout=8,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                    self.assertEqual(result.stderr, "")
                    if as_json:
                        payload = json.loads(result.stdout)
                        last = payload.get("runtime", {}).get("last_known")
                        if last is not None:
                            self.assertIsInstance(last["observed_at"], str)
                            last["observed_at"] = "<generated timestamp>"
                        outputs.append(payload)
                    else:
                        outputs.append(result.stdout)

                invoke("doctor")
                invoke("doctor", True)
                manager = IntegrationManager(config, state / "lease.json",
                                             instance_id="e2e", lock_path=state / "lease.lock")
                manager.enable("http://127.0.0.1:43123/v1", "fixture-catalog.json", True)
                invoke("doctor", True)
                invoke("restore", True)
                invoke("doctor", True)
                invoke("restore")
                self.assertEqual(tomlkit.parse(config.read_text())["openai_base_url"], "native")
                recovery = json.loads((state / "runtime.json").read_text())
                if os.name == "posix":
                    self.assertEqual(state.stat().st_mode & 0o777, 0o700)
                    self.assertEqual((state / "runtime.json").stat().st_mode & 0o777, 0o600)
                recovery["updated_at"] = "<generated timestamp>"
                outputs.append(recovery)
                all_outputs.append(outputs)
        self.assertEqual(all_outputs[0], all_outputs[1])

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "existing Codex Unix socket fixture")
    def test_runtime_verification_reads_the_actual_shared_catalog(self):
        # Reuse the Python control-socket fixture: real initialize/model-list
        # messages, pagination, and no process-stop or model-generation command.
        for stale_name in (False, True):
            results = []
            with tempfile.TemporaryDirectory(prefix="e-", dir="/tmp") as temporary:
                for name, command in [
                    ("python", [sys.executable, "-m", "easy_multi_provider"]),
                    ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
                ]:
                    with self.subTest(backend=name, stale_name=stale_name):
                        backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                        try:
                            status, _, raw = backend.request(
                                "POST", "/api/integration/enable", {"confirm_reload": True})
                            self.assertEqual(status, 200, raw)
                            enabled = json.loads(raw)
                            self.assertEqual(enabled["runtime"]["state"], "stopped_waiting_for_start")
                            # Valid native login uses remote catalog discovery,
                            # so no static catalog override is written.
                            self.assertNotIn("model_catalog_json", tomlkit.parse(backend.codex_config.read_text()))
                            status, _, raw = backend.request("GET", "/v1/models?client_version=0.155.0")
                            self.assertEqual(status, 200, raw)
                            models = [{"id": row["slug"], "displayName": row["display_name"],
                                       "description": row.get("description") or ""}
                                      for row in json.loads(raw)["models"]]
                            if stale_name:
                                models[0]["displayName"] = "Old model name"
                            pages = {"": {"data": models[:1], "nextCursor": "next"},
                                     "next": {"data": models[1:]}}
                            with _UnixModelListServer(backend.codex_config.parent, pages) as control:
                                status, _, raw = backend.request("POST", "/api/integration/verify", {})
                                self.assertEqual(status, 200, raw)
                                verified = json.loads(raw)
                            self.assertEqual([row["method"] for row in control.requests],
                                             ["initialize", "initialized", "model/list", "model/list"])
                            runtime = verified["runtime"]
                            self.assertEqual(runtime["state"], "reload_required" if stale_name else "emp_loaded")
                            self.assertEqual(runtime["verified"], not stale_name)
                            results.append({"runtime": runtime, "next_action": verified["next_action"],
                                            "configuration": verified["configuration"]})
                        finally:
                            backend.close()
            self.assertEqual(results[0], results[1])

    def test_management_image_and_request_limits_match_python(self):
        for path in ("/api/models/vision-test-image", "/api/request-limits"):
            results = []
            for backend in self.backends:
                with self.subTest(path=path, port=backend.port):
                    status, _, raw = backend.request("GET", path, auth=False)
                    self.assertEqual(status, 401, raw)
                    status, headers, raw = backend.request("GET", path)
                    self.assertEqual(status, 200, raw)
                    payload = json.loads(raw)
                    if path == "/api/request-limits":
                        run_id = payload.pop("run_id")
                        self.assertRegex(run_id, r"^[0-9a-f]{32}$")
                    else:
                        self.assertEqual(headers.get("cache-control"), "no-store")
                    results.append(payload)
            self.assertEqual(results[0], results[1])

    def test_complete_chat_reasoning_usage_and_compressed_input(self):
        result = self.compare_exchange(
            {"model": "test/model", "input": "hello", "reasoning": {"effort": "low"}},
            {"choices": [{"message": {"reasoning_content": "Check the sum.", "content": "Four."},
                          "finish_reason": "stop"}], "usage": self.usage},
            compressed=True,
        )
        self.assertEqual(result["status"], "completed")
        self.assertEqual(result["output_text"], "Four.")
        self.assertEqual(result["usage"], self.expected_usage)
        self.assertEqual([item["type"] for item in result["output"]], ["reasoning", "message"])

    def test_complete_anthropic_tool_pairing(self):
        result = self.compare_exchange(
            {"model": "anthropic/model", "input": "hello",
             "tools": [{"type": "function", "name": "calculator",
                        "parameters": {"type": "object", "properties": {"x": {"type": "number"}}}}]},
            {"id": "upstream-message", "type": "message", "role": "assistant",
             "content": [{"type": "text", "text": "Calculating."},
                         {"type": "tool_use", "id": "tool_fixture", "name": "calculator",
                          "input": {"x": 4}}],
             "stop_reason": "tool_use", "usage": {"input_tokens": 3, "output_tokens": 2}},
        )
        self.assertEqual(result["output"][-1]["call_id"], "tool_fixture")
        self.assertEqual(json.loads(result["output"][-1]["arguments"]), {"x": 4})

    def test_native_response_retains_unknown_fields_and_model(self):
        upstream = {"id": "upstream-response", "object": "response", "status": "completed",
                    "model": "upstream-native-alias", "output": [],
                    "future_native_field": {"opaque": ["retained"]}}
        result = self.compare_exchange(
            {"model": "native/model", "input": "hello"}, upstream)
        self.assertEqual(result, upstream)

    def test_upstream_503_is_visible_without_replay(self):
        self.compare_exchange(
            {"model": "test/model", "input": "hello"},
            {"error": {"message": "temporarily unavailable", "type": "server_error"}},
            status=503,
        )


    def test_quit_stops_the_actual_process(self):
        with tempfile.TemporaryDirectory(prefix="emp-quit-e2e-") as temporary:
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                with self.subTest(backend=name):
                    backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                    try:
                        status, _, raw = backend.request("POST", "/api/quit", {})
                        self.assertEqual(status, 200, raw)
                        self.assertEqual(json.loads(raw), {"status": "stopping"})
                        self.assertEqual(backend.process.wait(timeout=8), 0)
                    finally:
                        backend.close()

    def test_empty_picker_cannot_enable_integration(self):
        with tempfile.TemporaryDirectory(prefix="emp-empty-e2e-") as temporary:
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                with self.subTest(backend=name):
                    backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                    try:
                        original = backend.codex_config.read_bytes()
                        _, _, raw = backend.request("GET", "/api/config")
                        config = json.loads(raw)
                        config["models"] = []
                        config["port"] = backend.port
                        status, _, raw = backend.request("POST", "/api/config", config)
                        self.assertEqual(status, 200, raw)
                        status, _, raw = backend.request(
                            "POST", "/api/integration/enable", {"confirm_reload": True})
                        self.assertEqual(status, 409, raw)
                        self.assertEqual(json.loads(raw)["error"]["code"], "empty_emp_catalog")
                        self.assertEqual(backend.codex_config.read_bytes(), original)
                        self.assertFalse((backend.codex_config.parent / "easy-multi-provider/integration/lease.json").exists())
                    finally:
                        backend.close()

    @unittest.skipUnless(os.name == "posix", "POSIX termination contract")
    def test_sigterm_restores_owned_integration(self):
        with tempfile.TemporaryDirectory(prefix="emp-stop-e2e-") as temporary:
            restored = []
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                with self.subTest(backend=name):
                    backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                    try:
                        original = backend.codex_config.read_text()
                        status, _, raw = backend.request(
                            "POST", "/api/integration/enable", {"confirm_reload": True})
                        self.assertEqual(status, 200, raw)
                        self.assertIn(f"http://127.0.0.1:{backend.port}/v1",
                                      backend.codex_config.read_text())
                        backend.process.terminate()
                        self.assertEqual(backend.process.wait(timeout=8), 0)
                        result = backend.codex_config.read_text()
                        self.assertEqual(tomlkit.parse(result), tomlkit.parse(original))
                        self.assertIn("# user settings", result)
                        restored.append(result)
                    finally:
                        backend.close()
            self.assertEqual(restored[0], restored[1])

    def test_websocket_turn_uses_same_stream_contract(self):
        chunks = [{"choices": [{"delta": {"content": "Four."}, "finish_reason": "stop"}]}]
        wire = ("data: " + json.dumps(chunks[0]) + "\n\ndata: [DONE]\n\n").encode()
        results = []
        for backend in self.backends:
            self.upstream.configure(wire, content_type="text/event-stream")
            with socket.create_connection(("127.0.0.1", backend.port), timeout=8) as client:
                with client.makefile("rb") as reader:
                    client.sendall((
                        f"GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:{backend.port}\r\n"
                        "Upgrade: websocket\r\nConnection: Upgrade\r\nSec-WebSocket-Version: 13\r\n"
                        "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                        f"Cookie: {backend.cookie}\r\n\r\n").encode())
                    self.assertIn(b" 101 ", reader.readline())
                    while reader.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    client.sendall(_masked_text_frame(json.dumps({
                        "type": "response.create", "model": "test/model", "input": "hello",
                        "stream": True})))
                    events = []
                    while not events or events[-1].get("type") != "response.completed":
                        opcode, text = _read_text_frame(reader)
                        self.assertEqual(opcode, 1)
                        event = json.loads(text)
                        if event.get("type") in ("response.metadata", "codex.response.metadata"):
                            continue
                        events.append(event)
                    results.append(events)
            self.upstream.requests.get(timeout=5)
        self.assertEqual(normalized_ids(results[0]), normalized_ids(results[1]))
        self.assertEqual(results[1][-1]["response"]["output_text"], "Four.")

    def test_integration_preserves_multiline_preferences_and_quoted_keys(self):
        # Extend test_integration's real-user TOML round trip with instructions
        # containing managed-looking text. Those lines are string data.
        original = (
            '# keep this header\n'
            '"openai_base_url"   = "native"  # keep this inline comment\n'
            "instructions = '''Read this example literally:\n"
            'openai_base_url = "this is instruction text"\n'
            "[example]\nend of instructions'''\n"
            '\n[nested]\nopenai_base_url = "nested-value"\nenabled = true\n'
        )
        restored = []
        with tempfile.TemporaryDirectory(prefix="emp-preferences-e2e-") as temporary:
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                with self.subTest(backend=name):
                    backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                    try:
                        backend.codex_config.write_text(original)
                        status, _, raw = backend.request(
                            "POST", "/api/integration/enable", {"confirm_reload": True})
                        self.assertEqual(status, 200, raw)
                        applied = tomlkit.parse(backend.codex_config.read_text())
                        self.assertEqual(applied["openai_base_url"],
                                         f"http://127.0.0.1:{backend.port}/v1")
                        self.assertEqual(applied["instructions"],
                                         tomlkit.parse(original)["instructions"])
                        self.assertEqual(applied["nested"], tomlkit.parse(original)["nested"])
                        status, _, raw = backend.request(
                            "POST", "/api/integration/restore", {"confirm_reload": True})
                        self.assertEqual(status, 200, raw)
                        restored.append(backend.codex_config.read_text())
                        self.assertEqual(tomlkit.parse(restored[-1]), tomlkit.parse(original))
                    finally:
                        backend.close()
        self.assertEqual(restored[0], restored[1])


if __name__ == "__main__":
    unittest.main()
