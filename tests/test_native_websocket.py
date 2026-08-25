import json
import unittest
from unittest.mock import patch

from easy_multi_provider.native_websocket import (
    MAX_NATIVE_WEBSOCKET_REQUEST_BYTES,
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
    def test_large_uncompressed_request_uses_http_transport(self):
        self.assertTrue(
            native_websocket_request_fits(
                {"type": "response.create", "input": "small"}
            )
        )
        self.assertFalse(
            native_websocket_request_fits(
                {
                    "type": "response.create",
                    "input": "x" * MAX_NATIVE_WEBSOCKET_REQUEST_BYTES,
                }
            )
        )

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

    def test_default_connector_disables_credential_redirects(self):
        target = NativeWebSocketTarget(
            "wss://example.invalid/responses",
            {"Authorization": "Bearer test-only"},
            "route-a",
        )
        connection = _FakeConnection([])
        with patch("websocket.create_connection", return_value=connection) as opened:
            self.assertIs(_default_connector(target), connection)
        options = opened.call_args.kwargs
        self.assertEqual(options["redirect_limit"], 0)
        self.assertTrue(options["suppress_origin"])
        self.assertEqual(options["header"]["Authorization"], "Bearer test-only")

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

    def test_long_stream_uses_idle_timeout_without_absolute_deadline(self):
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

    def test_idle_timeout_allows_http_fallback_before_output(self):
        connection = _FakeConnection([])

        def timed_out():
            raise TimeoutError("idle")

        connection.recv = timed_out
        bridge = NativeWebSocketBridge(lambda _target: connection)

        with self.assertRaisesRegex(NativeWebSocketError, "idle") as raised:
            list(
                bridge.events(
                    NativeWebSocketTarget(
                        "wss://example.invalid/responses", {}, "route-a"
                    ),
                    {"type": "response.create", "input": []},
                )
            )

        self.assertEqual(raised.exception.status, 504)
        self.assertTrue(raised.exception.retryable)

    def test_cumulative_response_size_is_bounded(self):
        connection = _FakeConnection(
            [
                {"type": "response.created", "padding": "x" * 80},
                {"type": "response.output_text.delta", "delta": "y" * 80},
                {"type": "response.completed", "response": {"status": "completed"}},
            ]
        )
        bridge = NativeWebSocketBridge(lambda _target: connection)
        with patch(
            "easy_multi_provider.native_websocket.MAX_NATIVE_WEBSOCKET_RESPONSE_BYTES",
            180,
        ):
            with self.assertRaisesRegex(NativeWebSocketError, "response is too large"):
                list(
                    bridge.events(
                        NativeWebSocketTarget(
                            "wss://example.invalid/responses", {}, "route-a"
                        ),
                        {"type": "response.create", "input": []},
                    )
                )

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


if __name__ == "__main__":
    unittest.main()
