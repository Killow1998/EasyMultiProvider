import base64
import io
import json
import os
import signal
import socket
import struct
import tempfile
import threading
import unittest
from contextlib import redirect_stdout
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from urllib.error import HTTPError
from unittest.mock import patch
from pathlib import Path

import zstandard

from tests.support import ensure_test_master_key
from easy_multi_provider import __version__
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.codex_runtime import (
    HostStopResult,
    ProcessIdentity,
    STOP_FAILED,
    STOPPED_WAITING_FOR_START,
    CodexRuntimeController,
    RuntimeSyncResult,
    TargetedCodexHostStopper,
)
from easy_multi_provider.integration import IntegrationManager, ServiceNotReady
from easy_multi_provider.server import (
    AppState,
    WEB_FILE,
    BoundedThreadingHTTPServer,
    ObservationRing,
    _GracefulShutdown,
    _install_sigterm_handler,
    _restore_sigterm_handler,
    _integration_summary,
    _ws_replay_size,
    _WS_REPLAY_MAX_ITEMS,
    _WS_REPLAY_MAX_BYTES,
    configure_proxy_environment,
    make_handler,
    serve,
    startup_reconcile,
)


ensure_test_master_key()


class _NoResidualHosts:
    @staticmethod
    def stop_stale_codex_hosts():
        return HostStopResult("none")


def _write_stop_only_fake_codex(path: Path) -> Path:
    path.write_text(
        """#!/usr/bin/env python3
import json
import os
import sys
from pathlib import Path

args = sys.argv[1:]
home = Path(os.environ["CODEX_HOME"])
entry = {"args": args}
config_path = home / "config.toml"
if args == ["remote-control", "stop", "--json"]:
    entry["config_at_stop"] = config_path.read_text(encoding="utf-8")
    runtime_path = home / "easy-multi-provider" / "integration" / "runtime.json"
    if runtime_path.exists():
        entry["runtime_at_stop"] = json.loads(runtime_path.read_text(encoding="utf-8"))
    result = {"status": "stopped"}
    code = 0
elif args == ["app-server", "proxy"]:
    counter_path = home / "fake-proxy-count"
    try:
        count = int(counter_path.read_text(encoding="utf-8")) + 1
    except (FileNotFoundError, ValueError):
        count = 1
    counter_path.parent.mkdir(parents=True, exist_ok=True)
    counter_path.write_text(str(count), encoding="utf-8")
    ready_after = int(os.environ.get("EMP_FAKE_READY_AFTER", "999999"))
    if count >= ready_after:
        models = [
            {"id": model_id}
            for model_id in os.environ.get("EMP_FAKE_MODELS", "").split(",")
            if model_id
        ]
        print(json.dumps({"id": 2, "result": {"data": models}}))
        result = None
        code = 0
    else:
        print("connection refused", file=sys.stderr)
        result = None
        code = 1
else:
    result = None
    code = 97
with Path(os.environ["EMP_FAKE_CODEX_LOG"]).open("a", encoding="utf-8") as handle:
    handle.write(json.dumps(entry) + "\\n")
if result is not None:
    print(json.dumps(result))
sys.exit(code)
""",
        encoding="utf-8",
    )
    path.chmod(0o700)
    return path


def _integration_test_config(root: Path):
    return normalize(
        {
            "native_catalog_path": str(root / "native.json"),
            "providers": [
                {
                    "id": "external",
                    "base_url": "https://example.invalid/v1",
                    "protocol": "responses",
                }
            ],
            "models": [
                {
                    "id": "external/model-a",
                    "provider": "external",
                    "upstream_id": "model-a",
                    "enabled": True,
                }
            ],
        }
    )


class _WaitingRuntimeController:
    def reload(self, _expected_models, target, *, confirm_reload):
        if not confirm_reload:
            raise AssertionError("test transaction must be explicitly confirmed")
        return RuntimeSyncResult(
            STOPPED_WAITING_FOR_START,
            target,
            False,
            "controlled test runtime is absent",
        )


def _masked_text_frame(value):
    payload = value.encode("utf-8")
    mask = b"\x01\x02\x03\x04"
    if len(payload) < 126:
        header = bytes((0x81, 0x80 | len(payload)))
    else:
        header = bytes((0x81, 0x80 | 126)) + struct.pack("!H", len(payload))
    return header + mask + bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))


def _read_exact(stream, length):
    value = bytearray()
    while len(value) < length:
        chunk = stream.read(length - len(value))
        if not chunk:
            raise EOFError("websocket closed")
        value.extend(chunk)
    return bytes(value)


def _read_text_frame(stream):
    first, second = _read_exact(stream, 2)
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _read_exact(stream, 2))[0]
    elif length == 127:
        length = struct.unpack("!Q", _read_exact(stream, 8))[0]
    payload = _read_exact(stream, length)
    return first & 0x0F, payload.decode("utf-8")


