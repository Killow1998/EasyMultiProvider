import json
import socket
import tempfile
import threading
import unittest
from pathlib import Path

from easy_multi_provider.codex_runtime import (
    CodexRuntimeController,
    EMP_LOADED,
    NATIVE_LOADED,
    RELOAD_REQUIRED,
    STOPPED_WAITING_FOR_START,
    VERIFICATION_FAILED,
    RuntimeSyncError,
)
from easy_multi_provider.config import normalize, save
from easy_multi_provider.integration import IntegrationManager
from easy_multi_provider.server import AppState, make_handler
from easy_multi_provider.transport import WebSocketConnection, websocket_accept


class _ForbiddenCommandRunner:
    @staticmethod
    def run(*_args, **_kwargs):
        raise AssertionError("runtime catalog checks must not invoke Codex commands")


class _ForbiddenHostStopper:
    @staticmethod
    def stop_stale_codex_hosts():
        raise AssertionError("runtime catalog checks must not stop Codex processes")


class _StaticModelCatalogProbe:
    def __init__(self, models):
        self.models = tuple(models)
        self.calls = []

    def model_list(self, codex_home, timeout):
        self.calls.append((Path(codex_home), timeout))
        return self.models


class _FailingModelCatalogProbe:
    def __init__(self, kind):
        self.kind = kind

    def model_list(self, _codex_home, _timeout):
        raise RuntimeSyncError("controlled catalog probe failure", self.kind)


class _UnixModelListServer:
    def __init__(self, codex_home: Path, pages):
        self.socket_path = (
            codex_home / "app-server-control" / "app-server-control.sock"
        )
        self.socket_path.parent.mkdir(parents=True)
        self.pages = pages
        self.requests = []
        self.error = None
        self._ready = threading.Event()
        self._thread = threading.Thread(target=self._serve, daemon=True)

    def __enter__(self):
        self._thread.start()
        if not self._ready.wait(2):
            raise AssertionError("test app-server did not start")
        return self

    def __exit__(self, _type, _value, _traceback):
        self._thread.join(2)
        if self._thread.is_alive():
            raise AssertionError("test app-server did not stop")
        if self.error is not None:
            raise self.error

    @staticmethod
    def _headers(reader):
        lines = []
        while True:
            line = reader.readline()
            if not line or line == b"\r\n":
                break
            lines.append(line.decode("ascii").rstrip("\r\n"))
        headers = {}
        for line in lines[1:]:
            name, value = line.split(":", 1)
            headers[name.casefold()] = value.strip()
        return lines[0], headers

    def _serve(self):
        listener = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        try:
            listener.bind(str(self.socket_path))
            listener.listen(1)
            self._ready.set()
            connection, _ = listener.accept()
            with connection:
                reader = connection.makefile("rb")
                writer = connection.makefile("wb")
                request_line, headers = self._headers(reader)
                if request_line != "GET / HTTP/1.1":
                    raise AssertionError(request_line)
                if headers.get("upgrade", "").casefold() != "websocket":
                    raise AssertionError("missing WebSocket Upgrade")
                if "sec-websocket-extensions" in headers:
                    raise AssertionError("catalog probe must disable WebSocket compression")
                key = headers["sec-websocket-key"]
                writer.write(
                    (
                        "HTTP/1.1 101 Switching Protocols\r\n"
                        "Upgrade: websocket\r\n"
                        "Connection: Upgrade\r\n"
                        f"Sec-WebSocket-Accept: {websocket_accept(key)}\r\n"
                        "\r\n"
                    ).encode("ascii")
                )
                writer.flush()
                websocket = WebSocketConnection(reader, writer)

                initialize = json.loads(websocket.receive_text())
                self.requests.append(initialize)
                websocket.send_json({"id": initialize["id"], "result": {}})
                initialized = json.loads(websocket.receive_text())
                self.requests.append(initialized)

                while True:
                    request = json.loads(websocket.receive_text())
                    self.requests.append(request)
                    cursor = request.get("params", {}).get("cursor")
                    page = self.pages["" if cursor is None else cursor]
                    websocket.send_json({"id": request["id"], "result": page})
                    if not page.get("nextCursor"):
                        break
                websocket.close()
                writer.close()
                reader.close()
        except BaseException as exc:  # surfaced by __exit__ in the test thread
            self.error = exc
            self._ready.set()
        finally:
            listener.close()


