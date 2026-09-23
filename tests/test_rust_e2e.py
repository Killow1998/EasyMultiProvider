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
import time
import tomlkit
import unittest
from datetime import datetime
from contextlib import ExitStack

import zstandard

from tests import test_chat_projection_regressions as chat_cases
from tests import test_context_guard as context_cases
from tests.test_tool_bridge import function, namespace
from easy_multi_provider.tool_bridge import ExternalTools
from tests.test_server import _masked_text_frame, _read_text_frame
from tests.test_shared_app_server_runtime import _UnixModelListServer
from tests.rust_e2e_support import ROOT, EmpProcess, Upstream, normalized_ids
from easy_multi_provider.integration import IntegrationManager


def normalize_observation_times(value):
    if isinstance(value, list):
        return [normalize_observation_times(item) for item in value]
    if isinstance(value, dict):
        result = {}
        for key, item in value.items():
            if (key == "observed_at" or key.endswith("_observed_at")) and isinstance(item, str):
                datetime.fromisoformat(item.replace("Z", "+00:00"))
                result[key] = "<observation timestamp>"
            else:
                result[key] = normalize_observation_times(item)
        return result
    return value


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
                self.assertEqual(config["emp_version"], "0.11.10")

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

    def test_startup_recovers_only_its_own_listener_and_catalog(self):
        from easy_multi_provider.catalog import generated_catalog_path
        for mismatch in (True, False):
            results = []
            with tempfile.TemporaryDirectory(prefix="emp-recovery-") as temporary:
                root = Path(temporary)
                for name, command in [
                    ("python", [sys.executable, "-m", "easy_multi_provider"]),
                    ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
                ]:
                    home = root / name
                    home.mkdir()
                    config_path = home / "emp.json"
                    config_path.write_text(json.dumps({"native_catalog_path": str(home / "native.json")}))
                    (home / "native.json").write_text('{"models":[]}')
                    codex_config = home / "config.toml"
                    codex_config.write_text('# original\nmodel = "fixture-native"\n')
                    lease_path = home / "easy-multi-provider/integration/lease.json"
                    with socket.socket() as reservation:
                        reservation.bind(("127.0.0.1", 0))
                        port = reservation.getsockname()[1]
                    manager = IntegrationManager(codex_config, lease_path)
                    target = "http://127.0.0.1:%d/v1" % (1 if mismatch else port)
                    catalog = "wrong-catalog.json" if mismatch else str(generated_catalog_path(home))
                    manager.enable(target, catalog, True)
                    original_lease = lease_path.read_bytes()
                    applied = codex_config.read_bytes()
                    backend = EmpProcess.from_config(command, config_path, home, port=port)
                    try:
                        status, _, raw = backend.request("GET", "/api/integration")
                        self.assertEqual(status, 200, raw)
                        summary = json.loads(raw)
                        if mismatch:
                            self.assertEqual(summary["configuration"]["state"], "conflict")
                            self.assertEqual(summary["configuration"]["conflicts"],
                                             ["listener_mismatch", "catalog_mismatch"])
                            self.assertEqual(lease_path.read_bytes(), original_lease)
                        else:
                            self.assertEqual(summary["configuration"]["state"], "emp_applied")
                            self.assertEqual(summary["runtime"]["state"], "reload_required")
                            self.assertEqual(summary["runtime"]["confidence"], "pending")
                        results.append((summary["configuration"], summary["runtime"], summary["next_action"]))
                    finally:
                        backend.close()
                    if mismatch:
                        self.assertEqual(codex_config.read_bytes(), applied)
                        self.assertEqual(lease_path.read_bytes(), original_lease)
                    else:
                        self.assertEqual(codex_config.read_text(), '# original\nmodel = "fixture-native"\n')
                self.assertEqual(results[0], results[1])

    def test_subscription_search_enable_restore_and_external_edit(self):
        # Same user-owned TOML as test_search_integration, exercised via HTTP.
        original = '# user preferences\nmodel = "native"\n[features]\nunified_exec = true\n'
        for edited in (False, True):
            results = []
            with tempfile.TemporaryDirectory(prefix="emp-search-") as temporary:
                for name, command in [
                    ("python", [sys.executable, "-m", "easy_multi_provider"]),
                    ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
                ]:
                    backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                    try:
                        backend.codex_config.write_text(original)
                        status, _, raw = backend.request("GET", "/api/config")
                        self.assertEqual(status, 200, raw)
                        config = json.loads(raw)
                        config["port"] = backend.port
                        config["subscription_search"] = {"enabled": True}
                        status, _, raw = backend.request("POST", "/api/config", config)
                        self.assertEqual(status, 200, raw)
                        status, _, raw = backend.request("POST", "/api/integration/enable", {"confirm_reload": True})
                        self.assertEqual(status, 200, raw)
                        parsed = tomlkit.parse(backend.codex_config.read_text())
                        self.assertEqual(parsed["web_search"], "live")
                        self.assertTrue(parsed["features"]["standalone_web_search"])
                        self.assertTrue(parsed["features"]["unified_exec"])
                        search_lease = backend.codex_config.parent / "easy-multi-provider/integration/search.json"
                        lease = json.loads(search_lease.read_text())
                        self.assertEqual(lease["config_path"], str(backend.codex_config))
                        applied = {key: lease[key] for key in ("schema", "version", "status", "original", "applied")}
                        if edited:
                            parsed["web_search"] = "disabled"
                            backend.codex_config.write_text(tomlkit.dumps(parsed))
                            before = backend.codex_config.read_bytes()
                        status, _, raw = backend.request("POST", "/api/integration/restore", {"confirm_reload": True})
                        self.assertEqual(status, 409 if edited else 200, raw)
                        if edited:
                            self.assertEqual(backend.codex_config.read_bytes(), before)
                            results.append((applied, status, json.loads(raw)))
                        else:
                            final = tomlkit.parse(backend.codex_config.read_text())
                            self.assertNotIn("web_search", final)
                            self.assertNotIn("standalone_web_search", final["features"])
                            self.assertTrue(final["features"]["unified_exec"])
                            self.assertEqual(json.loads(search_lease.read_text())["status"], "restored")
                            results.append((applied, backend.codex_config.read_text()))
                    finally:
                        backend.close()
                self.assertEqual(results[0], results[1])

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "existing Codex Unix socket fixture")
    def test_integration_reload_syncs_search_before_probe_and_verify_is_passive(self):
        original = '# user preferences\nmodel = "native"\n[features]\nunified_exec = true\n'
        results = []
        with tempfile.TemporaryDirectory(prefix="emp-search-reload-") as temporary:
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                backend = EmpProcess(command, self.upstream, Path(temporary) / name)
                try:
                    backend.codex_config.write_text(original)
                    status, _, raw = backend.request("GET", "/api/config")
                    self.assertEqual(status, 200, raw)
                    config = json.loads(raw)
                    config["port"] = backend.port
                    config["subscription_search"] = {"enabled": True}
                    status, _, raw = backend.request("POST", "/api/config", config)
                    self.assertEqual(status, 200, raw)
                    status, _, raw = backend.request(
                        "POST", "/api/integration/enable", {"confirm_reload": True}
                    )
                    self.assertEqual(status, 200, raw)

                    config = json.loads(backend.request("GET", "/api/config")[2])
                    config["subscription_search"]["enabled"] = False
                    status, _, raw = backend.request("POST", "/api/config", config)
                    self.assertEqual(status, 200, raw)
                    before_reload = tomlkit.parse(backend.codex_config.read_text())
                    self.assertEqual(before_reload["web_search"], "live")
                    self.assertTrue(before_reload["features"]["standalone_web_search"])

                    status, _, raw = backend.request(
                        "GET", "/v1/models?client_version=0.156.1"
                    )
                    self.assertEqual(status, 200, raw)
                    models = [
                        {
                            "id": row["slug"],
                            "displayName": row["display_name"],
                            "description": row.get("description") or "",
                        }
                        for row in json.loads(raw)["models"]
                    ]
                    observations = []

                    class ObservedPages(dict):
                        def __getitem__(self, key):
                            current = tomlkit.parse(backend.codex_config.read_text())
                            observations.append(
                                (
                                    current.get("web_search"),
                                    current.get("features", {}).get(
                                        "standalone_web_search"
                                    ),
                                )
                            )
                            return super().__getitem__(key)

                    pages = ObservedPages(
                        {
                            "": {"data": models[:1], "nextCursor": "next"},
                            "next": {"data": models[1:]},
                        }
                    )
                    with _UnixModelListServer(backend.codex_config.parent, pages) as control:
                        status, _, raw = backend.request(
                            "POST", "/api/integration/reload", {"confirm_reload": True}
                        )
                        self.assertEqual(status, 200, raw)
                        reloaded = json.loads(raw)
                    self.assertEqual(
                        [row["method"] for row in control.requests],
                        ["initialize", "initialized", "model/list", "model/list"],
                    )
                    self.assertTrue(observations)
                    self.assertTrue(
                        all(fields == (None, None) for fields in observations),
                        "reload must reconcile search settings before probing Codex",
                    )
                    restored = tomlkit.parse(backend.codex_config.read_text())
                    self.assertNotIn("web_search", restored)
                    self.assertNotIn(
                        "standalone_web_search", restored["features"]
                    )
                    self.assertTrue(restored["features"]["unified_exec"])

                    restored["web_search"] = "external-edit"
                    backend.codex_config.write_text(tomlkit.dumps(restored))
                    before_verify = backend.codex_config.read_bytes()
                    control_socket = (
                        backend.codex_config.parent
                        / "app-server-control"
                        / "app-server-control.sock"
                    )
                    control_socket.unlink(missing_ok=True)
                    control_socket.parent.rmdir()
                    with _UnixModelListServer(backend.codex_config.parent, pages):
                        status, _, raw = backend.request(
                            "POST", "/api/integration/verify", {}
                        )
                        self.assertEqual(status, 200, raw)
                    self.assertEqual(
                        backend.codex_config.read_bytes(),
                        before_verify,
                        "verify is passive and must not repair external Codex edits",
                    )
                    verified_config = tomlkit.parse(backend.codex_config.read_text())
                    results.append(
                        (
                            reloaded["configuration"]["state"],
                            tuple(observations),
                            restored.get("web_search"),
                            restored["features"].get("standalone_web_search"),
                            verified_config.get("web_search"),
                        )
                    )
                finally:
                    backend.close()
            self.assertEqual(results[0], results[1])

    def test_command_help_matches_python(self):
        source = "import sys; sys.argv[0]='EMP'; from easy_multi_provider.main import main; raise SystemExit(main())"
        for arguments in (["--help"], ["serve", "--help"], ["doctor", "--help"], ["restore", "--help"], ["--version"]):
            results = []
            for command in ([sys.executable, "-c", source], [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]):
                result = subprocess.run(command + arguments, cwd=ROOT, capture_output=True, timeout=8)
                results.append((result.returncode, result.stdout, result.stderr))
            self.assertEqual(results[0], results[1])

    @unittest.skipUnless(os.name == "posix", "browser fixture requires an executable script")
    def test_desktop_launch_and_configured_listener_defaults(self):
        # Packaged Python's no-argument entry is the behavior oracle for native EMP.
        frozen_entry = "import sys; sys.frozen=True; from easy_multi_provider.main import main; raise SystemExit(main())"
        for desktop in (False, True):
            for name, command in [
                ("python", [sys.executable, "-c", frozen_entry] if desktop else [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                with self.subTest(backend=name, desktop=desktop), tempfile.TemporaryDirectory(prefix="emp-desktop-") as temporary:
                    root = Path(temporary)
                    home = root / "codex"
                    home.mkdir()
                    user_home = root / "user"
                    user_home.mkdir()
                    config_root = root / "Desktop settings"
                    config_path = config_root / "easy-multi-provider/config.json"
                    config_path.parent.mkdir(parents=True)
                    browser_log = root / "browser-url.txt"
                    browser = root / "browser"
                    browser.write_text("#!" + sys.executable + "\nimport sys\nfrom pathlib import Path\nPath(" + repr(str(browser_log)) + ").write_text(sys.argv[1])\n")
                    browser.chmod(0o700)
                    with socket.socket() as reservation:
                        reservation.bind(("127.0.0.1", 0))
                        port = reservation.getsockname()[1]
                    (root / "native.json").write_text('{"models":[]}')
                    config_path.write_text(json.dumps({"host": "127.0.0.1", "port": port,
                        "native_catalog_path": str(root / "native.json")}))
                    arguments = [] if desktop else ["serve", "--config", str(config_path)]
                    environment = {"HOME": str(user_home), "XDG_CONFIG_HOME": str(config_root), "BROWSER": str(browser)}
                    # macOS uses Application Support rather than XDG.
                    if sys.platform == "darwin" and desktop:
                        configured = user_home / "Library/Application Support/EasyMultiProvider/config.json"
                        configured.parent.mkdir(parents=True)
                        configured.write_bytes(config_path.read_bytes())
                        config_path = configured
                    backend = EmpProcess.from_config(command, config_path, home,
                        environment_overrides=environment, arguments=arguments)
                    try:
                        self.assertEqual(backend.port, port)
                        self.assertEqual(backend.request("GET", "/healthz")[0], 200)
                        if desktop:
                            deadline = time.monotonic() + 3
                            while not browser_log.exists() and time.monotonic() < deadline:
                                time.sleep(0.01)
                            self.assertTrue(browser_log.exists(), "desktop did not launch configured browser")
                            self.assertTrue(browser_log.read_text().startswith("http://127.0.0.1:%d/?bootstrap=" % port))
                        else:
                            self.assertFalse(browser_log.exists())
                    finally:
                        backend.close()

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

    def test_tool_namespace_choice_and_history_round_trip(self):
        # Reuse the Python tool-bridge scenario through actual HTTP endpoints.
        body = {"input": [{"type": "function_call", "namespace": "one", "name": "search",
                            "call_id": "call", "arguments": '{"name":"do not rewrite"}'},
                           {"type": "function_call_output", "call_id": "call", "output": "result"}],
                "tools": [namespace("one"), namespace("two"), function()],
                "tool_choice": {"type": "function", "namespace": "two", "name": "search"}}
        prepared = ExternalTools().prepare(body)
        alias = prepared["tools"][1]["name"]
        call = {"type": "function_call", "id": "fixture-item", "name": alias,
                "call_id": "fixture-call", "arguments": "{}", "status": "completed"}
        replies = {
            "test": {"choices": [{"finish_reason": "tool_calls", "message": {"role": "assistant",
                       "tool_calls": [{"id": "fixture-call", "type": "function",
                                       "function": {"name": alias, "arguments": "{}"}}]}}],
                     "usage": {"prompt_tokens": 10, "completion_tokens": 3, "total_tokens": 13}},
            "anthropic": {"id": "fixture", "type": "message", "role": "assistant",
                          "content": [{"type": "tool_use", "id": "fixture-call", "name": alias, "input": {}}],
                          "stop_reason": "tool_use", "usage": {"input_tokens": 10, "output_tokens": 3}},
            "responses": {"id": "fixture", "object": "response", "status": "completed", "output": [call]},
        }
        for name, reply in replies.items():
            with self.subTest(protocol=name):
                result = self.compare_exchange(dict(body, model=name + "/model"), reply)
                restored = next(item for item in result["output"] if item["type"] == "function_call")
                self.assertEqual((restored["name"], restored["namespace"]), ("search", "two"))

    @unittest.skipUnless(os.name == "posix", "executable fixture scripts require POSIX")
    def test_runtime_scan_selection_and_restart(self):
        # Installed layout fixtures are shared byte-for-byte by both backends.
        with tempfile.TemporaryDirectory(prefix="emp-runtimes-") as temporary:
            root = Path(temporary)
            home = root / "codex"
            home.mkdir()
            user_home = root / "user"
            paths = {
                home / "plugins/.plugin-appserver/codex": "0.154.0",
                home / "packages/standalone/current/bin/codex": "0.155.0",
                user_home / ".cursor/extensions/openai.chatgpt-fixture/bin/codex": "0.155.0-alpha.1",
                root / "bin/codex": "0.100.0",
            }
            for path, version in paths.items():
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("#!" + sys.executable + "\nprint('codex-cli " + version + "')\n")
                path.chmod(0o700)
            (root / "native.json").write_text('{"models":[]}')
            config_path = root / "config.json"
            environment = {"HOME": str(user_home), "PATH": str(root / "bin")}
            results = []
            for command in ([sys.executable, "-m", "easy_multi_provider"],
                            [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]):
                config_path.write_text(json.dumps({"native_catalog_path": str(root / "native.json")}))
                backend = EmpProcess.from_config(command, config_path, home, environment_overrides=environment)
                try:
                    # Python serve --port 0 is a test-only listener setting;
                    # restore its assigned port before any persisted config edit.
                    status, _, raw = backend.request("GET", "/api/config")
                    self.assertEqual(status, 200, raw)
                    config = json.loads(raw)
                    config["port"] = backend.port
                    status, _, raw = backend.request("POST", "/api/config", config)
                    self.assertEqual(status, 200, raw)
                    status, _, raw = backend.request("POST", "/api/runtime/scan", {})
                    self.assertEqual(status, 200, raw)
                    scanned = json.loads(raw)
                    self.assertEqual(scanned["helper_source"], "managed")
                    status, _, raw = backend.request("POST", "/api/runtime/select", {"sources": [" cursor ", "cursor"]})
                    self.assertEqual(status, 200, raw)
                    selected = json.loads(raw)
                    self.assertEqual(selected["preferences"], ["cursor"])
                    self.assertEqual(selected["helper_source"], "managed")
                    self.assertEqual([item["source"] for item in selected["runtimes"] if item["targeted"]], ["cursor"])
                    failures = []
                    for sources in ([], ["auto", "cursor"], ["path_cli"], [False]):
                        status, _, raw = backend.request("POST", "/api/runtime/select", {"sources": sources})
                        self.assertEqual(status, 400, raw)
                        failures.append(json.loads(raw))
                    self.assertEqual(json.loads(config_path.read_text())["codex_runtime_sources"], ["cursor"])
                finally:
                    backend.close()
                backend = EmpProcess.from_config(command, config_path, home, environment_overrides=environment)
                try:
                    status, _, raw = backend.request("GET", "/api/integration")
                    self.assertEqual(status, 200, raw)
                    restarted = json.loads(raw)["codex_compatibility"]
                    self.assertEqual(restarted, selected)
                    results.append((scanned, selected, failures, restarted))
                finally:
                    backend.close()
            self.assertEqual(results[0], results[1])

    def test_calibrated_context_budget_is_an_input_boundary(self):
        fixture = context_cases.ContextGuardTests()
        fixture.setUp()
        provider = dict(fixture.provider, protocol="chat_completions", base_url=self.upstream.base_url,
                        auth_mode="api_key", api_key="fixture-key")
        model = dict(fixture.model, provider="demo", output_limit=128)
        observation = context_cases.assess_context(provider, model, "chat_completions",
            {"messages": [{"role": "user", "content": "hello"}], "max_tokens": 128}).to_safe_dict()
        self.assertTrue(context_cases.update_calibration(model, observation, "explicit_failure", 1250))
        results = []
        with tempfile.TemporaryDirectory(prefix="emp-context-") as temporary:
            root = Path(temporary)
            (root / "native.json").write_text('{"models":[]}')
            for name, command in [
                ("python", [sys.executable, "-m", "easy_multi_provider"]),
                ("rust", [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())]),
            ]:
                home = root / name
                home.mkdir()
                config_path = home / "emp.json"
                config_path.write_text(json.dumps({"native_catalog_path": str(root / "native.json"),
                    "providers": [provider], "models": [model]}))
                backend = EmpProcess.from_config(command, config_path, home)
                try:
                    status, _, raw = backend.request("GET", "/api/capabilities")
                    self.assertEqual(status, 200, raw)
                    capabilities = json.loads(raw)
                    context = capabilities["capabilities"][0]["context"]
                    self.assertEqual(context["safe_input_limit"], 1249)
                    self.assertEqual(context["context_limit"], 4096)
                    self.assertEqual(context["source"], "observed")
                    reply = {"choices": [{"finish_reason": "stop", "message": {"role": "assistant", "content": "ok"}}]}
                    self.upstream.configure(reply)
                    status, _, raw = backend.request("POST", "/v1/responses",
                        {"model": "demo/model", "input": "x" * 2000, "max_output_tokens": 128})
                    self.assertEqual(status, 200, raw)
                    forwarded = self.upstream.requests.get(timeout=5)
                    # The active user turn cannot be silently truncated to fit.
                    status, _, raw = backend.request("POST", "/v1/responses",
                        {"model": "demo/model", "input": "x" * 3500, "max_output_tokens": 128})
                    self.assertGreaterEqual(status, 400, raw)
                    self.assertTrue(self.upstream.requests.empty(), "blocked input reached generation")
                    results.append((capabilities, forwarded[2], status, json.loads(raw)))
                finally:
                    backend.close()
            self.assertEqual(results[0], results[1])

    def test_management_image_and_request_limits_match_python(self):
        for path in ("/api/models/vision-test-image", "/api/request-limits", "/api/capabilities"):
            results = []
            for backend in self.backends:
                with self.subTest(path=path, port=backend.port):
                    status, _, raw = backend.request("GET", path, auth=False)
                    self.assertEqual(status, 401, raw)
                    status, headers, raw = backend.request("GET", path)
                    self.assertEqual(status, 200, raw)
                    payload = json.loads(raw)
                    if path == "/api/capabilities":
                        payload = normalize_observation_times(payload)
                    if path == "/api/request-limits":
                        run_id = payload.pop("run_id")
                        self.assertRegex(run_id, r"^[0-9a-f]{32}$")
                    elif path == "/api/models/vision-test-image":
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
        contexts = []
        for backend in self.backends:
            status, _, raw = backend.request("GET", "/api/capabilities")
            self.assertEqual(status, 200, raw)
            record = next(item for item in json.loads(raw)["capabilities"] if item["model_id"] == "native/model")
            self.assertIsNotNone(record["context"]["largest_success_estimate"])
            contexts.append(record["context"])
        self.assertEqual(contexts[0], contexts[1])

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
        for automatic in (False, True):
            with self.subTest(automatic=automatic):
                self._websocket_chat_turn(automatic)

    def _websocket_chat_turn(self, automatic):
        chunks = [{"choices": [{"delta": {"content": "Four."}, "finish_reason": "stop"}]}]
        wire = ("data: " + json.dumps(chunks[0]) + "\n\ndata: [DONE]\n\n").encode()
        results = []
        for backend in self.backends:
            if automatic:
                status, _, raw = backend.request("GET", "/api/config")
                self.assertEqual(status, 200, raw)
                config = json.loads(raw)
                config["port"] = backend.port
                next(provider for provider in config["providers"] if provider["id"] == "test")["protocol"] = "auto"
                status, _, raw = backend.request("POST", "/api/config", config)
                self.assertEqual(status, 200, raw)
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
