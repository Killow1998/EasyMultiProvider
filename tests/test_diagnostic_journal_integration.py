import base64
import contextlib
import hashlib
import io
import json
import socket
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from easy_multi_provider import __version__
from easy_multi_provider.config import ConfigError, normalize, save
from easy_multi_provider.diagnostic_journal import NullJournal, create_journal
from easy_multi_provider.server import (
    AppState,
    ObservationRing,
    _GracefulShutdown,
    _diagnostic_http_path,
    make_handler,
    serve,
)


def unsafe_route_event():
    return {
        "route": "responses",
        "provider_id": "demo",
        "model_id": "demo/model",
        "endpoint_fingerprint": "sha256:" + "a" * 64,
        "resolved_protocol": "responses",
        "dialect": "portable_responses",
        "transport": "http",
        "request_bytes": 42,
        "response_bytes": 84,
        "duration_ms": 7,
        "status": 200,
        "error_class": "none",
        "decision": "normal_order",
        "decoded_request_bytes": 100,
        "upstream_request_bytes": 25,
        "upstream_content_encoding": "zstd",
        "compression_ratio": 0.25,
        "prompt": "prompt-secret",
        "api_key": "api-key-secret",
        "path": "/private/config.json",
    }


class CapturingJournal:
    def __init__(self):
        self.events = []
        self.exceptions = []

    def event(self, level, event_name, **fields):
        self.events.append((level, event_name, fields))

    def exception_event(self, level, event_name, stage, exception):
        self.exceptions.append({
            "level": level,
            "event": event_name,
            "stage": stage,
            "exception_class": exception.__class__.__name__,
        })

    def pseudonym(self, value):
        return "account_" + hashlib.sha256(value.encode("utf-8")).hexdigest()[:16]


class FailingJournal:
    @property
    def enabled(self):
        raise OSError("journal enabled failure")

    @property
    def current_path(self):
        raise OSError("journal path failure")

    def event(self, level, event_name, **fields):
        raise OSError("journal event failure")

    def exception_event(self, level, event_name, stage, exception):
        raise OSError("journal exception failure")

    def pseudonym(self, value):
        raise OSError("journal pseudonym failure")

    def close(self):
        raise OSError("journal close failure")


def lifecycle_result(
    action="noop",
    state="native",
    relation="original",
    conflicts=(),
    ok=True,
):
    return SimpleNamespace(
        action=action,
        state=state,
        relation=relation,
        conflicts=conflicts,
        ok=ok,
    )


def fake_lifecycle_state(order=None, restore_result=None, restore_error=None):
    class FakeState:
        def __init__(
            self,
            path,
            integration_manager=None,
            catalog_path=None,
            journal=None,
        ):
            self.path = Path(path)
            self.journal = journal
            self.bootstrap_token = "BOOTSTRAP-LIFECYCLE-SECRET"
            self.session_token = "SESSION-LIFECYCLE-SECRET"
            self.config = {
                "host": "127.0.0.1",
                "port": 4200,
                "accounts": [{}, {}],
                "providers": [{}],
                "models": [{}, {}, {}],
            }

        def snapshot(self):
            return json.loads(json.dumps(self.config))

        def start_quota_sampler(self):
            return None

        def stop_quota_sampler(self):
            return None

        def shutdown_restore(self):
            if order is not None:
                order.append("shutdown_restore")
            if restore_error is not None:
                raise restore_error
            return restore_result or lifecycle_result(action="restored")

    return FakeState


def fake_lifecycle_server(order=None, serve_error=None):
    class FakeServer:
        def __init__(self, address, handler):
            self.requested_address = address
            self.handler = handler
            self.server_address = ("127.0.0.1", 45678)

        def fileno(self):
            return 7

        def serve_forever(self):
            if order is not None:
                order.append("serve_forever")
            raise serve_error if serve_error is not None else _GracefulShutdown()

        def server_close(self):
            if order is not None:
                order.append("server_close")

    return FakeServer