class SharedAppServerRuntimeTests(unittest.TestCase):
    def _controller(self, codex_home: Path):
        return CodexRuntimeController(
            runner=_ForbiddenCommandRunner(),
            host_stopper=_ForbiddenHostStopper(),
            target_codex_home=codex_home,
            control_timeout=1,
            observation_timeout=0,
        )

    @staticmethod
    def _state(root: Path, probe: _StaticModelCatalogProbe):
        config_path = root / "emp.json"
        save(
            normalize(
                {
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
            ),
            config_path,
        )
        codex_home = root / "codex"
        codex_home.mkdir()
        (codex_home / "config.toml").write_text(
            'openai_base_url = "https://native.invalid/v1"\n'
            'model_catalog_json = "/native/catalog.json"\n',
            encoding="utf-8",
        )
        manager = IntegrationManager(
            codex_home / "config.toml",
            codex_home / "easy-multi-provider" / "integration" / "lease.json",
            instance_id="shared-runtime-boundary",
        )
        controller = CodexRuntimeController(
            runner=_ForbiddenCommandRunner(),
            host_stopper=_ForbiddenHostStopper(),
            target_codex_home=codex_home,
            model_catalog_probe=probe,
            control_timeout=1,
            observation_timeout=0,
        )
        state = AppState(
            config_path,
            integration_manager=manager,
            runtime_controller=controller,
        )
        state.mark_service_ready()
        return state, codex_home

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "Unix sockets unavailable")
    def test_reload_uses_websocket_model_list_without_process_control(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory)
            pages = {
                "": {
                    "data": [{"id": "external/model-a"}],
                    "nextCursor": "page-2",
                },
                "page-2": {"data": [{"id": "external/model-b"}]},
            }
            with _UnixModelListServer(codex_home, pages) as server:
                result = self._controller(codex_home).reload(
                    ("external/model-a", "external/model-b"),
                    "emp",
                    confirm_reload=True,
                )

            self.assertEqual(result.state, EMP_LOADED)
            self.assertTrue(result.verified)
            self.assertEqual(
                [request.get("method") for request in server.requests],
                ["initialize", "initialized", "model/list", "model/list"],
            )
            self.assertNotIn("cursor", server.requests[2]["params"])
            self.assertEqual(server.requests[3]["params"]["cursor"], "page-2")

    def test_reload_reports_missing_shared_listener_without_starting_one(self):
        with tempfile.TemporaryDirectory() as directory:
            result = self._controller(Path(directory)).reload(
                ("external/model-a",), "emp", confirm_reload=True
            )

        self.assertEqual(result.state, STOPPED_WAITING_FOR_START)
        self.assertFalse(result.verified)
        self.assertIn("owner", result.detail.casefold())

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "Unix sockets unavailable")
    def test_reload_reports_owner_restart_when_saved_catalog_is_not_loaded(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory)
            with _UnixModelListServer(
                codex_home, {"": {"data": [{"id": "native/model"}]}}
            ):
                result = self._controller(codex_home).reload(
                    ("external/model-a",), "emp", confirm_reload=True
                )

        self.assertEqual(result.state, RELOAD_REQUIRED)
        self.assertFalse(result.verified)
        self.assertIn("owner", result.detail.casefold())
        self.assertIn("restart", result.detail.casefold())

    def test_enable_refresh_and_reload_never_control_the_shared_backend(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe = _StaticModelCatalogProbe(("native/model",))
            state, _codex_home = self._state(root, probe)

            enabled = state.enable_integration(
                "http://127.0.0.1:4201/v1", confirm_reload=True
            )
            self.assertTrue(enabled.ok)
            self.assertEqual(state.runtime_sync_snapshot()["state"], RELOAD_REQUIRED)

            checked = state.sync_integration_runtime(True)
            self.assertEqual(checked.state, RELOAD_REQUIRED)

            calls_before_refresh = len(probe.calls)
            state.refresh_catalog()
            self.assertEqual(len(probe.calls), calls_before_refresh)
            self.assertEqual(state.runtime_sync_snapshot()["state"], RELOAD_REQUIRED)

    def test_restore_writes_native_files_without_controlling_shared_backend(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe = _StaticModelCatalogProbe(("external/model-a",))
            state, codex_home = self._state(root, probe)
            state.enable_integration(
                "http://127.0.0.1:4201/v1", confirm_reload=True
            )
            self.assertEqual(state.runtime_sync_snapshot()["state"], EMP_LOADED)

            restored = state.restore_integration(confirm_reload=True)

            self.assertTrue(restored.ok)
            self.assertEqual(state.runtime_sync_snapshot()["state"], RELOAD_REQUIRED)
            native_config = (codex_home / "config.toml").read_text(encoding="utf-8")
            self.assertIn("https://native.invalid/v1", native_config)

    def test_permission_and_malformed_probe_failures_are_not_reported_as_waiting(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory)
            for kind in ("permission", "malformed"):
                with self.subTest(kind=kind):
                    controller = self._controller(codex_home)
                    controller.model_catalog_probe = _FailingModelCatalogProbe(kind)
                    observed = controller.observe(("external/model-a",), "emp")
                    checked = controller.reload(
                        ("external/model-a",), "emp", confirm_reload=True
                    )
                    self.assertEqual(observed.state, VERIFICATION_FAILED)
                    self.assertEqual(checked.state, VERIFICATION_FAILED)
                    self.assertNotEqual(observed.state, STOPPED_WAITING_FOR_START)
                    self.assertNotEqual(checked.state, STOPPED_WAITING_FOR_START)

    def test_native_target_is_verified_only_when_emp_model_ids_are_absent(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory)
            native = self._controller(codex_home)
            native.model_catalog_probe = _StaticModelCatalogProbe(("native/model",))
            loaded = native.reload(
                ("external/model-a",), "native", confirm_reload=True
            )
            self.assertEqual(loaded.state, NATIVE_LOADED)
            self.assertTrue(loaded.verified)

            stale = self._controller(codex_home)
            stale.model_catalog_probe = _StaticModelCatalogProbe(
                ("native/model", "external/model-a")
            )
            pending = stale.reload(
                ("external/model-a",), "native", confirm_reload=True
            )
            self.assertEqual(pending.state, RELOAD_REQUIRED)
            self.assertFalse(pending.verified)

    def test_read_only_observe_preserves_loaded_and_offline_states(self):
        with tempfile.TemporaryDirectory() as directory:
            codex_home = Path(directory)
            online = self._controller(codex_home)
            online.model_catalog_probe = _StaticModelCatalogProbe(
                ("external/model-a",)
            )
            loaded = online.observe(("external/model-a",), "emp")
            offline = self._controller(codex_home).observe(
                ("external/model-a",), "emp"
            )

        self.assertEqual(loaded.state, EMP_LOADED)
        self.assertTrue(loaded.verified)
        self.assertEqual(offline.state, STOPPED_WAITING_FOR_START)
        self.assertFalse(offline.verified)

    def test_http_enable_and_restore_require_confirmation_before_file_changes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            probe = _StaticModelCatalogProbe(("native/model",))
            state, codex_home = self._state(root, probe)
            state.codex_compatibility_snapshot = lambda: {
                "installed": None,
                "status": "unknown",
            }

            def request(path):
                handler = object.__new__(make_handler(state))
                handler.path = path
                handler.headers = {}
                handler.server = type(
                    "FakeServer", (), {"server_address": ("127.0.0.1", 4201)}
                )()
                handler._management_allowed = lambda: True
                handler._body = lambda _limit: {}
                captured = {}
                handler._send = lambda status, body, *args, **kwargs: captured.update(
                    status=status, body=body
                )
                handler.do_POST()
                return captured

            native_before = (codex_home / "config.toml").read_text(encoding="utf-8")
            enable = request("/api/integration/enable")
            self.assertEqual(enable["status"], 409)
            self.assertEqual(
                (codex_home / "config.toml").read_text(encoding="utf-8"),
                native_before,
            )

            state.enable_integration(
                "http://127.0.0.1:4201/v1", confirm_reload=True
            )
            applied_before = (codex_home / "config.toml").read_text(encoding="utf-8")
            restore = request("/api/integration/restore")
            self.assertEqual(restore["status"], 409)
            self.assertEqual(
                (codex_home / "config.toml").read_text(encoding="utf-8"),
                applied_before,
            )


if __name__ == "__main__":
    unittest.main()
