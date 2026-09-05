import json
import unittest
from unittest.mock import patch

import easy_multi_provider.native_websocket as native_websocket
from easy_multi_provider.native_websocket import (
    MAX_NATIVE_WEBSOCKET_UNCOMPRESSED_REQUEST_BYTES,
    NativeWebSocketBridge,
    NativeWebSocketError,
    NativeWebSocketTarget,
    _default_connector,
    native_websocket_request_fits,
    terminal_observation,
)


class _Handshake:
    def __init__(self, status=101):
        self.status = status


class _FakeConnection:
    def __init__(self, responses, status=101):
        self.handshake_response = _Handshake(status)
        self.responses = list(responses)
        self.sent = []
        self.connected = True
        self.closed = False
        self.timeout = None

    def settimeout(self, value):
        self.timeout = value

    def send(self, value):
        self.sent.append(json.loads(value))

    def recv(self):
        if not self.responses:
            return ""
        return json.dumps(self.responses.pop(0))

    def shutdown(self):
        self.connected = False
        self.closed = True

    def close(self):
        self.shutdown()


class _GatewayFailure(Exception):
    status_code = 502


class NativeWebSocketTests(unittest.TestCase):
    def test_bridge_reports_sanitized_transport_phases(self):
        phases = []
        connection = _FakeConnection(
            [
                {"type": "response.output_text.delta", "delta": "private output"},
                {
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed"},
                },
            ]
        )
        target = NativeWebSocketTarget(
            "wss://private.example/responses",
            {"Authorization": "Bearer private-token"},
            "private-route",
        )
        bridge = NativeWebSocketBridge(
            lambda _target: connection,
            observer=lambda phase, **fields: phases.append((phase, fields)),
        )

        list(bridge.events(target, {"type": "response.create", "input": "secret"}))

        self.assertEqual(
            [phase for phase, _ in phases],
            [
                "upstream_handshake_started",
                "upstream_handshake_accepted",
                "upstream_request_sent",
                "upstream_first_event_received",
                "upstream_terminal_received",
            ],
        )
        self.assertEqual(phases[1][1]["status"], 101)
        self.assertGreater(phases[2][1]["request_bytes"], 0)
        self.assertNotIn("private", json.dumps(phases))

    def test_large_request_requires_compressed_client(self):
        self.assertTrue(
            native_websocket_request_fits(
                {"type": "response.create", "input": "small"}
            )
        )
        large = {
            "type": "response.create",
            "input": "x" * MAX_NATIVE_WEBSOCKET_UNCOMPRESSED_REQUEST_BYTES,
        }
        with patch.object(
            native_websocket,
            "compressed_native_websocket_available",
            return_value=False,
        ):
            self.assertFalse(native_websocket_request_fits(large))
        with patch.object(
            native_websocket,
            "compressed_native_websocket_available",
            return_value=True,
        ):
            self.assertTrue(native_websocket_request_fits(large))

    @unittest.skipUnless(
        native_websocket.compressed_native_websocket_available(),
        "compressed WebSocket client is optional on Python 3.8",
    )
    def test_default_connector_enables_deflate_and_system_proxy(self):
        class Response:
            status_code = 101

        class State:
            name = "OPEN"

        class Connection:
            response = Response()
            state = State()

            def close(self):
                pass

        target = NativeWebSocketTarget(
            "wss://example.invalid/responses",
            {"Authorization": "Bearer test-only"},
            "route-a",
        )
        connection = Connection()
        with patch("websockets.sync.client.connect", return_value=connection) as opened:
            wrapped = _default_connector(target)

        self.assertTrue(wrapped.connected)
        options = opened.call_args.kwargs
        self.assertEqual(options["compression"], "deflate")
        self.assertTrue(options["proxy"])
        self.assertEqual(options["additional_headers"]["Authorization"], "Bearer test-only")
        self.assertEqual(options["max_size"], native_websocket.MAX_NATIVE_WEBSOCKET_EVENT_BYTES)

    def test_compressed_connection_adapter_supports_bridge_continuity(self):
        class Response:
            status_code = 101

        class State:
            name = "OPEN"

        class Acknowledgement:
            def __init__(self):
                self.timeout = None

            def wait(self, timeout):
                self.timeout = timeout
                return True

        class Connection:
            response = Response()
            state = State()

            def __init__(self):
                self.sent = []
                self.receive_timeouts = []
                self.acknowledgement = Acknowledgement()
                self.closed = False

            def send(self, value):
                self.sent.append(json.loads(value))

            def recv(self, timeout):
                self.receive_timeouts.append(timeout)
                return json.dumps(
                    {
                        "type": "response.completed",
                        "response": {"id": "resp_1", "status": "completed"},
                    }
                )

            def ping(self, _payload):
                return self.acknowledgement

            def close(self):
                self.closed = True

        connection = Connection()
        wrapped = native_websocket._CompressedWebSocketConnection(connection)
        target = NativeWebSocketTarget(
            "wss://example.invalid/responses", {}, "route-a"
        )
        bridge = NativeWebSocketBridge(lambda _target: wrapped)

        events = list(
            bridge.events(
                target,
                {"type": "response.create", "input": ["large-history"]},
            )
        )

        self.assertEqual(events[-1]["response"]["id"], "resp_1")
        self.assertEqual(connection.sent[0]["input"], ["large-history"])
        self.assertTrue(connection.receive_timeouts)
        self.assertTrue(bridge.can_continue(target))
        self.assertEqual(
            connection.acknowledgement.timeout,
            native_websocket.NATIVE_WEBSOCKET_REUSE_PROBE_TIMEOUT,
        )

    def test_default_connector_falls_back_when_compressed_client_is_unavailable(self):
        connection = _FakeConnection([])
        target = NativeWebSocketTarget(
            "wss://example.invalid/responses", {}, "route-a"
        )
        with patch.object(
            native_websocket, "_compressed_connector", side_effect=ImportError
        ), patch.object(
            native_websocket, "_legacy_connector", return_value=connection
        ) as legacy:
            self.assertIs(_default_connector(target), connection)
        legacy.assert_called_once_with(target)

    def test_gateway_failure_before_request_allows_http_fallback(self):
        def fail(_target):
            raise _GatewayFailure()

        bridge = NativeWebSocketBridge(fail)

        with self.assertRaises(NativeWebSocketError) as raised:
            bridge.connect(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                )
            )

        self.assertEqual(raised.exception.status, 502)
        self.assertTrue(raised.exception.retryable)

    def test_reuses_matching_connection_and_preserves_incremental_request(self):
        connection = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed"},
                },
                {
                    "type": "response.completed",
                    "response": {"id": "resp_2", "status": "completed"},
                },
            ]
        )
        calls = []

        def connector(target):
            calls.append(target)
            return connection

        target = NativeWebSocketTarget(
            "wss://example.invalid/v1/responses",
            {"Authorization": "Bearer test-only"},
            "sha256:route-a",
        )
        bridge = NativeWebSocketBridge(connector)
        first = list(
            bridge.events(
                target,
                {"type": "response.create", "model": "m", "input": ["first"]},
            )
        )
        second = list(
            bridge.events(
                target,
                {
                    "type": "response.create",
                    "model": "m",
                    "previous_response_id": "resp_1",
                    "input": ["delta"],
                },
            )
        )

        self.assertEqual(len(calls), 1)
        self.assertEqual(first[-1]["response"]["id"], "resp_1")
        self.assertEqual(second[-1]["response"]["id"], "resp_2")
        self.assertEqual(connection.sent[1]["input"], ["delta"])
        self.assertEqual(connection.sent[1]["previous_response_id"], "resp_1")
        self.assertTrue(bridge.last_connection_reused)

    def test_disconnected_matching_socket_cannot_continue_incrementally(self):
        connection = _FakeConnection([])
        target = NativeWebSocketTarget(
            "wss://example.invalid/v1/responses", {}, "sha256:route-a"
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)

        bridge.connect(target)
        connection.connected = False

        self.assertFalse(bridge.can_continue(target))

    def test_reuse_probe_keeps_a_live_socket(self):
        connection = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {"id": "one", "status": "completed"},
                },
                {
                    "type": "response.completed",
                    "response": {"id": "two", "status": "completed"},
                },
            ]
        )
        pings = []
        connection.ping = pings.append

        class Pong:
            @property
            def data(self):
                return pings[-1].encode("ascii")

        connection.recv_data_frame = lambda control_frame=False: (0xA, Pong())
        calls = []
        target = NativeWebSocketTarget(
            "wss://example.invalid/v1/responses", {}, "sha256:route-a"
        )
        bridge = NativeWebSocketBridge(lambda item: calls.append(item) or connection)

        list(bridge.events(target, {"type": "response.create", "input": []}))
        self.assertTrue(bridge.can_continue(target))
        list(bridge.events(target, {"type": "response.create", "input": []}))

        self.assertEqual(len(calls), 1)
        self.assertEqual(len(pings), 1)
        self.assertTrue(bridge.last_connection_reused)

    def test_reuse_probe_discards_a_stale_socket_before_next_request(self):
        first = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {"id": "one", "status": "completed"},
                },
            ]
        )
        second = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {"id": "two", "status": "completed"},
                },
            ]
        )
        first.ping = lambda _payload: None

        class Close:
            data = b""

        first.recv_data_frame = lambda control_frame=False: (0x8, Close())
        connections = iter((first, second))
        target = NativeWebSocketTarget(
            "wss://example.invalid/v1/responses", {}, "sha256:route-a"
        )
        bridge = NativeWebSocketBridge(lambda _target: next(connections))

        list(bridge.events(target, {"type": "response.create", "input": []}))

        self.assertFalse(bridge.can_continue(target))
        self.assertTrue(first.closed)
        list(bridge.events(target, {"type": "response.create", "input": []}))
        self.assertFalse(bridge.last_connection_reused)

    def test_route_change_closes_old_connection(self):
        first = _FakeConnection(
            [{"type": "response.completed", "response": {"status": "completed"}}]
        )
        second = _FakeConnection(
            [{"type": "response.completed", "response": {"status": "completed"}}]
        )
        connections = iter((first, second))
        bridge = NativeWebSocketBridge(lambda _target: next(connections))

        list(
            bridge.events(
                NativeWebSocketTarget("wss://example.invalid/responses", {}, "route-a"),
                {"type": "response.create", "input": []},
            )
        )
        list(
            bridge.events(
                NativeWebSocketTarget("wss://example.invalid/responses", {}, "route-b"),
                {"type": "response.create", "input": []},
            )
        )

        self.assertTrue(first.closed)
        self.assertFalse(second.closed)

    def test_missing_terminal_closes_connection(self):
        connection = _FakeConnection([{"type": "response.created"}])
        bridge = NativeWebSocketBridge(lambda _target: connection)
        with self.assertRaisesRegex(NativeWebSocketError, "terminal"):
            list(
                bridge.events(
                    NativeWebSocketTarget(
                        "wss://example.invalid/responses", {}, "route-a"
                    ),
                    {"type": "response.create", "input": []},
                )
            )
        self.assertTrue(connection.closed)

    def test_reused_connection_failure_keeps_request_reuse_diagnostic(self):
        connection = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {"id": "resp_1", "status": "completed"},
                }
            ]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)
        target = NativeWebSocketTarget(
            "wss://example.invalid/responses", {}, "route-a"
        )
        list(bridge.events(target, {"type": "response.create", "input": []}))
        with self.assertRaises(NativeWebSocketError):
            list(
                bridge.events(
                    target,
                    {
                        "type": "response.create",
                        "previous_response_id": "resp_1",
                        "input": [],
                    },
                )
            )
        self.assertTrue(bridge.last_connection_reused)

    def test_rejected_upgrade_never_follows_redirect(self):
        connection = _FakeConnection([], status=302)
        bridge = NativeWebSocketBridge(lambda _target: connection)
        with self.assertRaisesRegex(NativeWebSocketError, "upgrade") as raised:
            bridge.connect(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                )
            )
        self.assertEqual(raised.exception.status, 302)
        self.assertTrue(connection.closed)

    def test_auth_and_rate_limit_upgrades_are_not_retryable(self):
        for status in (401, 403, 429):
            with self.subTest(status=status):
                connection = _FakeConnection([], status=status)
                bridge = NativeWebSocketBridge(lambda _target: connection)
                with self.assertRaises(NativeWebSocketError) as raised:
                    bridge.connect(
                        NativeWebSocketTarget(
                            "wss://example.invalid/responses", {}, "route-a"
                        )
                    )
                self.assertFalse(raised.exception.retryable)

    def test_bad_request_upgrade_can_fall_back_to_http_before_output(self):
        connection = _FakeConnection([], status=400)
        bridge = NativeWebSocketBridge(lambda _target: connection)

        with self.assertRaises(NativeWebSocketError) as raised:
            bridge.connect(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                )
            )

        self.assertEqual(raised.exception.status, 400)
        self.assertTrue(raised.exception.retryable)

    def test_first_output_uses_codex_idle_timeout(self):
        connection = _FakeConnection(
            [{"type": "response.completed", "response": {"status": "completed"}}]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)
        events = list(
            bridge.events(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                ),
                {"type": "response.create", "input": []},
            )
        )

        self.assertEqual(events[-1]["type"], "response.completed")
        self.assertEqual(connection.timeout, 300)

    def test_timeout_after_send_never_allows_http_replay(self):
        connection = _FakeConnection([])

        def timed_out():
            raise TimeoutError("idle")

        connection.recv = timed_out
        bridge = NativeWebSocketBridge(lambda _target: connection)

        with self.assertRaisesRegex(NativeWebSocketError, "no output") as raised:
            list(
                bridge.events(
                    NativeWebSocketTarget(
                        "wss://example.invalid/responses", {}, "route-a"
                    ),
                    {"type": "response.create", "input": []},
                )
            )

        self.assertEqual(raised.exception.status, 504)
        self.assertFalse(raised.exception.retryable)
        self.assertTrue(raised.exception.request_sent)
        self.assertEqual(raised.exception.error_class, "first_output_timeout")

    def test_output_switches_to_long_stream_idle_timeout(self):
        connection = _FakeConnection(
            [
                {"type": "response.output_text.delta", "delta": "x"},
                {"type": "response.completed", "response": {"status": "completed"}},
            ]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)

        list(
            bridge.events(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                ),
                {"type": "response.create", "input": []},
            )
        )

        self.assertEqual(connection.timeout, 300)

    def test_response_has_no_extra_cumulative_size_limit(self):
        connection = _FakeConnection(
            [
                {"type": "response.created", "padding": "x" * 80},
                {"type": "response.output_text.delta", "delta": "y" * 80},
                {"type": "response.completed", "response": {"status": "completed"}},
            ]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)
        events = list(
            bridge.events(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                ),
                {"type": "response.create", "input": []},
            )
        )

        self.assertEqual(events[-1]["type"], "response.completed")

    def test_contradictory_completed_event_is_failure(self):
        terminal = terminal_observation(
            {
                "type": "response.completed",
                "response": {"status": "failed", "error": {"code": "bad"}},
            }
        )
        self.assertFalse(terminal["success"])
        self.assertEqual(terminal["error_class"], "stream_error")

    def test_contradictory_completion_is_replaced_before_it_is_forwarded(self):
        connection = _FakeConnection(
            [
                {
                    "type": "response.completed",
                    "response": {
                        "id": "resp_bad",
                        "status": "failed",
                        "error": {"message": "Bearer private-upstream-token"},
                    },
                }
            ]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)

        events = list(
            bridge.events(
                NativeWebSocketTarget(
                    "wss://example.invalid/responses", {}, "route-a"
                ),
                {"type": "response.create", "input": []},
            )
        )

        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["type"], "response.failed")
        self.assertEqual(events[0]["response"]["id"], "resp_bad")
        self.assertNotIn("private-upstream-token", json.dumps(events[0]))

    def test_unknown_completed_status_is_failure(self):
        terminal = terminal_observation(
            {
                "type": "response.completed",
                "response": {"status": "mystery"},
            }
        )

        self.assertFalse(terminal["success"])
        self.assertEqual(terminal["error_class"], "stream_error")

    def test_wrapped_error_preserves_upstream_status(self):
        terminal = terminal_observation(
            {
                "type": "error",
                "status": 429,
                "error": {"code": "rate_limit_exceeded"},
            }
        )

        self.assertEqual(terminal["status"], 429)
        self.assertEqual(terminal["error_class"], "rate_limit")


if __name__ == "__main__":
    unittest.main()