@contextlib.contextmanager
def fake_serve_dependencies(
    root,
    *,
    order=None,
    restore_result=None,
    restore_error=None,
    startup_result=None,
    serve_error=None,
):
    paths = SimpleNamespace(
        config_path=root / "codex" / "config.toml",
        lease_path=root / "codex" / "integration" / "lease.json",
        lock_path=root / "codex" / "integration" / "lock",
        codex_home=root / "codex",
    )
    with contextlib.ExitStack() as stack:
        stack.enter_context(
            patch("easy_multi_provider.server.ensure_master_key", return_value=None)
        )
        stack.enter_context(
            patch(
                "easy_multi_provider.server.configure_proxy_environment",
                return_value="direct",
            )
        )
        stack.enter_context(
            patch("easy_multi_provider.server.resolve_integration_paths", return_value=paths)
        )
        stack.enter_context(
            patch("easy_multi_provider.server.IntegrationManager", return_value=object())
        )
        stack.enter_context(
            patch(
                "easy_multi_provider.server.AppState",
                fake_lifecycle_state(order, restore_result, restore_error),
            )
        )
        stack.enter_context(
            patch(
                "easy_multi_provider.server.BoundedThreadingHTTPServer",
                fake_lifecycle_server(order, serve_error),
            )
        )
        stack.enter_context(
            patch(
                "easy_multi_provider.server.startup_reconcile",
                return_value=startup_result or lifecycle_result(action="checked"),
            )
        )
        stack.enter_context(
            patch("easy_multi_provider.server._install_sigterm_handler", return_value=None)
        )
        yield


@contextlib.contextmanager
def running_server(state):
    server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    try:
        yield server
    finally:
        server.shutdown()
        server.server_close()
        thread.join(timeout=3)


def request(server, method, path, body=None, headers=None):
    connection = HTTPConnection(*server.server_address, timeout=3)
    connection.request(method, path, body=body, headers=headers or {})
    response = connection.getresponse()
    payload = response.read()
    status = response.status
    connection.close()
    return status, payload


def http_events(journal):
    return [
        fields
        for _, event_name, fields in journal.events
        if event_name == "http_request"
    ]


def management_events(journal, event_name):
    return [
        fields
        for _, recorded_name, fields in journal.events
        if recorded_name == event_name
    ]


def read_journal_records(root):
    records = []
    for path in sorted((root / "state" / "logs").glob("emp-*.jsonl")):
        records.extend(
            json.loads(line)
            for line in path.read_text(encoding="utf-8").splitlines()
            if line
        )
    return records