class ServerAccountTests(unittest.TestCase):
    def test_runtime_controller_targets_integration_manager_codex_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "active-integration-home"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="home-scope",
            )

            with patch.dict(
                os.environ,
                {"CODEX_HOME": str(root / "unrelated-shell-home")},
            ):
                runtime = CodexRuntimeController(
                    target_codex_home=root / "unrelated-injected-home"
                )
                state = AppState(
                    root / "config.json",
                    integration_manager=manager,
                    runtime_controller=runtime,
                )

            self.assertEqual(
                state.runtime_controller.target_codex_home,
                os.path.normcase(os.path.realpath(str(codex_home.resolve()))),
            )

    def test_runtime_failure_api_and_durable_record_do_not_leak_codex_home(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "private-home-marker"
            config_path = root / "config.json"
            log_path = root / "fake-codex.jsonl"
            save(_integration_test_config(root), config_path)
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="no-home-leak",
            )
            normalized_home = os.path.normcase(
                os.path.realpath(str(codex_home.resolve()))
            )
            identity = ProcessIdentity(
                42004,
                0,
                "test-user",
                1.0,
                "/opt/codex",
                ("/opt/codex", "remote-control", "--json"),
                normalized_home,
            )

            class DeniedInventory:
                current_username = "test-user"

                @staticmethod
                def list_processes():
                    return (identity,)

                @staticmethod
                def terminate(_expected, _timeout):
                    return "denied"

            runtime = CodexRuntimeController(
                codex_executable=str(_write_stop_only_fake_codex(root / "codex-fake")),
                host_stopper=TargetedCodexHostStopper(
                    codex_home,
                    process_inventory=DeniedInventory(),
                    termination_timeout=0.1,
                ),
                control_timeout=0.1,
                observation_timeout=0.01,
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(log_path),
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/enable",
                        json.dumps({"confirm_reload": True}),
                        {
                            "Cookie": "emp_session=" + state.session_token,
                            "Content-Type": "application/json",
                        },
                    )
                    response = connection.getresponse()
                    payload_text = response.read().decode("utf-8")
                    connection.close()

                self.assertEqual(response.status, 409)
                payload = json.loads(payload_text)
                self.assertEqual(payload["runtime"]["state"], STOP_FAILED)
                runtime_path = codex_home / "easy-multi-provider" / "integration" / "runtime.json"
                durable_text = runtime_path.read_text(encoding="utf-8")
                for rendered in (payload_text, durable_text):
                    self.assertNotIn(str(codex_home), rendered)
                    self.assertNotIn(normalized_home, rendered)
                    self.assertNotIn("private-home-marker", rendered)
                    self.assertNotIn("CODEX_HOME", rendered)
                    self.assertNotIn("UNRELATED", rendered)
            finally:
                server.shutdown()
                server.server_close()

    def test_enable_rejects_empty_emp_catalog_before_config_or_stop(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            log_path = root / "fake-codex.jsonl"
            save(normalize({}), config_path)
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="empty-enable",
            )
            runtime = CodexRuntimeController(
                codex_executable=str(_write_stop_only_fake_codex(root / "codex-fake")),
                host_stopper=_NoResidualHosts(),
                control_timeout=0.1,
                observation_timeout=0.01,
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(log_path),
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/enable",
                        json.dumps({"confirm_reload": True}),
                        {
                            "Cookie": "emp_session=" + state.session_token,
                            "Content-Type": "application/json",
                        },
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode("utf-8"))
                    connection.close()

                self.assertEqual(response.status, 409)
                self.assertEqual(payload["error"]["code"], "empty_emp_catalog")
                self.assertFalse((codex_home / "config.toml").exists())
                self.assertFalse(log_path.exists())
            finally:
                server.shutdown()
                server.server_close()

    def test_enable_confirmation_applies_config_then_only_stops_runtime(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            log_path = root / "fake-codex.jsonl"
            fake_codex = _write_stop_only_fake_codex(root / "codex-fake")
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "name": "External",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model-a",
                                "provider": "external",
                                "upstream_id": "model-a",
                            }
                        ],
                    }
                ),
                config_path,
            )
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="endpoint-enable",
            )
            runtime = CodexRuntimeController(
                codex_executable=str(fake_codex),
                host_stopper=_NoResidualHosts(),
                control_timeout=0.1,
            )
            runtime.observation_timeout = 0.05
            runtime.poll_interval = 0.005
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {
                    "Cookie": "emp_session=" + state.session_token,
                    "Content-Type": "application/json",
                }
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(log_path),
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/enable",
                        json.dumps({"confirm_reload": True}),
                        headers,
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode("utf-8"))
                    connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "emp_applied")
                self.assertEqual(payload["runtime"]["state"], "stopped_waiting_for_start")
                calls = [
                    json.loads(line)
                    for line in log_path.read_text(encoding="utf-8").splitlines()
                ]
                self.assertEqual(calls[0]["args"], ["remote-control", "stop", "--json"])
                self.assertIn("openai_base_url", calls[0]["config_at_stop"])
                self.assertEqual(calls[0]["runtime_at_stop"]["state"], "stopping")
                self.assertNotIn(
                    "start",
                    " ".join(part for call in calls for part in call["args"]),
                )
                self.assertFalse(any("daemon" in call["args"] for call in calls))
            finally:
                server.shutdown()
                server.server_close()

    def test_enable_verifies_complete_delayed_catalog_and_persists_runtime_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            log_path = root / "fake-codex.jsonl"
            fake_codex = _write_stop_only_fake_codex(root / "codex-fake")
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "name": "External",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model-a",
                                "provider": "external",
                                "upstream_id": "model-a",
                            },
                            {
                                "id": "external/model-b",
                                "provider": "external",
                                "upstream_id": "model-b",
                            },
                        ],
                    }
                ),
                config_path,
            )
            lease_path = (
                codex_home / "easy-multi-provider" / "integration" / "lease.json"
            )
            manager = IntegrationManager(
                codex_home / "config.toml",
                lease_path,
                instance_id="delayed-enable",
            )
            runtime = CodexRuntimeController(
                codex_executable=str(fake_codex),
                host_stopper=_NoResidualHosts(),
                control_timeout=0.1,
                observation_timeout=0.5,
                poll_interval=0.005,
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {
                    "Cookie": "emp_session=" + state.session_token,
                    "Content-Type": "application/json",
                }
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(log_path),
                        "EMP_FAKE_READY_AFTER": "3",
                        "EMP_FAKE_MODELS": "native,external/model-a,external/model-b",
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/enable",
                        json.dumps({"confirm_reload": True}),
                        headers,
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode("utf-8"))
                    connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(payload["runtime"]["state"], "emp_loaded")
                self.assertTrue(payload["runtime"]["verified"])

                reopened = AppState(
                    config_path,
                    integration_manager=IntegrationManager(
                        codex_home / "config.toml",
                        lease_path,
                        instance_id="reopened",
                    ),
                    runtime_controller=runtime,
                )
                recovered = _integration_summary(reopened)
                self.assertEqual(recovered["runtime"]["state"], "not_checked")
                self.assertEqual(recovered["runtime"]["target"], "emp")
                self.assertFalse(recovered["runtime"]["verified"])
                self.assertEqual(recovered["runtime"]["confidence"], "stale")
                self.assertEqual(
                    recovered["runtime"]["last_known"]["state"], "emp_loaded"
                )
                self.assertTrue(recovered["runtime"]["last_known"]["verified"])
            finally:
                server.shutdown()
                server.server_close()

    def test_enable_keeps_applied_configuration_when_runtime_verification_warns(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            fake_codex = _write_stop_only_fake_codex(root / "codex-fake")
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model-a",
                                "provider": "external",
                                "upstream_id": "model-a",
                            },
                            {
                                "id": "external/model-b",
                                "provider": "external",
                                "upstream_id": "model-b",
                            },
                        ],
                    }
                ),
                config_path,
            )
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="partial-enable",
            )
            runtime = CodexRuntimeController(
                codex_executable=str(fake_codex),
                host_stopper=_NoResidualHosts(),
                control_timeout=0.1,
                observation_timeout=0.2,
                poll_interval=0.005,
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(root / "fake-codex.jsonl"),
                        "EMP_FAKE_READY_AFTER": "1",
                        "EMP_FAKE_MODELS": "native,external/model-a",
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/enable",
                        json.dumps({"confirm_reload": True}),
                        {
                            "Cookie": "emp_session=" + state.session_token,
                            "Content-Type": "application/json",
                        },
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode("utf-8"))
                    connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "emp_applied")
                self.assertEqual(payload["configuration"]["conflicts"], [])
                self.assertEqual(payload["runtime"]["state"], "verification_failed")
                self.assertFalse(payload["runtime"]["verified"])
                self.assertTrue(payload["runtime"]["action_required"])
            finally:
                server.shutdown()
                server.server_close()

    def test_restore_confirmation_writes_native_config_before_stop_and_accounts_relation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            codex_config = codex_home / "config.toml"
            codex_config.parent.mkdir(parents=True)
            codex_config.write_text(
                'openai_base_url = "https://native.invalid/v1"\n'
                'model_catalog_json = "/native/catalog.json"\n',
                encoding="utf-8",
            )
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model-a",
                                "provider": "external",
                                "upstream_id": "model-a",
                            }
                        ],
                    }
                ),
                config_path,
            )
            lease_path = (
                codex_home / "easy-multi-provider" / "integration" / "lease.json"
            )
            manager = IntegrationManager(
                codex_config,
                lease_path,
                instance_id="restore-transaction",
            )
            manager.enable(
                "http://127.0.0.1:4201/v1",
                str(codex_home / "easy-multi-provider" / "catalog.json"),
                service_ready=True,
            )
            log_path = root / "fake-codex.jsonl"
            runtime = CodexRuntimeController(
                codex_executable=str(_write_stop_only_fake_codex(root / "codex-fake")),
                host_stopper=_NoResidualHosts(),
                control_timeout=0.1,
                observation_timeout=0.05,
                poll_interval=0.005,
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=runtime,
            )
            state._mark_runtime_pending("emp", "test fixture active EMP target")
            state.mark_service_ready()
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch.dict(
                    os.environ,
                    {
                        "CODEX_HOME": str(codex_home),
                        "EMP_FAKE_CODEX_LOG": str(log_path),
                    },
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/integration/restore",
                        json.dumps({"confirm_reload": True}),
                        {
                            "Cookie": "emp_session=" + state.session_token,
                            "Content-Type": "application/json",
                        },
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode("utf-8"))
                    connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "native")
                self.assertEqual(payload["runtime"]["state"], "stopped_waiting_for_start")
                calls = [
                    json.loads(line)
                    for line in log_path.read_text(encoding="utf-8").splitlines()
                ]
                self.assertEqual(calls[0]["args"], ["remote-control", "stop", "--json"])
                self.assertIn("https://native.invalid/v1", calls[0]["config_at_stop"])
                recovery = json.loads(
                    lease_path.with_name("runtime.json").read_text(encoding="utf-8")
                )
                self.assertEqual(recovery["target"], "native")
                self.assertEqual(recovery["configuration_relation"], "original")
            finally:
                server.shutdown()
                server.server_close()

    def test_web_exposes_runtime_sync_and_large_model_picker_controls(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertIn("应用模型变化到 Codex", html)
        self.assertIn("/api/integration/reload", html)
        self.assertIn(">全选</button>", html)
        self.assertIn(">全不选</button>", html)
        self.assertIn("全选搜索结果", html)
        self.assertIn("全不选搜索结果", html)
        self.assertIn("discovered_search", html)
        self.assertIn("discovered_count", html)

    def test_web_exposes_subscription_and_provider_visibility_controls(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertIn('name="subscription_model"', html)
        self.assertIn("hidden_models", html)
        self.assertIn("模型显示设置作用于原生列表", html)
        self.assertIn("toggleProviderModels", html)
        self.assertIn("隐藏全部模型", html)

    def test_web_does_not_expose_internal_tool_call_mode(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertNotIn("工具调用", html)
        self.assertNotIn("modal_provider_tools", html)

    def test_successful_auto_protocol_is_persisted(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({
                "providers": [{
                    "id": "demo",
                    "name": "Demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "auto",
                    "auth_mode": "api_key",
                    "api_key": "test-key",
                }],
                "models": [{
                    "id": "demo/model",
                    "provider": "demo",
                    "upstream_id": "model",
                }],
            }), config_path)
            state = AppState(config_path)
            with patch(
                "easy_multi_provider.server.proxy",
                return_value=(
                    {
                        "kind": "body",
                        "status": 200,
                        "content_type": "application/json",
                        "provider_id": "demo",
                        "resolved_protocol": "responses",
                    },
                    b"{}",
                ),
            ):
                state.route({"model": "demo/model", "input": "hello"}, {})
            provider = state.snapshot()["providers"][0]
            model = state.snapshot()["models"][0]
            self.assertEqual(provider["protocol"], "auto")
            self.assertEqual(provider["resolved_protocol"], "responses")
            self.assertEqual(provider["protocol_observation"]["source"], "observed")
            self.assertEqual(model["resolved_protocol"], "responses")
            self.assertEqual(api_key(provider), "test-key")

    def test_compact_endpoint_routes_remote_compaction(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                with patch(
                    "easy_multi_provider.server.proxy_compact",
                    return_value=(
                        {"kind": "body", "status": 200, "content_type": "application/json"},
                        b'{"output":[]}',
                    ),
                ) as routed:
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/v1/responses/compact",
                        b'{"model":"demo/fixed","input":[]}',
                        {
                            "Content-Type": "application/json",
                            "Cookie": "emp_session=" + state.session_token,
                        },
                    )
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    self.assertEqual(json.loads(response.read()), {"output": []})
                    connection.close()
                routed.assert_called_once()
            finally:
                server.shutdown()
                server.server_close()

    def test_zstd_compressed_proxy_request_is_decoded(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            body = b'{"model":"demo/fixed","input":"hello"}'
            encoded = zstandard.ZstdCompressor().compress(body)
            try:
                with patch(
                    "easy_multi_provider.server.proxy",
                    return_value=(
                        {"kind": "body", "status": 200, "content_type": "application/json"},
                        b"{}",
                    ),
                ) as routed:
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/v1/responses",
                        encoded,
                        {
                            "Content-Type": "application/json",
                            "Content-Encoding": "zstd",
                            "Cookie": "emp_session=" + state.session_token,
                        },
                    )
                    response = connection.getresponse()
                    self.assertEqual(response.status, 200)
                    response.read()
                    connection.close()
                self.assertEqual(routed.call_args.args[1]["input"], "hello")
            finally:
                server.shutdown()
                server.server_close()

    def test_responses_websocket_routes_response_create(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            response_id = "resp_websocket_test"
            events = [
                (
                    'event: response.created\ndata: {"type":"response.created",'
                    '"response":{"id":"%s"}}\n\n' % response_id
                ).encode(),
                (
                    'event: response.completed\ndata: {"type":"response.completed",'
                    '"response":{"id":"%s","usage":{"input_tokens":1,'
                    '"output_tokens":1,"total_tokens":2}}}\n\n' % response_id
                ).encode(),
            ]
            client = None
            stream = None
            try:
                with patch(
                    "easy_multi_provider.server.proxy",
                    return_value=(
                        {"kind": "stream", "status": 200, "content_type": "text/event-stream"},
                        iter(events),
                    ),
                ) as routed:
                    client = socket.create_connection(server.server_address, timeout=3)
                    stream = client.makefile("rb")
                    port = server.server_address[1]
                    client.sendall((
                        (
                            "GET /v1/responses HTTP/1.1\r\n"
                            "Host: 127.0.0.1:%d\r\n"
                            "Upgrade: websocket\r\n"
                            "Connection: Upgrade\r\n"
                            "Sec-WebSocket-Version: 13\r\n"
                            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                            "Cookie: emp_session=%s\r\n\r\n"
                        )
                        % (port, state.session_token)
                    ).encode("ascii"))
                    self.assertIn(b" 101 ", stream.readline())
                    while stream.readline() not in (b"\r\n", b"\n", b""):
                        pass
                    client.sendall(
                        _masked_text_frame(
                            json.dumps(
                                {
                                    "type": "response.create",
                                    "model": "demo/fixed",
                                    "input": "hello",
                                    "stream": True,
                                }
                            )
                        )
                    )
                    received = []
                    while not any(item.get("type") == "response.completed" for item in received):
                        opcode, text = _read_text_frame(stream)
                        self.assertEqual(opcode, 1)
                        received.append(json.loads(text))
                self.assertEqual(routed.call_count, 1)
                self.assertNotIn("type", routed.call_args.args[1])
            finally:
                if stream is not None:
                    stream.close()
                if client is not None:
                    client.close()
                server.shutdown()
                server.server_close()

    def test_system_proxy_is_imported_when_environment_has_none(self):
        proxy_keys = {
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "NO_PROXY",
            "no_proxy",
        }
        clean_environment = {
            key: value for key, value in os.environ.items() if key not in proxy_keys
        }
        settings = {
            "http": "http://127.0.0.1:7897",
            "https": "http://127.0.0.1:7897",
            "all": "socks5://127.0.0.1:7897",
            "no": "localhost,127.0.0.1",
        }
        with patch.dict(os.environ, clean_environment, clear=True), patch(
            "easy_multi_provider.server.getproxies", return_value={}
        ), patch("easy_multi_provider.server._gnome_proxy_settings", return_value=settings):
            self.assertEqual(configure_proxy_environment(), "system")
            self.assertEqual(os.environ["HTTPS_PROXY"], settings["https"])
            self.assertEqual(os.environ["ALL_PROXY"], settings["all"])
            self.assertEqual(os.environ["NO_PROXY"], settings["no"])

    def test_explicit_proxy_environment_wins(self):
        with patch.dict(os.environ, {"HTTPS_PROXY": "http://proxy.invalid"}, clear=True), patch(
            "easy_multi_provider.server.getproxies"
        ) as system_proxies:
            self.assertEqual(configure_proxy_environment(), "environment")
            system_proxies.assert_not_called()

    def test_proxy_requests_are_not_serialized_by_state_lock(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            server_thread = threading.Thread(target=server.serve_forever, daemon=True)
            server_thread.start()
            entered_upstream = threading.Barrier(2, timeout=2)
            statuses = []

            def fake_proxy(*args):
                entered_upstream.wait()
                return {"kind": "json", "status": 200, "content_type": "application/json"}, b"{}"

            def request():
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/v1/responses",
                    b'{"model":"demo/fixed","input":"hello"}',
                    {
                        "Content-Type": "application/json",
                        "Cookie": "emp_session=" + state.session_token,
                    },
                )
                statuses.append(connection.getresponse().status)
                connection.close()

            try:
                with patch("easy_multi_provider.server.proxy", side_effect=fake_proxy):
                    workers = [threading.Thread(target=request) for _ in range(2)]
                    for worker in workers:
                        worker.start()
                    for worker in workers:
                        worker.join(3)
                self.assertTrue(all(not worker.is_alive() for worker in workers))
                self.assertEqual(sorted(statuses), [200, 200])
            finally:
                server.shutdown()
                server.server_close()

    def test_account_delete_removes_only_private_account_copy(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize({"account_store_path": str(root / "state" / "accounts")}),
                config_path,
            )
            state = AppState(config_path)
            account = state.import_account(
                {"id": "primary", "name": "Primary", "prefix": "primary"},
                {"auth_mode": "chatgpt", "tokens": {"access_token": "account-secret"}},
            )
            auth_path = Path(account["auth_file"])
            self.assertTrue(auth_path.exists())
            state.delete_account("primary")
            self.assertFalse(auth_path.exists())
            self.assertFalse(auth_path.parent.exists())
            self.assertEqual(state.config["accounts"], [])
            self.assertTrue(config_path.exists())

    def test_web_config_update_keeps_api_key_out_of_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({"secret_store_path": str(root / "state" / "secrets")}), config_path)
            state = AppState(config_path)
            state.update({
                "providers": [{
                    "id": "demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "chat_completions",
                    "auth_mode": "api_key",
                    "api_key": "provider-secret",
                }],
                "models": [{"id": "demo/model", "provider": "demo"}],
            })
            self.assertNotIn("provider-secret", config_path.read_text(encoding="utf-8"))
            self.assertEqual(state.config["providers"][0]["api_key"], "")
            self.assertEqual(api_key(load(config_path)["providers"][0]), "provider-secret")

    def test_modal_submission_errors_are_visible_inside_modal(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertIn('<div id="status"', html)
        self.assertIn('id="modal_status"', html)
        self.assertIn("catch (error) { $('modal_status').textContent = error.message; }", html)
        self.assertIn("position:fixed", html)
        self.assertIn("/api/migration/export", html)
        self.assertIn("easy-multi-provider-0.3.0.emp", html)

    def test_web_codex_integration_uses_safe_state_actions(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        for label in ("Native", "EMP applied", "Recovery needed", "Conflict"):
            self.assertIn(label, html)
        self.assertIn('id="native_catalog_path"', html)
        self.assertIn("confirmIntegrationAction('enable')", html)
        self.assertIn("confirmIntegrationAction('restore')", html)
        self.assertIn("/api/integration/enable", html)
        self.assertIn("/api/integration/restore", html)
        self.assertIn("codex", html)
        self.assertIn("codex resume", html)
        self.assertIn("/model", html)
        self.assertIn("应用模型变化到 Codex", html)
        self.assertIn("/api/integration/reload", html)
        self.assertIn("await loadIntegration()", html)
        self.assertIn("const configuration = info.configuration || {}", html)
        self.assertIn("configuration.state === 'native'", html)
        self.assertIn("JSON.stringify({confirm_reload:true})", html)
        self.assertNotIn("generateIntegration", html)
        self.assertNotIn("integrationText", html)
        self.assertNotIn("snippet", html)
        self.assertNotIn("profile_path", html)
        self.assertNotIn("--profile emp", html)
        self.assertNotIn("生成 EMP 配置", html)
        self.assertNotIn("/api/integration/generate", html)
        self.assertNotIn("window.confirm", html)

    def test_web_includes_compact_safe_diagnostics_status(self):
        html = WEB_FILE.read_text(encoding="utf-8")
        self.assertIn("最近运行状态", html)
        self.assertIn("/api/diagnostics", html)
        self.assertIn("loadDiagnostics", html)
        self.assertIn("不保存消息内容、响应内容或凭据", html)
        self.assertIn("diagnosticsContextLabel", html)
        self.assertIn("context_decision", html)
        self.assertIn("safe_input_limit", html)
        self.assertNotIn("prompt", html[html.find("function renderDiagnostics"):html.find("function confirmIntegrationAction")])
        self.assertNotIn("response_text", html[html.find("function renderDiagnostics"):html.find("function confirmIntegrationAction")])

    def test_integration_api_failure_is_unavailable_without_configuration_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({}), config_path)
            manager = IntegrationManager(
                root / "codex" / "config.toml",
                root / "codex" / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="unavailable-handler",
            )
            state = AppState(config_path, integration_manager=manager)
            handler = object.__new__(make_handler(state))
            handler.path = "/api/integration"
            handler.headers = {}
            handler._management_allowed = lambda: True
            captured = {}
            handler._send = lambda status, body, *args, **kwargs: captured.update(
                status=status, body=body
            )

            with patch.object(
                state,
                "integration_status",
                side_effect=OSError("private-path-marker"),
            ):
                handler.do_GET()

            self.assertEqual(captured["status"], 503)
            payload = json.loads(captured["body"].decode("utf-8"))
            self.assertEqual(payload["error"]["code"], "integration_unavailable")
            self.assertEqual(payload["error"]["message"], "integration operation failed")
            self.assertNotIn("private-path-marker", json.dumps(payload))

    def test_successful_enable_clears_stale_startup_mismatch_with_runtime_warning(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model",
                                "provider": "external",
                                "upstream_id": "model",
                            }
                        ],
                    }
                ),
                config_path,
            )
            codex_home = root / "codex"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="clear-startup-mismatch",
            )

            class VerificationWarningRuntime:
                @staticmethod
                def reload(_expected_models, target, *, confirm_reload):
                    return RuntimeSyncResult(
                        "verification_failed",
                        target,
                        verified=False,
                        detail="fake catalog observation was incomplete",
                    )

            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=VerificationWarningRuntime(),
            )
            state.mark_service_ready()
            state.set_startup_conflicts(("listener_mismatch", "catalog_mismatch"))

            result = state.enable_integration(
                "http://127.0.0.1:43123/v1",
                confirm_reload=True,
            )

            self.assertTrue(result.ok)
            self.assertEqual(result.state, "active")
            self.assertEqual(state.startup_conflicts(), ())
            summary = _integration_summary(state)
            self.assertEqual(summary["configuration"]["state"], "emp_applied")
            self.assertEqual(summary["runtime"]["state"], "verification_failed")
            self.assertTrue(summary["runtime"]["action_required"])

    def test_enable_endpoint_returns_success_for_applied_config_with_runtime_warning(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "external",
                                "base_url": "https://example.invalid/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model",
                                "provider": "external",
                                "upstream_id": "model",
                            }
                        ],
                    }
                ),
                config_path,
            )
            manager = IntegrationManager(
                root / "codex" / "config.toml",
                root / "codex" / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="warning-response",
            )

            class VerificationWarningRuntime:
                @staticmethod
                def reload(_expected_models, target, *, confirm_reload):
                    return RuntimeSyncResult(
                        "verification_failed",
                        target,
                        verified=False,
                        detail="fake catalog observation was incomplete",
                    )

            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=VerificationWarningRuntime(),
            )
            state.mark_service_ready()
            handler = object.__new__(make_handler(state))
            handler.path = "/api/integration/enable"
            handler.headers = {}
            handler.server = type("FakeServer", (), {"server_address": ("127.0.0.1", 43123)})()
            handler._management_allowed = lambda: True
            handler._body = lambda _limit: {"confirm_reload": True}
            captured = {}
            handler._send = lambda status, body, *args, **kwargs: captured.update(
                status=status, body=body
            )

            handler.do_POST()

            payload = json.loads(captured["body"].decode("utf-8"))
            self.assertEqual(captured["status"], 200)
            self.assertEqual(payload["configuration"]["state"], "emp_applied")
            self.assertEqual(payload["configuration"]["conflicts"], [])
            self.assertEqual(payload["runtime"]["state"], "verification_failed")
            self.assertTrue(payload["runtime"]["action_required"])

    def test_restore_endpoint_returns_success_for_native_config_with_runtime_warning(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({}), config_path)
            codex_home = root / "codex"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="restore-warning-response",
            )
            manager.enable(
                "http://127.0.0.1:43123/v1",
                str((codex_home / "easy-multi-provider" / "catalog.json").resolve()),
                service_ready=True,
            )

            class VerificationWarningRuntime:
                @staticmethod
                def reload(_expected_models, target, *, confirm_reload):
                    return RuntimeSyncResult(
                        "verification_failed",
                        target,
                        verified=False,
                        detail="fake native catalog observation was incomplete",
                    )

            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=VerificationWarningRuntime(),
            )
            state.mark_service_ready()
            handler = object.__new__(make_handler(state))
            handler.path = "/api/integration/restore"
            handler.headers = {}
            handler.server = type("FakeServer", (), {"server_address": ("127.0.0.1", 43123)})()
            handler._management_allowed = lambda: True
            handler._body = lambda _limit: {"confirm_reload": True}
            captured = {}
            handler._send = lambda status, body, *args, **kwargs: captured.update(
                status=status, body=body
            )

            handler.do_POST()

            payload = json.loads(captured["body"].decode("utf-8"))
            self.assertEqual(captured["status"], 200)
            self.assertEqual(payload["configuration"]["state"], "native")
            self.assertEqual(payload["configuration"]["conflicts"], [])
            self.assertEqual(payload["runtime"]["state"], "verification_failed")
            self.assertTrue(payload["runtime"]["action_required"])

    def test_successful_startup_reconcile_clears_stale_mismatch(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({}), config_path)
            codex_home = root / "codex"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="startup-clear",
            )
            state = AppState(config_path, integration_manager=manager)
            base_url, catalog_path = (
                "http://127.0.0.1:43124/v1",
                str(state.integration_catalog_path.resolve()),
            )
            manager.enable(base_url, catalog_path, service_ready=True)
            state.set_startup_conflicts(("listener_mismatch", "catalog_mismatch"))

            class BoundServer:
                server_address = ("127.0.0.1", 43124)

                @staticmethod
                def fileno():
                    return 1

            result = startup_reconcile(state, BoundServer())

            self.assertEqual(result.action, "re_adopted")
            self.assertEqual(result.state, "active")
            self.assertEqual(state.startup_conflicts(), ())
            self.assertEqual(_integration_summary(state)["configuration"]["state"], "emp_applied")

    def test_startup_re_adoption_refreshes_catalog_without_runtime_restart(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            native_path = root / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-native",
                                "display_name": "Native",
                                "context_window": 272000,
                                "effective_context_window_percent": 95,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config_path = root / "config.json"
            save(normalize({"native_catalog_path": str(native_path)}), config_path)
            codex_home = root / "codex"
            catalog_path = codex_home / "easy-multi-provider" / "catalog.json"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="startup-refresh",
            )
            manager.enable(
                "http://127.0.0.1:43124/v1",
                str(catalog_path.resolve()),
                service_ready=True,
            )

            class NoRuntimeRestart:
                @staticmethod
                def reload(*_args, **_kwargs):
                    raise AssertionError("startup catalog refresh must not restart Codex")

            state = AppState(
                config_path,
                integration_manager=manager,
                catalog_path=catalog_path,
                runtime_controller=NoRuntimeRestart(),
            )

            class BoundServer:
                server_address = ("127.0.0.1", 43124)

                @staticmethod
                def fileno():
                    return 1

            result = startup_reconcile(state, BoundServer())

            self.assertEqual(result.action, "re_adopted")
            generated = json.loads(catalog_path.read_text(encoding="utf-8"))
            self.assertEqual(generated["models"][0]["display_name"], "Native [258K]")
            self.assertEqual(state.runtime_sync_snapshot()["state"], "reload_required")

    def test_integration_status_is_safe_and_handler_is_ready(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            save(normalize({}), config_path)
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="server-status",
            )
            state = AppState(config_path, integration_manager=manager)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {"Cookie": "emp_session=" + state.session_token}
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/integration", headers=headers)
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(
                    set(payload),
                    {"configuration", "runtime", "service_health", "next_action"},
                )
                self.assertEqual(payload["configuration"]["state"], "native")
                self.assertEqual(payload["configuration"]["relation"], "unleased")
                self.assertFalse(payload["configuration"]["config_exists"])
                self.assertEqual(payload["configuration"]["lease_status"], "none")
                self.assertEqual(payload["runtime"]["state"], "not_checked")
                self.assertEqual(payload["service_health"], "ready")
                self.assertNotIn(str(root), json.dumps(payload))
                self.assertNotIn("fields", payload)
                self.assertNotIn("lease", payload)
                self.assertNotIn("config_path", payload)

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/integration/generate",
                    b"{}",
                    {
                        "Cookie": "emp_session=" + state.session_token,
                        "Content-Type": "application/json",
                    },
                )
                removed = connection.getresponse()
                removed.read()
                connection.close()
                self.assertEqual(removed.status, 404)

                with patch.object(
                    state,
                    "integration_status",
                    side_effect=OSError("private-path-marker"),
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request("GET", "/api/integration", headers=headers)
                    failed = connection.getresponse()
                    error_payload = json.loads(failed.read().decode("utf-8"))
                    connection.close()
                self.assertEqual(failed.status, 503)
                self.assertEqual(error_payload["error"]["code"], "integration_unavailable")
                self.assertEqual(error_payload["error"]["message"], "integration operation failed")
                self.assertNotIn("private-path-marker", json.dumps(error_payload))
            finally:
                server.shutdown()
                server.server_close()

    def test_capabilities_endpoint_requires_management_session_and_is_safe(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize({
                    "providers": [{
                        "id": "demo",
                        "base_url": "https://example.com/v1?tenant=private",
                        "protocol": "responses",
                        "api_key": "provider-secret",
                    }],
                    "models": [{
                        "id": "demo/model",
                        "provider": "demo",
                        "upstream_id": "model",
                        "context_window": 128000,
                    }],
                }),
                config_path,
            )
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/capabilities")
                denied = connection.getresponse()
                denied.read()
                connection.close()
                self.assertEqual(denied.status, 401)

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/capabilities",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()
                self.assertEqual(response.status, 200)
                self.assertEqual(len(payload["capabilities"]), 1)
                serialized = json.dumps(payload)
                for value in (
                    "https://",
                    "tenant=private",
                    "provider-secret",
                    "base_url",
                    "api_key",
                ):
                    self.assertNotIn(value, serialized)
                record = payload["capabilities"][0]
                self.assertNotIn("base_url", record)
                self.assertEqual(
                    set(record["key"]),
                    {
                        "endpoint_fingerprint",
                        "upstream_model",
                        "protocol_identity",
                        "deployment_identity",
                    },
                )
            finally:
                server.shutdown()
                server.server_close()

    def test_observation_ring_is_bounded_thread_safe_and_redacted(self):
        ring = ObservationRing(3)

        def write(index):
            ring.record({
                "route": "responses",
                "provider_id": "demo",
                "model_id": "demo/model",
                "endpoint_fingerprint": "https://user:secret@example.com/v1?key=token",
                "protocol": "responses",
                "transport": "http",
                "request_bytes": index,
                "response_bytes": index + 1,
                "duration_ms": 1,
                "status": 200,
                "error_class": "none",
                "decision": "explicit",
                "prompt": "prompt-secret",
                "response": "response-secret",
                "tool_args": "tool-args-secret",
                "api_key": "api-key-secret",
                "raw_endpoint": "https://private.example/v1?key=secret",
                "upstream_html": "<html>private-body</html>",
                "path": "/private/config.json",
            })

        workers = [threading.Thread(target=write, args=(index,)) for index in range(20)]
        for worker in workers:
            worker.start()
        for worker in workers:
            worker.join()
        snapshot = ring.snapshot()
        self.assertEqual(snapshot["capacity"], 3)
        self.assertLessEqual(len(snapshot["records"]), 3)
        self.assertTrue(snapshot["records"])
        expected = {
            "observed_at",
            "route",
            "provider_id",
            "model_id",
            "endpoint_fingerprint",
            "deployment_identity",
            "protocol",
            "transport",
            "request_bytes",
            "response_bytes",
            "duration_ms",
            "status",
            "error_class",
            "fallback",
            "decision",
            "context_decision",
            "estimated_tokens",
            "context_limit",
            "safe_input_limit",
            "context_confidence",
            "context_source",
            "context_estimate_method",
            "context_reserves",
            "context_completeness",
        }
        for record in snapshot["records"]:
            self.assertEqual(set(record), expected)
        serialized = json.dumps(snapshot)
        for value in (
            "secret",
            "key=token",
            "prompt-secret",
            "response-secret",
            "tool-args-secret",
            "api-key-secret",
            "private.example",
            "private-body",
            "https://",
            "/private/config.json",
        ):
            self.assertNotIn(value, serialized)

    def test_diagnostics_endpoint_requires_management_session_and_has_safe_schema(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            state.diagnostics.record({
                "route": "responses",
                "provider_id": "demo",
                "model_id": "demo/model",
                "endpoint_fingerprint": "sha256:" + "a" * 64,
                "protocol": "responses",
                "transport": "http",
                "request_bytes": 42,
                "response_bytes": 84,
                "duration_ms": 7,
                "status": 200,
                "error_class": "none",
                "decision": "normal_order",
                "raw_endpoint": "https://secret.example/v1?key=token",
                "authorization": "Bearer secret-token",
            })
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/diagnostics")
                denied = connection.getresponse()
                denied.read()
                connection.close()
                self.assertEqual(denied.status, 401)

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/diagnostics",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()
                self.assertEqual(response.status, 200)
                self.assertEqual(set(payload), {"capacity", "records"})
                self.assertEqual(set(payload["records"][0]), {
                    "observed_at",
                    "route",
                    "provider_id",
                    "model_id",
                    "endpoint_fingerprint",
                    "deployment_identity",
                    "protocol",
                    "transport",
                    "request_bytes",
                    "response_bytes",
                    "duration_ms",
                    "status",
                    "error_class",
                    "fallback",
                    "decision",
                    "context_decision",
                    "estimated_tokens",
                    "context_limit",
                    "safe_input_limit",
                    "context_confidence",
                    "context_source",
                    "context_estimate_method",
                    "context_reserves",
                    "context_completeness",
                })
                serialized = json.dumps(payload)
                self.assertNotIn("secret", serialized)
                self.assertNotIn("raw_endpoint", serialized)
            finally:
                server.shutdown()
                server.server_close()

    def test_stream_diagnostics_record_terminal_success_and_failure_without_buffering(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            success = iter([b'event: response.completed\ndata: {"type":"response.completed"}\n\n'])
            failure = iter([b'event: response.failed\ndata: {"type":"response.failed"}\n\n'])
            partial = iter([b'data: partial\n\n', b'data: never\n\n'])
            with patch(
                "easy_multi_provider.server.proxy",
                side_effect=[
                    ({
                        "kind": "stream",
                        "status": 200,
                        "content_type": "text/event-stream",
                        "provider_id": "demo",
                        "model_id": "demo/model",
                        "resolved_protocol": "responses",
                        "protocol_decision": "normal_order",
                    }, success),
                    ({
                        "kind": "stream",
                        "status": 200,
                        "content_type": "text/event-stream",
                        "provider_id": "demo",
                        "model_id": "demo/model",
                        "resolved_protocol": "responses",
                        "protocol_decision": "normal_order",
                    }, failure),
                    ({
                        "kind": "stream",
                        "status": 200,
                        "content_type": "text/event-stream",
                        "provider_id": "demo",
                        "model_id": "demo/model",
                        "resolved_protocol": "responses",
                        "protocol_decision": "normal_order",
                    }, partial),
                ],
            ):
                _, success_result = state.route(
                    {"model": "demo/model", "input": [], "stream": True}, {}
                )
                list(success_result)
                _, failure_result = state.route(
                    {"model": "demo/model", "input": [], "stream": True}, {}
                )
                list(failure_result)
                _, partial_result = state.route(
                    {"model": "demo/model", "input": [], "stream": True}, {}
                )
                next(partial_result)
                partial_result.close()
            records = state.diagnostics.snapshot()["records"]
            self.assertEqual(len(records), 3)
            self.assertEqual(records[0]["error_class"], "none")
            self.assertEqual(records[0]["status"], 200)
            self.assertEqual(records[1]["error_class"], "stream_error")
            self.assertEqual(records[1]["status"], 502)
            self.assertEqual(records[2]["error_class"], "client_disconnect")
            self.assertIsNone(records[2]["status"])

    def test_websocket_replay_state_has_fixed_item_and_byte_bounds(self):
        self.assertEqual(_ws_replay_size([{"type": "message"}])[0], 1)
        self.assertIsNone(_ws_replay_size([{}] * (_WS_REPLAY_MAX_ITEMS + 1)))
        self.assertIsNone(_ws_replay_size([{"text": "x" * (4 * 1024 * 1024)}]))
        self.assertIsNone(
            _ws_replay_size([{"text": "界" * (_WS_REPLAY_MAX_BYTES // 3 + 1)}])
        )

    def test_context_guard_uses_translated_payload_and_persists_only_numeric_calibration(self):
        class Response:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self):
                self.done = False

            def read(self, size=-1):
                if self.done:
                    return b""
                self.done = True
                return b'{"output":[]}'

            def close(self):
                pass

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({
                "providers": [{
                    "id": "demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "responses",
                    "auth_mode": "api_key",
                    "api_key": "secret-key",
                }],
                "models": [{
                    "id": "demo/model",
                    "provider": "demo",
                    "upstream_id": "model",
                    "context_window": 4096,
                    "capability_sources": {
                        "context_window": {
                            "source": "manual",
                            "confidence": 1,
                            "observed_at": "2026-08-21T00:00:00+00:00",
                        }
                    },
                }],
            }), config_path)
            state = AppState(config_path)
            with patch("easy_multi_provider.router.urlopen", return_value=Response()):
                _, result = state.route({
                    "model": "demo/model",
                    "input": "prompt-secret",
                    "tools": [{"type": "function", "name": "tool-secret"}],
                    "max_output_tokens": 128,
                }, {})
                self.assertEqual(result, b'{"output":[]}')
            records = state.diagnostics.snapshot()["records"]
            self.assertEqual(records[-1]["context_decision"], "allowed")
            self.assertGreater(records[-1]["estimated_tokens"], 0)
            self.assertEqual(records[-1]["context_source"], "manual")
            persisted = load(config_path)
            calibration = persisted["models"][0]["context_calibrations"][0]
            self.assertGreater(calibration["largest_success_estimate"], 0)
            serialized = json.dumps(persisted)
            self.assertNotIn("prompt-secret", serialized)
            self.assertNotIn("tool-secret", serialized)
            self.assertNotIn("secret-key", serialized)

    def test_context_guard_blocks_high_confidence_request_without_upstream_call(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({
                "providers": [{
                    "id": "demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "responses",
                    "auth_mode": "api_key",
                    "api_key": "secret-key",
                }],
                "models": [{
                    "id": "demo/model",
                    "provider": "demo",
                    "upstream_id": "model",
                    "context_window": 1000,
                    "capability_sources": {
                        "context_window": {
                            "source": "manual",
                            "confidence": 1,
                            "observed_at": "2026-08-21T00:00:00+00:00",
                        }
                    },
                }],
            }), config_path)
            state = AppState(config_path)
            with patch("easy_multi_provider.router.urlopen") as urlopen:
                with self.assertRaises(Exception) as raised:
                    state.route({
                        "model": "demo/model",
                        "input": "x" * 10000,
                        "max_output_tokens": 128,
                    }, {})
            urlopen.assert_not_called()
            self.assertEqual(raised.exception.status, 413)
            self.assertIn("context length exceeded", str(raised.exception))
            record = state.diagnostics.snapshot()["records"][-1]
            self.assertEqual(record["context_decision"], "blocked")
            self.assertEqual(record["error_class"], "context_length_exceeded")

    def test_explicit_context_failure_updates_smallest_boundary_without_retry(self):
        error = HTTPError(
            "https://example.com/v1/responses",
            400,
            "bad request",
            {"Content-Type": "application/json"},
            io.BytesIO(b'{"error":{"code":"context_length_exceeded","message":"secret"}}'),
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(normalize({
                "providers": [{
                    "id": "demo",
                    "base_url": "https://example.com/v1",
                    "protocol": "responses",
                    "auth_mode": "api_key",
                    "api_key": "secret-key",
                }],
                "models": [{
                    "id": "demo/model",
                    "provider": "demo",
                    "upstream_id": "model",
                    "context_window": 4096,
                    "capability_sources": {
                        "context_window": {
                            "source": "manual",
                            "confidence": 1,
                            "observed_at": "2026-08-21T00:00:00+00:00",
                        }
                    },
                }],
            }), config_path)
            state = AppState(config_path)
            with patch("easy_multi_provider.router.urlopen", side_effect=error) as urlopen:
                with self.assertRaises(Exception) as raised:
                    state.route({
                        "model": "demo/model",
                        "input": "prompt-secret",
                        "max_output_tokens": 128,
                    }, {})
            self.assertEqual(urlopen.call_count, 1)
            self.assertEqual(raised.exception.status, 413)
            self.assertNotIn("secret", str(raised.exception))
            calibration = load(config_path)["models"][0]["context_calibrations"][0]
            self.assertEqual(
                calibration["smallest_failure_estimate"],
                state.diagnostics.snapshot()["records"][-1]["estimated_tokens"],
            )
            self.assertEqual(
                state.diagnostics.snapshot()["records"][-1]["context_source"],
                "observed",
            )

    def test_enable_writes_stable_catalog_before_managed_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            save(_integration_test_config(root), config_path)
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="server-enable",
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=_WaitingRuntimeController(),
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {
                    "Cookie": "emp_session=" + state.session_token,
                    "Content-Type": "application/json",
                }
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/integration/enable",
                    json.dumps({"confirm_reload": True}),
                    headers,
                )
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()

                catalog_path = codex_home / "easy-multi-provider" / "catalog.json"
                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "emp_applied")
                self.assertEqual(
                    payload["runtime"]["state"], STOPPED_WAITING_FOR_START
                )
                self.assertEqual(payload["service_health"], "ready")
                self.assertTrue(catalog_path.exists())
                config_text = (codex_home / "config.toml").read_text(encoding="utf-8")
                self.assertIn(
                    'openai_base_url = "http://127.0.0.1:%d/v1"' % server.server_address[1],
                    config_text,
                )
                self.assertIn(
                    'model_catalog_json = "%s"' % str(catalog_path.resolve()),
                    config_text,
                )
                self.assertFalse((codex_home / "emp.config.toml").exists())
                self.assertFalse((root / "generated").exists())
            finally:
                server.shutdown()
                server.server_close()

    def test_enable_conflict_is_non_2xx_and_does_not_overwrite_user_change(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = root / "config.json"
            save(_integration_test_config(root), config_path)
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="server-conflict",
            )
            state = AppState(
                config_path,
                integration_manager=manager,
                runtime_controller=_WaitingRuntimeController(),
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            headers = {
                "Cookie": "emp_session=" + state.session_token,
                "Content-Type": "application/json",
            }
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/integration/enable",
                    json.dumps({"confirm_reload": True}),
                    headers,
                )
                first = connection.getresponse()
                first.read()
                connection.close()

                config_path = codex_home / "config.toml"
                config_path.write_text(
                    'openai_base_url = "user-owned-value"\n'
                    'model_catalog_json = "catalog-value"\n',
                    encoding="utf-8",
                )
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/integration/enable",
                    json.dumps({"confirm_reload": True}),
                    headers,
                )
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                response.read()
                connection.close()

                self.assertEqual(first.status, 200)
                self.assertGreaterEqual(response.status, 400)
                self.assertEqual(payload["configuration"]["state"], "conflict")
                self.assertIn(
                    "openai_base_url", payload["configuration"]["conflicts"]
                )
                self.assertIn("user-owned-value", config_path.read_text(encoding="utf-8"))
            finally:
                server.shutdown()
                server.server_close()

    def test_restore_endpoint_repairs_orphaned_lease_after_crash(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = codex_home / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                'openai_base_url = "native"\nmodel_catalog_json = "native-catalog"\n',
                encoding="utf-8",
            )
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            first = IntegrationManager(config_path, lease_path, instance_id="foreign")
            first.enable("http://127.0.0.1:43123/v1", "catalog", service_ready=True)
            emp_config_path = root / "config.json"
            save(_integration_test_config(root), emp_config_path)
            state = AppState(
                emp_config_path,
                integration_manager=IntegrationManager(
                    config_path,
                    lease_path,
                    instance_id="current",
                ),
                runtime_controller=_WaitingRuntimeController(),
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                headers = {
                    "Cookie": "emp_session=" + state.session_token,
                    "Content-Type": "application/json",
                }
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/integration/restore",
                    json.dumps({"confirm_reload": True}),
                    headers,
                )
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()

                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "native")
                self.assertEqual(payload["configuration"]["relation"], "original")
                self.assertEqual(
                    config_path.read_text(encoding="utf-8"),
                    'openai_base_url = "native"\nmodel_catalog_json = "native-catalog"\n',
                )
            finally:
                server.shutdown()
                server.server_close()

    def test_nonowned_shutdown_is_noop_and_does_not_modify_orphaned_lease(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = codex_home / "config.toml"
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            config_path.parent.mkdir(parents=True)
            config_path.write_text(
                'openai_base_url = "native"\nmodel_catalog_json = "native-catalog"\n',
                encoding="utf-8",
            )
            first = IntegrationManager(config_path, lease_path, instance_id="foreign")
            first.enable("http://127.0.0.1:43123/v1", "catalog", service_ready=True)
            before_config = config_path.read_bytes()
            before_lease = lease_path.read_bytes()
            state = AppState(
                root / "config.json",
                integration_manager=IntegrationManager(
                    config_path,
                    lease_path,
                    instance_id="current",
                ),
            )
            save(normalize({}), state.path)

            result = state.shutdown_restore()

            self.assertEqual(result.action, "noop")
            self.assertFalse(state._integration_owned)
            self.assertEqual(config_path.read_bytes(), before_config)
            self.assertEqual(lease_path.read_bytes(), before_lease)

    def test_startup_reconciles_after_bind_and_normal_shutdown_restores(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = codex_home / "config.toml"
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            second = IntegrationManager(config_path, lease_path, instance_id="new")
            state = AppState(root / "config.json", integration_manager=second)
            save(normalize({}), state.path)
            server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            try:
                base_url = "http://127.0.0.1:%d/v1" % server.server_address[1]
                catalog_path = codex_home / "easy-multi-provider" / "catalog.json"
                first = IntegrationManager(config_path, lease_path, instance_id="old")
                first.enable(base_url, str(catalog_path.resolve()), service_ready=True)
                result = startup_reconcile(state, server)
                self.assertEqual(result.action, "re_adopted")
                self.assertEqual(state.integration_status().state, "active")
                self.assertTrue(state.service_ready())
                self.assertTrue(state.shutdown_restore().ok)
                self.assertFalse(config_path.exists())
                self.assertEqual(second.status().state, "restored")
            finally:
                server.server_close()

    def test_startup_old_endpoint_or_catalog_is_conflict_without_mutation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = codex_home / "config.toml"
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            second = IntegrationManager(config_path, lease_path, instance_id="new")
            state = AppState(root / "config.json", integration_manager=second)
            save(normalize({}), state.path)
            server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            server_thread = None
            try:
                first = IntegrationManager(config_path, lease_path, instance_id="old")
                first.enable(
                    "http://127.0.0.1:43123/v1",
                    str((root / "old-catalog.json").resolve()),
                    service_ready=True,
                )
                before_config = config_path.read_bytes()
                before_lease = lease_path.read_bytes()

                result = startup_reconcile(state, server)

                self.assertEqual(result.state, "conflict")
                self.assertIn("listener_mismatch", result.conflicts)
                self.assertIn("catalog_mismatch", result.conflicts)
                self.assertFalse(state._integration_owned)
                self.assertEqual(config_path.read_bytes(), before_config)
                self.assertEqual(lease_path.read_bytes(), before_lease)
                self.assertEqual(state.integration_status().state, "active")

                server_thread = threading.Thread(target=server.serve_forever, daemon=True)
                server_thread.start()
                headers = {"Cookie": "emp_session=" + state.session_token}
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/integration", headers=headers)
                response = connection.getresponse()
                payload = json.loads(response.read().decode("utf-8"))
                connection.close()
                self.assertEqual(response.status, 200)
                self.assertEqual(payload["configuration"]["state"], "conflict")
                self.assertEqual(payload["service_health"], "ready")
                self.assertEqual(payload["next_action"], "restore")
                self.assertIn(
                    "listener_mismatch", payload["configuration"]["conflicts"]
                )
                self.assertIn(
                    "catalog_mismatch", payload["configuration"]["conflicts"]
                )
            finally:
                if server_thread is not None:
                    server.shutdown()
                server.server_close()

    def test_startup_original_relation_restores_even_when_target_is_stale(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            config_path = codex_home / "config.toml"
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            config_path.parent.mkdir(parents=True)
            config_path.write_text('title = "native"\n', encoding="utf-8")
            first = IntegrationManager(config_path, lease_path, instance_id="old")
            first.enable(
                "http://127.0.0.1:43123/v1",
                str((root / "old-catalog.json").resolve()),
                service_ready=True,
            )
            config_path.write_text('title = "native"\n', encoding="utf-8")
            state = AppState(
                root / "config.json",
                integration_manager=IntegrationManager(
                    config_path,
                    lease_path,
                    instance_id="current",
                ),
            )
            save(normalize({}), state.path)
            server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            try:
                result = startup_reconcile(state, server)
                self.assertEqual(result.action, "recovered_restored")
                self.assertEqual(result.state, "restored")
                self.assertEqual(state.integration_status().state, "restored")
                self.assertFalse(state._integration_owned)
                self.assertEqual(state.startup_conflicts(), ())
            finally:
                server.server_close()

    def test_startup_non_applied_relations_keep_manager_conflicts(self):
        cases = (
            (
                "mixed",
                lambda old_catalog: 'model_catalog_json = "%s"\n' % old_catalog,
                ("mixed_state",),
            ),
            (
                "other",
                lambda old_catalog: (
                    'openai_base_url = "user-owned"\n'
                    'model_catalog_json = "user-catalog"\n'
                ),
                ("openai_base_url", "model_catalog_json"),
            ),
        )
        for name, config_text, expected_conflicts in cases:
            with self.subTest(relation=name), tempfile.TemporaryDirectory() as directory:
                root = Path(directory)
                codex_home = root / "codex"
                config_path = codex_home / "config.toml"
                lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
                config_path.parent.mkdir(parents=True)
                config_path.write_text('title = "native"\n', encoding="utf-8")
                old_catalog = (root / "old-catalog.json").resolve()
                first = IntegrationManager(config_path, lease_path, instance_id="old")
                first.enable(
                    "http://127.0.0.1:43123/v1",
                    str(old_catalog),
                    service_ready=True,
                )
                config_path.write_text(config_text(str(old_catalog)), encoding="utf-8")
                before_config = config_path.read_bytes()
                before_lease = lease_path.read_bytes()
                state = AppState(
                    root / "config.json",
                    integration_manager=IntegrationManager(
                        config_path,
                        lease_path,
                        instance_id="current",
                    ),
                )
                save(normalize({}), state.path)
                server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
                try:
                    result = startup_reconcile(state, server)
                    self.assertEqual(result.state, "conflict")
                    self.assertEqual(result.relation, name)
                    self.assertEqual(result.conflicts, expected_conflicts)
                    self.assertNotIn("listener_mismatch", result.conflicts)
                    self.assertNotIn("catalog_mismatch", result.conflicts)
                    self.assertFalse(state._integration_owned)
                    self.assertEqual(config_path.read_bytes(), before_config)
                    self.assertEqual(lease_path.read_bytes(), before_lease)
                finally:
                    server.server_close()

    def test_fresh_startup_remains_native_without_lease_or_config(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            manager = IntegrationManager(
                codex_home / "config.toml",
                codex_home / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="fresh",
            )
            state = AppState(root / "config.json", integration_manager=manager)
            save(normalize({}), state.path)
            server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            try:
                result = startup_reconcile(state, server)
                self.assertEqual(result.state, "native")
                self.assertEqual(state.integration_status().state, "native")
                self.assertFalse((codex_home / "config.toml").exists())
                self.assertFalse(
                    (codex_home / "easy-multi-provider" / "catalog.json").exists()
                )
            finally:
                server.server_close()

    def test_startup_reconcile_rejects_unbound_listener_before_recovery(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            manager = IntegrationManager(
                root / "codex" / "config.toml",
                root / "codex" / "easy-multi-provider" / "integration" / "lease.json",
                instance_id="unbound",
            )
            state = AppState(root / "config.json", integration_manager=manager)
            save(normalize({}), state.path)

            class UnboundListener:
                def fileno(self):
                    return -1

            with self.assertRaises(ServiceNotReady):
                startup_reconcile(state, UnboundListener())
            self.assertFalse(state.service_ready())
            self.assertEqual(manager.status().state, "native")

    def test_sigterm_handler_is_main_thread_only_and_restores_previous_handler(self):
        previous_handler = object()
        with patch(
            "easy_multi_provider.server.signal.getsignal",
            return_value=previous_handler,
        ), patch("easy_multi_provider.server.signal.signal") as register:
            installed = _install_sigterm_handler()
            self.assertEqual(installed, (signal.SIGTERM, previous_handler))
            handler = register.call_args.args[1]
            with self.assertRaises(_GracefulShutdown):
                handler(signal.SIGTERM, None)
            _restore_sigterm_handler(installed)
            self.assertEqual(register.call_args.args, (signal.SIGTERM, previous_handler))

        with patch(
            "easy_multi_provider.server.threading.current_thread",
            return_value=object(),
        ), patch("easy_multi_provider.server.signal.signal") as register:
            self.assertIsNone(_install_sigterm_handler())
            register.assert_not_called()

    def test_sigterm_control_flow_restores_owned_lease_and_prints_bound_port(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            codex_home = root / "codex"
            emp_config = root / "emp.json"
            save(normalize({}), emp_config)
            config_path = codex_home / "config.toml"
            config_path.parent.mkdir(parents=True)
            config_path.write_text('title = "native"\n', encoding="utf-8")
            lease_path = codex_home / "easy-multi-provider" / "integration" / "lease.json"
            catalog_path = codex_home / "easy-multi-provider" / "catalog.json"
            first = IntegrationManager(config_path, lease_path, instance_id="old")
            first.enable(
                "http://127.0.0.1:45678/v1",
                str(catalog_path.resolve()),
                service_ready=True,
            )

            created_servers = []

            class FakeServer:
                def __init__(self, address, handler):
                    self.requested_address = address
                    self.handler = handler
                    self.server_address = ("127.0.0.1", 45678)
                    self.closed = False
                    created_servers.append(self)

                def fileno(self):
                    return 7

                def serve_forever(self):
                    raise _GracefulShutdown()

                def server_close(self):
                    self.closed = True

            output = io.StringIO()
            with patch.dict(
                os.environ,
                {
                    "CODEX_HOME": str(codex_home),
                    "EASY_MULTI_PROVIDER_CONFIG": str(emp_config),
                },
                clear=False,
            ), patch(
                "easy_multi_provider.server.BoundedThreadingHTTPServer",
                FakeServer,
            ), patch(
                "easy_multi_provider.server.configure_proxy_environment",
                return_value="direct",
            ), redirect_stdout(output):
                serve(port=0)

            self.assertEqual(created_servers[0].requested_address, ("127.0.0.1", 0))
            self.assertIn("EasyMultiProvider listening on http://127.0.0.1:45678", output.getvalue())
            self.assertNotIn("listening on http://127.0.0.1:0", output.getvalue())
            restored = config_path.read_text(encoding="utf-8")
            self.assertIn('title = "native"', restored)
            self.assertNotIn("openai_base_url", restored)
            self.assertNotIn("model_catalog_json", restored)
            self.assertEqual(first.status().state, "restored")

    def test_migration_endpoints_export_and_import_emp_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "secret_store_path": str(root / "state" / "secrets"),
                        "providers": [
                            {
                                "id": "demo",
                                "base_url": "https://example.com/v1",
                                "api_key": "provider-secret",
                            }
                        ],
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            headers = {
                "Cookie": "emp_session=" + state.session_token,
                "Content-Type": "application/json",
            }
            try:
                with patch(
                    "easy_multi_provider.server.generated_catalog_path",
                    return_value=root / "generated" / "catalog.json",
                ):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/migration/export",
                        json.dumps({"password": "migration-pass-3"}).encode(),
                        headers,
                    )
                    response = connection.getresponse()
                    bundle = response.read()
                    self.assertEqual(response.status, 200)
                    self.assertIn(
                        "easy-multi-provider-%s.emp" % __version__,
                        response.getheader("Content-Disposition"),
                    )
                    self.assertTrue(bundle.startswith(b"EMP-MIGRATION"))
                    connection.close()

                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/migration/import",
                        json.dumps(
                            {
                                "password": "migration-pass-3",
                                "bundle": base64.b64encode(bundle).decode("ascii"),
                            }
                        ).encode(),
                        headers,
                    )
                    response = connection.getresponse()
                    payload = json.loads(response.read().decode())
                    self.assertEqual(response.status, 200)
                    self.assertEqual(payload["providers"], 1)
                    connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_provider_discovery_adds_models_and_preserves_hidden_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize({
                    "native_catalog_path": str(root / "missing-native.json"),
                    "providers": [{"id": "demo", "base_url": "https://example.com/v1"}],
                    "models": [{
                        "id": "demo/hidden",
                        "provider": "demo",
                        "enabled": False,
                    }],
                }),
                config_path,
            )
            state = AppState(config_path)
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=[
                    {
                        "upstream_id": "hidden",
                        "reasoning_levels": ["low", "high"],
                        "context_window": 123,
                    },
                    {
                        "upstream_id": "new-model",
                        "display_name": "New model",
                        "reasoning_levels": ["medium"],
                        "context_window": 456,
                    },
                ],
            ), patch("easy_multi_provider.server.write_catalog", return_value=root / "catalog.json"):
                preview = state.discover_provider_models("demo")
                self.assertEqual(preview["available"], 2)
                result = state.discover_provider_models("demo", ["hidden", "new-model"])
            self.assertEqual(result["added"], 1)
            models = {item["id"]: item for item in state.config["models"]}
            self.assertFalse(models["demo/hidden"]["enabled"])
            self.assertEqual(models["demo/hidden"]["reasoning_levels"], ["low", "high"])
            self.assertEqual(
                models["demo/hidden"]["capability_sources"]["reasoning_levels"]["source"],
                "advertised",
            )
            self.assertEqual(
                models["demo/hidden"]["capability_sources"]["context_window"]["source"],
                "advertised",
            )
            self.assertTrue(models["demo/new-model"]["enabled"])
            self.assertEqual(
                models["demo/new-model"]["capability_sources"]["reasoning_levels"]["source"],
                "advertised",
            )

    def test_provider_discovery_deselect_hides_model_without_losing_overrides(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "providers": [
                            {
                                "id": "demo",
                                "base_url": "https://example.com/v1",
                                "api_key": "test-only-key",
                            }
                        ],
                        "models": [
                            {
                                "id": "demo/retained",
                                "provider": "demo",
                                "upstream_id": "retained",
                                "enabled": True,
                                "context_window": 98765,
                                "capability_sources": {
                                    "context_window": {"source": "manual", "confidence": 1}
                                },
                            }
                        ],
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=[
                    {
                        "upstream_id": "retained",
                        "context_window": 123,
                    },
                    {
                        "upstream_id": "new-model",
                        "context_window": 456,
                    },
                ],
            ), patch(
                "easy_multi_provider.server.write_catalog",
                return_value=root / "catalog.json",
            ):
                result = state.discover_provider_models("demo", ["new-model"])

            self.assertEqual(result["available"], 2)
            self.assertEqual(result["added"], 1)
            self.assertEqual(result["hidden"], 1)

            models = {item["id"]: item for item in state.snapshot()["models"]}
            self.assertFalse(models["demo/retained"]["enabled"])
            self.assertEqual(models["demo/retained"]["context_window"], 98765)
            self.assertEqual(
                models["demo/retained"]["capability_sources"]["context_window"]["source"],
                "manual",
            )
            self.assertTrue(models["demo/new-model"]["enabled"])
            self.assertEqual(api_key(state.snapshot()["providers"][0]), "test-only-key")

    def test_account_upload_needs_no_manual_web_token_and_never_returns_auth_json(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "account_store_path": str(root / "state" / "accounts"),
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                body = json.dumps(
                    {
                        "id": "primary",
                        "name": "Primary",
                        "prefix": "primary",
                        "auth_json": {
                            "auth_mode": "chatgpt",
                            "tokens": {"access_token": "account-secret"},
                        },
                    }
                ).encode("utf-8")
                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/api/accounts/import",
                    body,
                    {
                        "Content-Type": "application/json",
                        "Cookie": "emp_session=" + state.session_token,
                    },
                )
                response = connection.getresponse()
                payload = response.read().decode("utf-8")
                self.assertEqual(response.status, 200)
                self.assertNotIn("account-secret", payload)
                self.assertNotIn("auth_json", payload)
                connection.close()
                self.assertNotIn("account-secret", config_path.read_text(encoding="utf-8"))
                self.assertNotIn("refresh_token", config_path.read_text(encoding="utf-8"))
                self.assertTrue((root / "state" / "accounts" / "primary" / "auth.json.enc").exists())
                self.assertFalse((root / "state" / "accounts" / "primary" / "auth.json").exists())

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/accounts",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                listing = json.loads(response.read().decode("utf-8"))
                self.assertEqual(listing["accounts"][0]["prefix"], "primary")
                self.assertTrue(listing["accounts"][0]["credential_set"])
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "DELETE",
                    "/api/accounts/primary",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read().decode("utf-8"))["status"], "ok")
                connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_quota_refresh_persists_only_safe_snapshot(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "account_store_path": str(root / "state" / "accounts"),
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            state.import_account(
                {"id": "primary", "name": "Primary", "prefix": "primary"},
                {"auth_mode": "chatgpt", "tokens": {"access_token": "account-secret"}},
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                snapshot = {
                    "account_label": "s***@example.com",
                    "plan_type": "plus",
                    "rate_limits": {"primary": {"usedPercent": 10}},
                    "updated_at": 123,
                }
                with patch("easy_multi_provider.server.refresh_account_quota", return_value=snapshot):
                    connection = HTTPConnection(*server.server_address)
                    connection.request(
                        "POST",
                        "/api/accounts/primary/quota",
                        b"{}",
                        {
                            "Content-Type": "application/json",
                            "Cookie": "emp_session=" + state.session_token,
                        },
                    )
                    response = connection.getresponse()
                    payload = response.read().decode("utf-8")
                    self.assertEqual(response.status, 200)
                    self.assertIn('"usedPercent": 10', payload)
                    self.assertNotIn("account-secret", payload)
                    connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_management_and_proxy_require_automatic_session_or_caller_auth(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/api/config")
                self.assertEqual(connection.getresponse().status, 401)
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "GET",
                    "/api/config",
                    headers={
                        "Cookie": "emp_session=" + state.session_token,
                        "Host": "evil.example:%d" % server.server_address[1],
                        "Origin": "http://evil.example:%d" % server.server_address[1],
                    },
                )
                self.assertEqual(connection.getresponse().status, 403)
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request(
                    "POST",
                    "/v1/responses",
                    b'{"model":"demo/fixed","input":"hello"}',
                    {
                        "Content-Type": "application/json",
                        "Authorization": "Bearer attacker-controlled",
                    },
                )
                self.assertEqual(connection.getresponse().status, 401)
                connection.close()
            finally:
                server.shutdown()
                server.server_close()

    def test_web_root_requires_bootstrap_url_before_issuing_session(self):
        with tempfile.TemporaryDirectory() as directory:
            config_path = Path(directory) / "config.json"
            save(normalize({}), config_path)
            state = AppState(config_path)
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            try:
                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/")
                response = connection.getresponse()
                self.assertEqual(response.status, 401)
                self.assertIsNone(response.getheader("Set-Cookie"))
                response.read()
                connection.close()

                connection = HTTPConnection(*server.server_address)
                connection.request("GET", "/?bootstrap=" + state.bootstrap_token)
                response = connection.getresponse()
                self.assertEqual(response.status, 303)
                self.assertIn("emp_session=", response.getheader("Set-Cookie"))
                response.read()
                connection.close()
            finally:
                server.shutdown()
                server.server_close()


if __name__ == "__main__":
    unittest.main()
