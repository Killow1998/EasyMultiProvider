import json
import io
import ssl
import threading
import unittest
from unittest.mock import Mock, patch

import easy_multi_provider.native_websocket as native_websocket
from easy_multi_provider.transport import WebSocketConnection, WebSocketProtocolError
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
    def test_preencoded_downstream_json_matches_normal_frame_and_keeps_size_limit(self):
        event = {"type": "response.output_text.delta", "delta": "猫🐱"}
        encoded = json.dumps(event, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        normal, reused = io.BytesIO(), io.BytesIO()
        WebSocketConnection(None, normal).send_json(event)
        connection = WebSocketConnection(None, reused)
        connection.send_json_bytes(encoded)
        self.assertEqual(normal.getvalue(), reused.getvalue())
        with patch("easy_multi_provider.transport.MAX_WEBSOCKET_MESSAGE_BYTES", len(encoded) - 1):
            with self.assertRaises(WebSocketProtocolError):
                connection.send_json_bytes(encoded)
        self.assertEqual(normal.getvalue(), reused.getvalue())

    def test_incremental_collaboration_restores_plaintext_without_tools(self):
        terminal = {"type": "response.completed", "response": {"status": "completed"}}
        item = {"type": "function_call", "namespace": "emp_collaboration",
                "name": "spawn_agent", "arguments": '{"message":"test"}'}
        connection = _FakeConnection([terminal,
            {"type": "response.output_item.done", "item": item}, terminal])
        bridge = NativeWebSocketBridge(lambda _target: connection)
        target = NativeWebSocketTarget("wss://example.test/responses", {}, "route")
        list(bridge.events(target, {"tools": [{"type": "namespace", "name": "emp_collaboration"}]}))
        result = list(bridge.events(target, {"previous_response_id": "resp_1", "input": []}))
        self.assertEqual(result[0]["item"]["namespace"], "collaboration")
        self.assertEqual(result[0]["item"]["encrypted_function_args"], [])

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

    def test_failure_diagnostics_preserve_tls_cause_without_private_text(self):
        import ssl
        from urllib.error import URLError
        from easy_multi_provider.diagnostic_journal import exception_details
        cause = ssl.SSLCertVerificationError(1, "private prompt bearer secret")
        cause.verify_code = 20
        wrapped = URLError(cause)
        details = exception_details(wrapped)
        self.assertEqual(details[1]["verify_code"], 20)
        self.assertEqual(details[1]["type"], "SSLCertVerificationError")
        self.assertNotIn("private", str(details))
        wrapped.__cause__ = wrapped
        self.assertLessEqual(len(exception_details(wrapped)), 8)

    def test_transport_failure_snapshot_precedes_connection_cleanup(self):
        events = []
        bridge = NativeWebSocketBridge(observer=lambda phase, **fields: events.append((phase, fields)))
        class Connection:
            def diagnostic_state(self):
                return {"receive_queue_size": 17, "receive_paused": True}
        bridge._connection = Connection()
        bridge._observe_failure(TimeoutError(), True)
        self.assertEqual(events[0][0], "upstream_transport_failed")
        self.assertEqual(events[0][1]["receive_queue_size"], 17)
        self.assertTrue(events[0][1]["request_sent"])

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

            def __enter__(self):
                self.entered = True
                return self

            def __exit__(self, *_args):
                self.close()

            def close(self):
                self.closed = True

        target = NativeWebSocketTarget(
            "wss://example.invalid/responses",
            {"Authorization": "Bearer test-only"},
            "route-a",
            "http://127.0.0.1:7890",
        )
        connection = Connection()
        with patch("websockets.sync.client.connect", return_value=connection) as opened:
            wrapped = _default_connector(target)

        self.assertTrue(wrapped.connected)
        self.assertTrue(connection.entered)
        wrapped.close()
        self.assertTrue(connection.closed)
        options = opened.call_args.kwargs
        self.assertEqual(options["compression"], "deflate")
        self.assertEqual(options["proxy"], "http://127.0.0.1:7890")
        self.assertEqual(options["ping_timeout"], 20)
        self.assertEqual(options["ping_interval"], 20)
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
        self.assertIsNone(connection.acknowledgement.timeout)
        bridge._last_healthy_at -= native_websocket.NATIVE_WEBSOCKET_HEALTH_FRESHNESS
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

    def test_gateway_failure_does_not_imply_websocket_is_unsupported(self):
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
        self.assertFalse(raised.exception.retryable)

    def test_tls_handshake_failure_can_be_retried_by_the_client(self):
        connection = _FakeConnection([{
            "type": "response.completed", "response": {"id": "resp_retried", "status": "completed"},
        }])
        target = NativeWebSocketTarget("wss://example.invalid/responses", {}, "route-a")
        connector = Mock(side_effect=[ssl.SSLEOFError(8, "unexpected EOF"), connection])
        bridge = NativeWebSocketBridge(connector)
        request = {"type": "response.create", "model": "m", "input": ["hello"]}
        with self.assertRaises(NativeWebSocketError) as raised:
            list(bridge.events(target, request))
        self.assertEqual(raised.exception.error_class, "tls_failure")
        self.assertFalse(raised.exception.retryable)
        self.assertFalse(raised.exception.request_sent)
        self.assertEqual(connection.sent, [])
        self.assertEqual(list(bridge.events(target, request))[-1]["response"]["id"], "resp_retried")
        self.assertEqual(len(connection.sent), 1)

    def test_only_upgrade_incompatibility_allows_immediate_http_fallback(self):
        for status in [400, 404, 405, 415, 426, 501]:
            with self.subTest(status=status):
                self.assertTrue(NativeWebSocketError("upgrade unavailable", status).retryable)
        for status in [401, 403, 429, 500, 502, 503, 504]:
            with self.subTest(status=status):
                self.assertFalse(NativeWebSocketError("request failed", status).retryable)

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
        self.assertEqual(pings, [])
        # Idle reuse still probes. Successful continuation refreshes health.
        bridge._last_healthy_at -= native_websocket.NATIVE_WEBSOCKET_HEALTH_FRESHNESS
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

        bridge._last_healthy_at -= native_websocket.NATIVE_WEBSOCKET_HEALTH_FRESHNESS
        self.assertFalse(bridge.can_continue(target))
        self.assertTrue(first.closed)
        list(bridge.events(target, {"type": "response.create", "input": []}))
        self.assertFalse(bridge.last_connection_reused)

    def test_probe_health_expires_even_when_no_request_was_sent(self):
        connection = _FakeConnection([])
        connection.probe = Mock(return_value=True)
        target = NativeWebSocketTarget("wss://example.test/responses", {}, "route")
        bridge = NativeWebSocketBridge(lambda _: connection)
        bridge.connect(target)
        self.assertTrue(bridge.can_continue(target))
        self.assertTrue(bridge.can_continue(target))
        self.assertEqual(connection.probe.call_count, 1)
        bridge._last_healthy_at -= native_websocket.NATIVE_WEBSOCKET_HEALTH_FRESHNESS
        connection.probe.return_value = False
        self.assertFalse(bridge.can_continue(target))
        self.assertTrue(connection.closed)

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

    def test_peer_1009_before_output_is_safe_for_http_fallback(self):
        class CloseFrame:
            code = 1009

        class MessageTooBig(Exception):
            sent = CloseFrame()
            rcvd = CloseFrame()
            rcvd_then_sent = True

        connection = _FakeConnection([])
        connection.send = Mock(side_effect=MessageTooBig())
        bridge = NativeWebSocketBridge(lambda _target: connection)

        with self.assertRaises(NativeWebSocketError) as raised:
            list(
                bridge.events(
                    NativeWebSocketTarget(
                        "wss://example.invalid/responses", {}, "route-a"
                    ),
                    {"type": "response.create", "input": ["large"]},
                )
            )

        self.assertEqual(raised.exception.status, 413)
        self.assertEqual(raised.exception.close_code, 1009)
        self.assertTrue(raised.exception.request_sent)
        self.assertTrue(raised.exception.http_fallback_safe)
        self.assertEqual(
            raised.exception.failure_reason,
            "upstream_websocket_message_too_large",
        )
        self.assertTrue(connection.closed)

    def test_local_1009_or_peer_1009_after_acceptance_never_replays(self):
        class CloseFrame:
            code = 1009

        class MessageTooBig(Exception):
            sent = CloseFrame()
            rcvd = CloseFrame()

        for peer_first, accepted in ((False, False), (True, True)):
            with self.subTest(peer_first=peer_first, accepted=accepted):
                failure = MessageTooBig()
                failure.rcvd_then_sent = peer_first
                connection = _FakeConnection([])
                messages = iter(
                    [json.dumps({"type": "response.created"}), failure]
                    if accepted
                    else [failure]
                )

                def receive():
                    value = next(messages)
                    if isinstance(value, BaseException):
                        raise value
                    return value

                connection.recv = receive
                bridge = NativeWebSocketBridge(lambda _target: connection)
                with self.assertRaises(NativeWebSocketError) as raised:
                    list(bridge.events(
                        NativeWebSocketTarget(
                            "wss://example.invalid/responses", {}, "route-a"
                        ),
                        {"type": "response.create", "input": []},
                    ))
                self.assertFalse(raised.exception.http_fallback_safe)
                self.assertFalse(raised.exception.retryable)

    @unittest.skipUnless(
        native_websocket.compressed_native_websocket_available(),
        "compressed WebSocket client is optional on Python 3.8",
    )
    def test_real_compressed_socket_preserves_peer_1009_and_close_order(self):
        from websockets.sync.server import serve

        phases = []

        def reject(connection):
            connection.recv()
            connection.close(1009, "fixture message too big")

        with serve(reject, "127.0.0.1", 0) as server:
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            bridge = NativeWebSocketBridge(
                observer=lambda phase, **fields: phases.append((phase, fields))
            )
            target = NativeWebSocketTarget(
                "ws://127.0.0.1:%d/responses" % server.socket.getsockname()[1],
                {},
                "route-fixture",
            )
            try:
                with self.assertRaises(NativeWebSocketError) as raised:
                    list(bridge.events(target, {"type": "response.create", "input": []}))
                self.assertEqual(raised.exception.close_code, 1009)
                self.assertTrue(raised.exception.http_fallback_safe)
                failure = next(fields for phase, fields in phases if phase == "upstream_transport_failed")
                self.assertTrue(failure["exception_chain"][0]["peer_initiated_close"])
                self.assertNotIn("fixture message too big", json.dumps(phases))
            finally:
                bridge.close()
                server.shutdown()
                thread.join(2)

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