class DiagnosticJournalIntegrationTest(unittest.TestCase):
    def make_state(self, root, **kwargs):
        config_file = root / "config.json"
        save(normalize({}), config_file)
        return AppState(
            config_file,
            catalog_path=root / "catalog.json",
            runtime_controller=object(),
            **kwargs,
        )

    def test_app_state_defaults_to_null_journal_without_creating_logs(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = self.make_state(root)

            self.assertIsInstance(state.journal, NullJournal)
            self.assertFalse(state.journal.enabled)
            self.assertIsInstance(state.diagnostics, ObservationRing)
            self.assertFalse((root / "state" / "logs").exists())

    def test_app_state_persists_only_the_normalized_route_observation(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            source = unsafe_route_event()

            state.diagnostics.record(source)

            snapshot_record = state.diagnostics.snapshot()["records"][0]
            self.assertEqual(len(journal.events), 1)
            level, event_name, persisted = journal.events[0]
            self.assertEqual(level, "info")
            self.assertEqual(event_name, "route_observation")
            self.assertEqual(set(persisted), set(snapshot_record))
            self.assertEqual(persisted, snapshot_record)
            for unsafe_key in ("prompt", "api_key", "path"):
                self.assertNotIn(unsafe_key, persisted)
            for unsafe_value in (
                "prompt-secret",
                "api-key-secret",
                "/private/config.json",
            ):
                self.assertNotIn(unsafe_value, repr(persisted))
            self.assertEqual(persisted["decoded_request_bytes"], 100)
            self.assertEqual(persisted["upstream_request_bytes"], 25)
            self.assertEqual(persisted["upstream_content_encoding"], "zstd")
            self.assertEqual(persisted["compression_ratio"], 0.25)

    def test_transport_metadata_sanitizer_rejects_nonfinite_unbounded_and_content(self):
        ring = ObservationRing()
        ring.record(
            {
                **unsafe_route_event(),
                "decoded_request_bytes": "body-secret",
                "upstream_request_bytes": -1,
                "upstream_content_encoding": "header-secret",
                "compression_ratio": float("inf"),
                "opaque": "opaque-secret",
            }
        )
        record = ring.snapshot()["records"][0]
        self.assertIsNone(record["decoded_request_bytes"])
        self.assertEqual(record["upstream_request_bytes"], 0)
        self.assertEqual(record["upstream_content_encoding"], "unknown")
        self.assertNotIn("compression_ratio", record)
        self.assertNotIn("secret", repr(record))

    def test_observation_sink_receives_a_copy_of_the_normalized_record(self):
        received = []
        ring = ObservationRing(sink=received.append)

        ring.record(unsafe_route_event())

        snapshot_record = ring.snapshot()["records"][0]
        self.assertEqual(received, [snapshot_record])
        self.assertIsNot(received[0], snapshot_record)
        received[0]["route"] = "mutated"
        self.assertEqual(ring.snapshot()["records"][0]["route"], "responses")

    def test_sink_exception_does_not_prevent_ring_storage(self):
        def failing_sink(record):
            raise RuntimeError("sink failure")

        ring = ObservationRing(sink=failing_sink)
        ring.record(unsafe_route_event())

        records = ring.snapshot()["records"]
        self.assertEqual(len(records), 1)
        self.assertEqual(records[0]["route"], "responses")

    def test_explicit_diagnostics_preserves_identity_and_is_not_rewired(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            diagnostics = ObservationRing()
            state = self.make_state(
                root,
                diagnostics=diagnostics,
                journal=journal,
            )

            self.assertIs(state.journal, journal)
            self.assertIs(state.diagnostics, diagnostics)
            diagnostics.record(unsafe_route_event())
            self.assertEqual(journal.events, [])

    def test_health_request_is_logged_once_without_query(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            with running_server(state) as server:
                status, _ = request(
                    server,
                    "GET",
                    "/healthz?bootstrap=TOPSECRET",
                )

            self.assertEqual(status, 200)
            events = http_events(journal)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["method"], "GET")
            self.assertEqual(events[0]["path"], "/healthz")
            self.assertEqual(events[0]["status"], 200)
            self.assertEqual(events[0]["request_bytes"], 0)
            self.assertNotIn("TOPSECRET", repr(journal.events))

    def test_http_path_normalizer_only_replaces_account_route_segments(self):
        self.assertEqual(
            _diagnostic_http_path(
                "/api/accounts/acct%40private.example?token=QUERYSECRET"
            ),
            "/api/accounts/{account}",
        )
        self.assertEqual(
            _diagnostic_http_path("/api/accounts/acct%2Fprivate/quota"),
            "/api/accounts/{account}/quota",
        )
        for path in (
            "/api/accounts",
            "/api/accounts/",
            "/api/accounts/import",
            "/api/accounts/account/extra",
            "/api/accounts/account/quota/extra",
        ):
            self.assertEqual(_diagnostic_http_path(path), path)

    def test_post_logs_declared_bytes_without_body_or_headers(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            body = json.dumps({
                "prompt": "BODYSECRET",
                "api_key": "BODYKEYSECRET",
            }).encode("utf-8")
            headers = {
                "Content-Type": "application/json",
                "Content-Length": str(len(body)),
                "Cookie": "emp_session=COOKIESECRET",
                "X-API-Key": "HEADERKEYSECRET",
            }
            with running_server(state) as server:
                status, _ = request(server, "POST", "/not-found", body, headers)

            self.assertEqual(status, 404)
            events = http_events(journal)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["method"], "POST")
            self.assertEqual(events[0]["path"], "/not-found")
            self.assertEqual(events[0]["status"], 404)
            self.assertEqual(events[0]["request_bytes"], len(body))
            persisted = repr(journal.events)
            for secret in (
                "BODYSECRET",
                "BODYKEYSECRET",
                "COOKIESECRET",
                "HEADERKEYSECRET",
            ):
                self.assertNotIn(secret, persisted)

    def test_get_404_and_delete_status_are_recorded(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            with running_server(state) as server:
                get_status, _ = request(server, "GET", "/missing")
                delete_status, _ = request(server, "DELETE", "/missing")

            self.assertEqual(get_status, 404)
            self.assertEqual(delete_status, 404)
            events = http_events(journal)
            self.assertEqual(len(events), 2)
            self.assertEqual(
                [(event["method"], event["status"]) for event in events],
                [("GET", 404), ("DELETE", 404)],
            )

    def test_journal_failures_do_not_change_http_responses(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = self.make_state(root, journal=FailingJournal())
            with running_server(state) as server:
                health_status, _ = request(server, "GET", "/healthz")

                def fail_route(*args, **kwargs):
                    raise RuntimeError("route failure secret")

                state.codex.route = fail_route
                with patch(
                    "easy_multi_provider.server.valid_caller_authorization",
                    return_value=True,
                ):
                    error_status, _ = request(
                        server,
                        "POST",
                        "/v1/responses",
                        b"{}",
                        {
                            "Content-Type": "application/json",
                            "Authorization": "Bearer caller-secret",
                        },
                    )

            self.assertEqual(health_status, 200)
            self.assertEqual(error_status, 500)

    def test_broad_handler_exceptions_record_class_once_without_message(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)

            def fail_route(*args, **kwargs):
                raise RuntimeError("POST-EXCEPTION-SECRET")

            def fail_delete(*args, **kwargs):
                raise OSError("DELETE-EXCEPTION-SECRET")

            state.codex.route = fail_route
            state.delete_account = fail_delete
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.valid_caller_authorization",
                    return_value=True,
                ):
                    post_status, _ = request(
                        server,
                        "POST",
                        "/v1/responses?token=POSTQUERYSECRET",
                        b"{}",
                        {
                            "Content-Type": "application/json",
                            "Authorization": "Bearer caller-secret",
                        },
                    )
                delete_status, _ = request(
                    server,
                    "DELETE",
                    "/api/accounts/demo?token=DELETEQUERYSECRET",
                    headers={"Cookie": "emp_session=" + state.session_token},
                )

            self.assertEqual(post_status, 500)
            self.assertEqual(delete_status, 500)
            self.assertEqual(len(journal.exceptions), 2)
            self.assertEqual(
                [item["exception_class"] for item in journal.exceptions],
                ["RuntimeError", "OSError"],
            )
            self.assertEqual(
                {item["stage"] for item in journal.exceptions},
                {"http_handler"},
            )
            persisted = repr(journal.exceptions + http_events(journal))
            for secret in (
                "POST-EXCEPTION-SECRET",
                "DELETE-EXCEPTION-SECRET",
                "POSTQUERYSECRET",
                "DELETEQUERYSECRET",
            ):
                self.assertNotIn(secret, persisted)

    def test_sse_direct_response_records_status_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            chunk = b'data: {"type": "response.failed"}\n\n'
            state.codex.route = lambda *args, **kwargs: (
                {"kind": "stream", "content_type": "text/event-stream"},
                iter([chunk]),
            )
            body = b'{"stream":true}'
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.valid_caller_authorization",
                    return_value=True,
                ):
                    status, response_body = request(
                        server,
                        "POST",
                        "/v1/responses?bootstrap=STREAMQUERYSECRET",
                        body,
                        {
                            "Content-Type": "application/json",
                            "Authorization": "Bearer stream-header-secret",
                        },
                    )

            self.assertEqual(status, 200)
            self.assertEqual(response_body, chunk)
            events = http_events(journal)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["path"], "/v1/responses")
            self.assertEqual(events[0]["status"], 200)
            self.assertEqual(events[0]["request_bytes"], len(body))
            self.assertNotIn("STREAMQUERYSECRET", repr(journal.events))
            self.assertNotIn("stream-header-secret", repr(journal.events))

    def test_websocket_upgrade_records_101_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.valid_caller_authorization",
                    return_value=True,
                ):
                    client = socket.create_connection(server.server_address, timeout=3)
                    client.settimeout(3)
                    host = "%s:%d" % server.server_address
                    client.sendall(
                        (
                            "GET /v1/responses?bootstrap=WSQUERYSECRET HTTP/1.1\r\n"
                            "Host: %s\r\n"
                            "Upgrade: websocket\r\n"
                            "Connection: Upgrade\r\n"
                            "Sec-WebSocket-Version: 13\r\n"
                            "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                            "Authorization: Bearer ws-header-secret\r\n"
                            "\r\n" % host
                        ).encode("ascii")
                    )
                    response = b""
                    while b"\r\n\r\n" not in response:
                        response += client.recv(4096)
                    self.assertTrue(response.startswith(b"HTTP/1.1 101"))
                    mask = b"\x01\x02\x03\x04"
                    close_payload = b"\x03\xe8"
                    masked = bytes(
                        byte ^ mask[index % 4]
                        for index, byte in enumerate(close_payload)
                    )
                    client.sendall(b"\x88\x82" + mask + masked)
                    client.close()
                    for _ in range(100):
                        if http_events(journal):
                            break
                        time.sleep(0.01)

            events = http_events(journal)
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["method"], "GET")
            self.assertEqual(events[0]["path"], "/v1/responses")
            self.assertEqual(events[0]["status"], 101)
            self.assertNotIn("WSQUERYSECRET", repr(journal.events))
            self.assertNotIn("ws-header-secret", repr(journal.events))

    def test_provider_discovery_records_safe_counts_and_failure_class(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            state.discover_provider_models = lambda provider, selected: {
                "available": 5,
                "added": 2,
                "hidden": 1,
                "model_count": 7,
            }
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            with running_server(state) as server:
                status, _ = request(
                    server,
                    "POST",
                    "/api/providers/discover",
                    json.dumps({
                        "provider": "../../unsafe?provider=SECRET",
                        "selected": ["one", "two"],
                    }),
                    headers,
                )

                def fail_discovery(provider, selected):
                    raise ConfigError("provider failure secret")

                state.discover_provider_models = fail_discovery
                failure_status, _ = request(
                    server,
                    "POST",
                    "/api/providers/discover",
                    json.dumps({"provider": "safe-provider"}),
                    headers,
                )

            self.assertEqual(status, 200)
            self.assertEqual(failure_status, 400)
            events = management_events(journal, "provider_discovery")
            self.assertEqual(len(events), 2)
            self.assertEqual(events[0]["provider_id"], "")
            self.assertEqual(events[0]["available"], 5)
            self.assertEqual(events[0]["selected"], 2)
            self.assertEqual(events[0]["added"], 2)
            self.assertEqual(events[0]["hidden"], 1)
            self.assertEqual(events[0]["model_count"], 7)
            self.assertEqual(events[0]["result_class"], "success")
            self.assertEqual(events[1]["result_class"], "config_error")
            self.assertNotIn("provider failure secret", repr(events))
            self.assertNotIn("SECRET", repr(events))

    def test_catalog_refresh_records_visible_count_without_path(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            state.refresh_catalog = lambda: Path("/absolute/SECRET/catalog.json")
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.build_catalog",
                    return_value={"models": [{}, {}, {}]},
                ):
                    status, _ = request(
                        server,
                        "POST",
                        "/api/catalog/refresh",
                        b"{}",
                        headers,
                    )

            self.assertEqual(status, 200)
            events = management_events(journal, "catalog_refresh")
            self.assertEqual(len(events), 1)
            self.assertEqual(events[0]["visible_model_count"], 3)
            self.assertEqual(events[0]["result_class"], "success")
            self.assertNotIn("/absolute/SECRET", repr(events))

    def test_account_operations_persist_only_pseudonyms(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            import_id = "acct-import-secret"
            quota_id = "acct-quota-secret"
            delete_id = "acct-delete-secret"

            def account(account_id, prefix):
                return {
                    "id": account_id,
                    "name": "private-email@example.com",
                    "prefix": prefix,
                    "auth_file": "",
                    "enabled": True,
                    "hidden_models": [],
                    "quota": None,
                }

            state.import_account = lambda metadata, auth: account(import_id, "imp")
            state.refresh_account = lambda account_id: account(account_id, "quota")
            deleted = []
            state.delete_account = deleted.append
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.public_accounts",
                    side_effect=lambda accounts: [{"status": "ok"}],
                ):
                    import_status, _ = request(
                        server,
                        "POST",
                        "/api/accounts/import",
                        json.dumps({
                            "id": import_id,
                            "name": "private-email@example.com",
                            "prefix": "imp",
                            "auth_json": {"access_token": "AUTH-TOKEN-SECRET"},
                        }),
                        headers,
                    )
                    quota_status, _ = request(
                        server,
                        "POST",
                        "/api/accounts/%s/quota" % quota_id,
                        b"{}",
                        headers,
                    )
                delete_status, _ = request(
                    server,
                    "DELETE",
                    "/api/accounts/%s" % delete_id,
                    headers={"Cookie": "emp_session=" + state.session_token},
                )

            self.assertEqual((import_status, quota_status, delete_status), (200, 200, 200))
            self.assertEqual(deleted, [delete_id])
            events = management_events(journal, "account_operation")
            self.assertEqual(
                [event["operation"] for event in events],
                ["import", "quota_refresh", "delete"],
            )
            self.assertEqual(
                [event["account_ref"] for event in events],
                [
                    journal.pseudonym(import_id),
                    journal.pseudonym(quota_id),
                    journal.pseudonym(delete_id),
                ],
            )
            self.assertEqual(
                [event["path"] for event in http_events(journal)],
                [
                    "/api/accounts/import",
                    "/api/accounts/{account}/quota",
                    "/api/accounts/{account}",
                ],
            )
            persisted = repr(journal.events)
            for secret in (
                import_id,
                quota_id,
                delete_id,
                "private-email@example.com",
                "AUTH-TOKEN-SECRET",
            ):
                self.assertNotIn(secret, persisted)

    def test_real_journal_mixed_concurrency_is_valid_and_contiguous(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = create_journal(root)
            self.assertTrue(journal.enabled)
            try:
                state = self.make_state(root, journal=journal)

                def write_route(index):
                    event = unsafe_route_event()
                    event["request_bytes"] = index
                    event["prompt"] = "MIXED-ROUTE-SECRET"
                    state.diagnostics.record(event)

                def write_management(index):
                    journal.event(
                        "info",
                        "catalog_refresh",
                        visible_model_count=index,
                        duration_ms=1,
                        result_class="success",
                    )

                def request_health(server, index):
                    status, _ = request(
                        server,
                        "GET",
                        "/healthz?bootstrap=MIXED-HTTP-SECRET-%d" % index,
                    )
                    if status != 200:
                        raise AssertionError("unexpected health status: %d" % status)

                with running_server(state) as server:
                    with ThreadPoolExecutor(max_workers=8) as executor:
                        futures = []
                        for index in range(16):
                            futures.append(executor.submit(write_route, index))
                            if index < 8:
                                futures.append(executor.submit(write_management, index))
                                futures.append(
                                    executor.submit(request_health, server, index)
                                )
                        for future in futures:
                            future.result(timeout=5)
            finally:
                journal.close()

            records = read_journal_records(root)
            sequences = [record["sequence"] for record in records]
            self.assertEqual(sequences, list(range(1, len(records) + 1)))
            self.assertEqual(len(sequences), len(set(sequences)))
            self.assertTrue(
                {"route_observation", "http_request", "catalog_refresh"}.issubset(
                    {record["event"] for record in records}
                )
            )
            persisted = "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((root / "state" / "logs").glob("*.jsonl"))
            )
            self.assertNotIn("MIXED-ROUTE-SECRET", persisted)
            self.assertNotIn("MIXED-HTTP-SECRET", persisted)

    def test_migration_operations_keep_counts_without_password_or_bundle(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            state.export_migration = lambda password: b"EXPORT-BUNDLE-SECRET"
            state.snapshot = lambda: {
                "accounts": [{}, {}],
                "providers": [{}, {}, {}],
                "models": [{}, {}, {}, {}],
            }
            state.import_migration = lambda bundle, password: {
                "accounts": 5,
                "providers": 6,
                "models": 7,
                "restored": True,
                "catalog_path": "/absolute/IMPORT-PATH-SECRET/catalog.json",
            }
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            import_bundle = b"IMPORT-BUNDLE-SECRET"
            with running_server(state) as server:
                export_status, export_body = request(
                    server,
                    "POST",
                    "/api/migration/export",
                    json.dumps({"password": "EXPORT-PASSWORD-SECRET"}),
                    headers,
                )
                import_status, _ = request(
                    server,
                    "POST",
                    "/api/migration/import",
                    json.dumps({
                        "password": "IMPORT-PASSWORD-SECRET",
                        "bundle": base64.b64encode(import_bundle).decode("ascii"),
                    }),
                    headers,
                )

            self.assertEqual((export_status, import_status), (200, 200))
            self.assertEqual(export_body, b"EXPORT-BUNDLE-SECRET")
            events = management_events(journal, "migration_operation")
            self.assertEqual(len(events), 2)
            self.assertEqual(
                {key: events[0][key] for key in ("accounts", "providers", "models")},
                {"accounts": 2, "providers": 3, "models": 4},
            )
            self.assertEqual(
                {key: events[1][key] for key in ("accounts", "providers", "models")},
                {"accounts": 5, "providers": 6, "models": 7},
            )
            self.assertTrue(events[1]["restored"])
            persisted = repr(events)
            for secret in (
                "EXPORT-PASSWORD-SECRET",
                "IMPORT-PASSWORD-SECRET",
                "EXPORT-BUNDLE-SECRET",
                "IMPORT-BUNDLE-SECRET",
                "IMPORT-PATH-SECRET",
            ):
                self.assertNotIn(secret, persisted)

    def test_integration_operation_records_only_bounded_codes(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = CapturingJournal()
            state = self.make_state(root, journal=journal)
            result = SimpleNamespace(ok=True, state="active")
            state.enable_integration = lambda *args, **kwargs: result
            summary = {
                "configuration": {
                    "state": "emp_applied",
                    "relation": "applied",
                    "conflicts": ["catalog_mismatch"],
                    "config_path": "/absolute/CONFIG-PATH-SECRET",
                },
                "runtime": {"detail": "RUNTIME-DETAIL-SECRET"},
            }
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.integration_summary",
                    return_value=summary,
                ):
                    confirmation_status, _ = request(
                        server,
                        "POST",
                        "/api/integration/enable",
                        b"{}",
                        headers,
                    )
                    success_status, _ = request(
                        server,
                        "POST",
                        "/api/integration/enable",
                        b'{"confirm_reload":true}',
                        headers,
                    )

                    def fail_enable(*args, **kwargs):
                        raise OSError("INTEGRATION-EXCEPTION-SECRET")

                    state.enable_integration = fail_enable
                    failure_status, _ = request(
                        server,
                        "POST",
                        "/api/integration/enable",
                        b'{"confirm_reload":true}',
                        headers,
                    )

            self.assertEqual(
                (confirmation_status, success_status, failure_status),
                (409, 200, 409),
            )
            events = management_events(journal, "integration_operation")
            self.assertEqual(
                [event["result_class"] for event in events],
                ["confirmation_required", "success", "integration_error"],
            )
            self.assertEqual(events[0]["operation"], "enable")
            self.assertEqual(events[0]["state"], "emp_applied")
            self.assertEqual(events[0]["relation"], "applied")
            self.assertEqual(events[0]["conflicts"], ["catalog_mismatch"])
            persisted = repr(events)
            for secret in (
                "CONFIG-PATH-SECRET",
                "RUNTIME-DETAIL-SECRET",
                "INTEGRATION-EXCEPTION-SECRET",
            ):
                self.assertNotIn(secret, persisted)

    def test_failing_journal_and_pseudonym_do_not_change_account_response(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            state = self.make_state(root, journal=FailingJournal())
            state.import_account = lambda metadata, auth: {
                "id": "failure-account-secret",
                "name": "private@example.com",
                "prefix": "failure",
                "auth_file": "",
                "enabled": True,
                "hidden_models": [],
                "quota": None,
            }
            headers = {
                "Content-Type": "application/json",
                "Cookie": "emp_session=" + state.session_token,
            }
            with running_server(state) as server:
                with patch(
                    "easy_multi_provider.server.public_accounts",
                    return_value=[{"status": "ok"}],
                ):
                    status, _ = request(
                        server,
                        "POST",
                        "/api/accounts/import",
                        json.dumps({
                            "id": "failure-account-secret",
                            "prefix": "failure",
                            "auth_json": {"access_token": "secret"},
                        }),
                        headers,
                    )

            self.assertEqual(status, 200)

    def test_serve_writes_real_lifecycle_journal_without_startup_secrets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_file = root / "private-config-name.json"
            save(normalize({}), config_file)
            output = io.StringIO()
            with fake_serve_dependencies(root), contextlib.redirect_stdout(output):
                serve(config_file, port=0)

            records = read_journal_records(root)
            events = [record["event"] for record in records]
            self.assertEqual(
                events,
                [
                    "process_start",
                    "proxy_selected",
                    "startup_reconcile",
                    "service_listening",
                    "shutdown_start",
                    "shutdown_complete",
                ],
            )
            process_fields = records[0]["fields"]
            self.assertEqual(process_fields["emp_version"], __version__)
            self.assertEqual(process_fields["account_count"], 2)
            self.assertEqual(process_fields["provider_count"], 1)
            self.assertEqual(process_fields["model_count"], 3)
            self.assertEqual(records[1]["fields"]["source"], "direct")
            self.assertEqual(records[2]["fields"]["action"], "checked")
            self.assertEqual(records[2]["fields"]["result_class"], "success")
            self.assertEqual(records[3]["fields"]["port"], 45678)
            self.assertEqual(records[-1]["fields"]["reason"], "sigterm")
            self.assertIn("Diagnostic log: ", output.getvalue())

            persisted = "\n".join(
                path.read_text(encoding="utf-8")
                for path in sorted((root / "state" / "logs").glob("*.jsonl"))
            )
            for secret in (
                "BOOTSTRAP-LIFECYCLE-SECRET",
                "SESSION-LIFECYCLE-SECRET",
                str(config_file),
            ):
                self.assertNotIn(secret, persisted)

    def test_startup_failure_is_logged_safely_and_original_exception_is_raised(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_file = root / "config.json"
            save(normalize({}), config_file)
            failure = RuntimeError("STARTUP-EXCEPTION-MESSAGE-SECRET")

            with patch(
                "easy_multi_provider.server.ensure_master_key",
                side_effect=failure,
            ):
                with self.assertRaises(RuntimeError) as raised:
                    serve(config_file)

            self.assertIs(raised.exception, failure)
            records = read_journal_records(root)
            failures = [
                record for record in records if record["event"] == "startup_failure"
            ]
            self.assertEqual(len(failures), 1)
            self.assertEqual(failures[0]["fields"]["stage"], "ensure_master_key")
            self.assertEqual(
                failures[0]["fields"]["exception_class"],
                "RuntimeError",
            )
            persisted = repr(records)
            self.assertNotIn("STARTUP-EXCEPTION-MESSAGE-SECRET", persisted)
            self.assertNotIn(str(config_file), persisted)

    def test_null_and_failing_journals_do_not_prevent_graceful_serve(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_file = root / "config.json"
            save(normalize({}), config_file)

            with self.subTest(journal="creation failure"), patch(
                "easy_multi_provider.server.create_journal",
                side_effect=OSError("journal setup failure"),
            ), fake_serve_dependencies(root), contextlib.redirect_stdout(io.StringIO()):
                serve(config_file)

            with self.subTest(journal="operation failure"), patch(
                "easy_multi_provider.server.create_journal",
                return_value=FailingJournal(),
            ), fake_serve_dependencies(root), contextlib.redirect_stdout(io.StringIO()):
                serve(config_file)

    def test_serve_forever_failure_is_logged_safely_and_re_raised(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_file = root / "config.json"
            save(normalize({}), config_file)
            journal = CapturingJournal()
            failure = RuntimeError("SERVE-FOREVER-MESSAGE-SECRET")

            with patch(
                "easy_multi_provider.server.create_journal",
                return_value=journal,
            ), fake_serve_dependencies(
                root,
                serve_error=failure,
            ), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(RuntimeError) as raised:
                    serve(config_file)

            self.assertIs(raised.exception, failure)
            self.assertEqual(
                journal.exceptions,
                [{
                    "level": "error",
                    "event": "internal_error",
                    "stage": "serve_forever",
                    "exception_class": "RuntimeError",
                }],
            )
            self.assertNotIn("SERVE-FOREVER-MESSAGE-SECRET", repr(journal.exceptions))

    def test_shutdown_restore_result_failure_and_close_order_are_safe(self):
        class OrderedJournal(CapturingJournal):
            def __init__(self, order):
                super().__init__()
                self.order = order

            @property
            def enabled(self):
                return False

            @property
            def current_path(self):
                return None

            def event(self, level, event_name, **fields):
                self.order.append("event:" + event_name)
                super().event(level, event_name, **fields)

            def close(self):
                self.order.append("journal_close")

        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_file = root / "config.json"
            save(normalize({}), config_file)
            order = []
            journal = OrderedJournal(order)
            restore = lifecycle_result(
                action="restored",
                state="native",
                relation="original",
            )
            with patch(
                "easy_multi_provider.server.create_journal",
                return_value=journal,
            ), fake_serve_dependencies(
                root,
                order=order,
                restore_result=restore,
            ), contextlib.redirect_stdout(io.StringIO()):
                serve(config_file)

            self.assertLess(order.index("event:shutdown_start"), order.index("shutdown_restore"))
            self.assertLess(order.index("shutdown_restore"), order.index("server_close"))
            self.assertLess(order.index("server_close"), order.index("event:shutdown_complete"))
            self.assertLess(order.index("event:shutdown_complete"), order.index("journal_close"))
            shutdown = management_events(journal, "shutdown_complete")[0]
            self.assertEqual(
                {
                    key: shutdown[key]
                    for key in ("reason", "action", "state", "relation", "result_class")
                },
                {
                    "reason": "sigterm",
                    "action": "restored",
                    "state": "native",
                    "relation": "original",
                    "result_class": "success",
                },
            )

            failure_order = []
            failure_journal = OrderedJournal(failure_order)
            restore_failure = RuntimeError("SHUTDOWN-RESTORE-MESSAGE-SECRET")
            with patch(
                "easy_multi_provider.server.create_journal",
                return_value=failure_journal,
            ), fake_serve_dependencies(
                root,
                order=failure_order,
                restore_error=restore_failure,
            ), contextlib.redirect_stdout(io.StringIO()):
                with self.assertRaises(RuntimeError) as raised:
                    serve(config_file)

            self.assertIs(raised.exception, restore_failure)
            failed_shutdown = management_events(
                failure_journal, "shutdown_complete"
            )[0]
            self.assertEqual(failed_shutdown["result_class"], "RuntimeError")
            self.assertNotIn(
                "SHUTDOWN-RESTORE-MESSAGE-SECRET",
                repr(failure_journal.events),
            )
            self.assertLess(
                failure_order.index("event:shutdown_complete"),
                failure_order.index("journal_close"),
            )


if __name__ == "__main__":
    unittest.main()
