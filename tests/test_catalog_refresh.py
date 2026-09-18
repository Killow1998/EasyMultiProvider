import copy
import http.client
import io
import json
import socket
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.catalog import build_catalog, catalog_etag
from easy_multi_provider.codex_runtime import CodexRuntimeController, EMP_LOADED, RELOAD_REQUIRED
from easy_multi_provider.config import normalize, save
from easy_multi_provider.router_errors import UpstreamHTTPError
from easy_multi_provider.server import (
    AppState,
    _native_websocket_metadata_events,
    make_handler,
)
from tests.test_server import _masked_text_frame, _read_text_frame


class CatalogPresentationTests(unittest.TestCase):
    def test_native_only_changes_require_matching_visibility_and_presentation(self):
        catalog = {"models": [{"slug": "gpt-test", "display_name": "Daily",
                               "description": "Coding", "visibility": "list"}]}
        current = [{"id": "gpt-test", "displayName": "Daily", "description": "Coding"}]
        verify = CodexRuntimeController._validate_models
        self.assertEqual(verify(current, (), "emp", catalog).state, EMP_LOADED)
        for field, value in (("displayName", "[128K] Daily"), ("description", "Coding · Context 128K")):
            stale = copy.deepcopy(current)
            stale[0][field] = value
            self.assertEqual(verify(stale, (), "emp", catalog).state, RELOAD_REQUIRED)
        stale = current + [{"id": "hidden-model", "displayName": "Hidden", "description": ""}]
        self.assertEqual(verify(stale, (), "emp", catalog).state, RELOAD_REQUIRED)


class CatalogRefreshTransportTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        environment = patch.dict("os.environ", {"CODEX_HOME": str(root / "codex")})
        environment.start()
        self.addCleanup(environment.stop)
        native = root / "native.json"
        native.write_text(json.dumps({"models": [{"slug": "gpt-test", "display_name": "Original"}]}), encoding="utf-8")
        config_path = root / "emp.json"
        save(normalize({"native_catalog_path": str(native)}), config_path)
        self.state = AppState(config_path)
        handler = make_handler(self.state)
        handler._proxy_allowed = lambda _: True
        self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)

    def stop_server(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)

    def request(self, path, body=None):
        connection = http.client.HTTPConnection(*self.server.server_address, timeout=3)
        try:
            connection.request("GET" if body is None else "POST", path,
                               None if body is None else json.dumps(body),
                               {"Content-Type": "application/json"})
            response = connection.getresponse()
            return response.status, dict(response.getheaders()), json.loads(response.read())
        finally:
            connection.close()

    def rename(self):
        self.state.config["catalog_presentations"] = {
            "gpt-test": {"catalog_alias": "Daily", "show_context": False}}

    def test_responses_and_discovery_share_revision_after_rename(self):
        _, headers, catalog = self.request("/v1/models?client_version=0.154.0")
        initial = headers["ETag"]
        self.assertEqual(initial, catalog_etag(catalog))
        self.rename()
        metadata = {"kind": "body", "content_type": "application/json"}
        with patch.object(self.state.codex, "route", return_value=(metadata, b'{}')):
            _, response_headers, _ = self.request("/v1/responses", {"model": "gpt-test", "input": "hello"})
        _, headers, _ = self.request("/v1/models?client_version=0.154.0")
        self.assertNotEqual(initial, headers["ETag"])
        self.assertEqual(response_headers["X-Models-Etag"], headers["ETag"])

    def test_native_response_headers_reach_http_client_but_emp_etag_wins(self):
        metadata = {
            "kind": "body",
            "content_type": "application/json",
            "response_headers": {
                "x-codex-turn-state": "sticky-state",
                "x-codex-safety-buffering-enabled": "true",
                "x-codex-safety-buffering-faster-model": "gpt-fast",
                "openai-model": "server-model",
                "x-reasoning-included": "true",
                "set-cookie": "session=secret",
                "x-models-etag": "upstream-etag",
            },
        }
        with patch.object(
            self.state.codex,
            "route",
            return_value=(metadata, b'{"status":"completed","output":[]}'),
        ):
            _, headers, body = self.request(
                "/v1/responses", {"model": "gpt-test", "input": "hello"}
            )
        headers = {key.lower(): value for key, value in headers.items()}
        self.assertEqual(body["status"], "completed")
        self.assertEqual(headers["x-codex-turn-state"], "sticky-state")
        self.assertEqual(headers["x-codex-safety-buffering-enabled"], "true")
        self.assertEqual(headers["x-codex-safety-buffering-faster-model"], "gpt-fast")
        self.assertEqual(headers["openai-model"], "server-model")
        self.assertEqual(headers["x-reasoning-included"], "true")
        self.assertEqual(headers["x-models-etag"], self.state.catalog_etag())
        self.assertNotIn("set-cookie", headers)

    def test_native_stream_response_headers_reach_http_client(self):
        frame = (
            b'event: response.completed\n'
            b'data: {"type":"response.completed","response":'
            b'{"status":"completed","output":[]}}\n\n'
        )
        metadata = {
            "kind": "stream",
            "content_type": "text/event-stream",
            "response_headers": {"x-codex-turn-state": "stream-state"},
        }
        connection = http.client.HTTPConnection(*self.server.server_address, timeout=3)
        try:
            with patch.object(
                self.state.codex,
                "route",
                return_value=(metadata, iter([frame])),
            ):
                connection.request(
                    "POST",
                    "/v1/responses",
                    json.dumps({"model": "gpt-test", "input": "hello", "stream": True}),
                    {"Content-Type": "application/json"},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(response.getheader("X-Codex-Turn-State"), "stream-state")
                self.assertEqual(response.getheader("X-Models-Etag"), self.state.catalog_etag())
                self.assertEqual(response.read(len(frame)), frame)
        finally:
            connection.close()

    def test_native_pre_output_http_error_keeps_status_and_safe_headers(self):
        failure = UpstreamHTTPError(
            "private upstream detail",
            401,
            "auth_rejected",
            "auth",
            response_headers={
                "x-request-id": "request-fixture",
                "Set-Cookie": "session=secret",
            },
        )

        def failed_stream():
            raise failure
            yield b""

        metadata = {
            "kind": "stream",
            "content_type": "text/event-stream",
        }
        connection = http.client.HTTPConnection(*self.server.server_address, timeout=3)
        try:
            with patch.object(
                self.state.codex,
                "route",
                return_value=(metadata, failed_stream()),
            ):
                connection.request(
                    "POST",
                    "/v1/responses",
                    json.dumps({"model": "gpt-test", "input": "hello", "stream": True}),
                    {"Content-Type": "application/json"},
                )
                response = connection.getresponse()
                self.assertEqual(response.status, 401)
                self.assertEqual(response.getheader("X-Request-Id"), "request-fixture")
                self.assertIsNone(response.getheader("Set-Cookie"))
                body = json.loads(response.read())
        finally:
            connection.close()

        self.assertEqual(body["error"]["code"], "auth_rejected")
        self.assertNotIn("private upstream detail", json.dumps(body))

    def test_websocket_http_fallback_announces_native_response_headers(self):
        handler = object.__new__(make_handler(self.state))
        frame = (
            b'event: response.completed\n'
            b'data: {"type":"response.completed","response":'
            b'{"status":"completed","output":[]}}\n\n'
        )
        events = list(
            handler._websocket_events(
                {
                    "kind": "stream",
                    "response_headers": {"x-codex-turn-state": "websocket-state"},
                },
                iter([frame]),
            )
        )
        self.assertEqual(events[0]["type"], "response.metadata")
        self.assertEqual(
            events[0]["headers"],
            {"x-codex-turn-state": "websocket-state"},
        )
        self.assertEqual(events[1]["type"], "response.completed")

    def test_websocket_metadata_uses_separate_catalog_event_kind(self):
        events = _native_websocket_metadata_events(
            {
                "x-codex-turn-state": "websocket-state",
                "x-models-etag": "upstream-etag",
            }
        )
        self.assertEqual(
            events,
            [
                {
                    "type": "response.metadata",
                    "headers": {"x-codex-turn-state": "websocket-state"},
                },
                {
                    "type": "codex.response.metadata",
                    "headers": {"x-models-etag": "upstream-etag"},
                },
            ],
        )

    def test_both_stream_paths_advertise_the_current_catalog_revision(self):
        frame = b'data: {"type":"response.completed","response":{"status":"completed","output":[]}}\n\n'
        for kind in ("stream", "raw_stream"):
            with self.subTest(kind=kind):
                result = iter([frame]) if kind == "stream" else io.BytesIO(frame)
                metadata = {"kind": kind, "content_type": "text/event-stream"}
                connection = http.client.HTTPConnection(*self.server.server_address, timeout=3)
                try:
                    with patch.object(self.state.codex, "route", return_value=(metadata, result)):
                        connection.request("POST", "/v1/responses", json.dumps({
                            "model": "gpt-test", "input": "hello", "stream": True,
                        }), {"Content-Type": "application/json"})
                        response = connection.getresponse()
                        self.assertEqual(response.status, 200)
                        self.assertEqual(response.getheader("X-Models-Etag"), self.state.catalog_etag())
                        self.assertEqual(response.read(len(frame)), frame)
                finally:
                    connection.close()

    def test_websocket_reused_connection_announces_changed_emp_catalog(self):
        events = [{"type": "codex.response.metadata", "headers": {"X-Models-Etag": "upstream", "x-request-id": "safe-id"}},
                  {"type": "response.completed", "response": {"status": "completed", "output": []}}]
        frames = [("data: " + json.dumps(event) + "\n\n").encode() for event in events]
        metadata = {"kind": "stream", "content_type": "text/event-stream"}
        client = socket.create_connection(self.server.server_address, timeout=3)
        reader = client.makefile("rb")
        try:
            client.sendall(("GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n"
                            "Connection: Upgrade\r\nUpgrade: websocket\r\nSec-WebSocket-Version: 13\r\n"
                            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\r\n" % self.server.server_port).encode())
            self.assertIn(b"101", reader.readline())
            while reader.readline() != b"\r\n":
                pass
            revisions = []
            for change in (False, True):
                if change:
                    self.rename()
                with patch.object(self.state.codex, "prepare_native_websocket", return_value=(None, None, None)), patch.object(
                    self.state.codex, "route", return_value=(metadata, iter(frames))
                ):
                    client.sendall(_masked_text_frame(json.dumps({"type": "response.create", "model": "gpt-test", "input": "hello"})))
                    received = [json.loads(_read_text_frame(reader, include_metadata=True)[1]) for _ in range(3)]
                revision = self.state.catalog_etag()
                revisions.append(revision)
                self.assertEqual(received[0]["headers"]["x-models-etag"], revision)
                self.assertEqual(received[1]["headers"], {"x-models-etag": revision, "x-request-id": "safe-id"})
                self.assertEqual(received[-1]["type"], "response.completed")
            self.assertNotEqual(*revisions)
        finally:
            reader.close()
            client.close()

    def test_http_failure_keeps_safe_classification_and_delay(self):
        failure = UpstreamHTTPError(
            "private upstream detail",
            429,
            "rate_limited",
            "rate_limit",
            12,
            response_headers={
                "x-codex-active-limit": "plus",
                "x-codex-primary-used-percent": "91",
                "x-request-id": "request-fixture",
                "Set-Cookie": "session=secret",
            },
        )
        with patch.object(self.state.codex, "route", side_effect=failure) as route:
            status, headers, body = self.request("/v1/responses", {"model": "gpt-test", "input": "hello"})
        self.assertEqual(route.call_count, 1)
        self.assertEqual((status, headers["Retry-After"]), (429, "12"))
        self.assertEqual(body["error"]["code"], "rate_limit_exceeded")
        self.assertEqual(body["error"]["failure_reason"], "rate_limited")
        self.assertEqual(body["error"]["retry_after_seconds"], 12)
        self.assertEqual(headers["x-codex-active-limit"], "plus")
        self.assertEqual(headers["x-codex-primary-used-percent"], "91")
        self.assertEqual(headers["x-request-id"], "request-fixture")
        self.assertNotIn("Set-Cookie", headers)
        self.assertNotIn("private upstream detail", json.dumps(body))
