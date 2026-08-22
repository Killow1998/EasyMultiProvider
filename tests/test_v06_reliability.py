import io
import unittest
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.router import (
    _reliable_responses_stream,
    _response_failure_frame,
    _sse_frame,
)
from easy_multi_provider.server import ObservationRing
from easy_multi_provider.transport import WebSocketConnection, sse_json_events


def _event(event_type, **value):
    return _sse_frame(event_type, {"type": event_type, **value})


class StreamReliabilityTests(unittest.TestCase):
    def test_pre_output_close_retries_once_without_duplicate_events(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            if len(attempts) == 1:
                return iter(
                    [
                        _event("response.created", response={"id": "resp_fixture"}),
                        _response_failure_frame(
                            "upstream closed before terminal",
                            error_class="upstream_close_pre_output",
                        ),
                    ]
                )
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event("response.output_text.delta", delta="visible"),
                    _event(
                        "response.output_item.added",
                        item={"type": "function_call", "call_id": "call_fixture"},
                    ),
                    _event("response.completed", response={"id": "resp_fixture"}),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1, 2])
        self.assertEqual(
            [event["type"] for event in events],
            [
                "response.created",
                "response.output_text.delta",
                "response.output_item.added",
                "response.completed",
            ],
        )
        self.assertEqual(len(terminals), 1)
        self.assertTrue(terminals[0]["success"])
        self.assertTrue(terminals[0]["output_emitted"])
        self.assertTrue(terminals[0]["tool_activity"])
        self.assertTrue(terminals[0]["terminal_event_observed"])
        self.assertTrue(terminals[0]["recovery_succeeded"])

    def test_partial_output_close_fails_once_without_replay(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event("response.output_text.delta", delta="visible"),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(factory, terminal_callback=terminals.append)
            )
        )

        self.assertEqual(attempts, [1])
        self.assertEqual(
            [event["type"] for event in events],
            ["response.created", "response.output_text.delta", "response.failed"],
        )
        self.assertEqual(len(terminals), 1)
        self.assertEqual(terminals[0]["error_class"], "upstream_close_after_output")
        self.assertTrue(terminals[0]["output_emitted"])
        self.assertFalse(terminals[0]["recovery_succeeded"])

    def test_tool_activity_prevents_generation_replay(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event(
                        "response.output_item.added",
                        item={"type": "function_call", "call_id": "call_fixture"},
                    ),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1])
        self.assertEqual(events[-1]["type"], "response.failed")
        self.assertEqual(sum(event["type"] == "response.failed" for event in events), 1)
        self.assertEqual(terminals[0]["error_class"], "upstream_close_after_tool")
        self.assertTrue(terminals[0]["tool_activity"])

    def test_duplicate_terminal_is_suppressed(self):
        terminals = []

        def factory():
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event("response.completed", response={"id": "resp_fixture"}),
                    _event("response.completed", response={"id": "resp_duplicate"}),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(factory, terminal_callback=terminals.append)
            )
        )

        self.assertEqual(sum(event["type"] == "response.completed" for event in events), 1)
        self.assertEqual(len(terminals), 1)

    def test_pre_output_recovery_is_attempted_at_most_once(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _response_failure_frame(
                        "upstream closed before terminal",
                        error_class="upstream_close_pre_output",
                    ),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1, 2])
        self.assertEqual(sum(event["type"] == "response.failed" for event in events), 1)
        self.assertEqual(len(terminals), 1)
        self.assertFalse(terminals[0]["recovery_succeeded"])
        self.assertEqual(terminals[0]["recovery_mode"], "pre_output_retry")

    def test_production_responses_proxy_does_not_replay_pre_output_failure(self):
        terminals = []
        first = iter(
            [
                _event("response.created", response={"id": "resp_fixture"}),
                _response_failure_frame(
                    "upstream closed before terminal",
                    error_class="stream_incomplete",
                ),
            ]
        )
        second = iter(
            [
                _event("response.created", response={"id": "resp_fixture"}),
                _event("response.output_text.delta", delta="visible"),
                _event("response.completed", response={"id": "resp_fixture"}),
            ]
        )
        provider = {
            "id": "provider-fixture",
            "protocol": "responses",
            "auth_mode": "api_key",
        }
        model = {"id": "provider-fixture/model-fixture", "upstream_id": "model-fixture"}
        body = {"model": model["id"], "input": [], "stream": True}

        with patch(
            "easy_multi_provider.router.forward_responses_stream",
            side_effect=[first, second],
        ) as upstream:
            metadata, result = router._proxy_resolved(
                provider,
                model,
                body,
                {},
                terminal_callback=terminals.append,
            )
            events = list(sse_json_events(result))

        self.assertEqual(upstream.call_count, 1)
        self.assertEqual(metadata["kind"], "stream")
        self.assertEqual(
            [event["type"] for event in events],
            ["response.created", "response.failed"],
        )
        self.assertTrue(terminals[0]["terminal_event_observed"])
        self.assertFalse(terminals[0]["recovery_succeeded"])


class BoundaryDiagnosticsTests(unittest.TestCase):
    def test_lifecycle_diagnostics_are_bounded_and_content_free(self):
        ring = ObservationRing()
        ring.record(
            {
                "route": "responses",
                "resolved_protocol": "responses",
                "transport": "websocket",
                "close_code": 1006,
                "error_class": "upstream_close_after_output",
                "output_emitted": True,
                "tool_activity": False,
                "terminal_event_observed": False,
                "recovery_succeeded": False,
                "recovery_mode": "none",
                "unsafe_content": "must-not-survive",
                "authorization": "must-not-survive",
            }
        )

        record = ring.snapshot()["records"][0]

        self.assertEqual(record["close_code"], 1006)
        self.assertTrue(record["output_emitted"])
        self.assertFalse(record["tool_activity"])
        self.assertFalse(record["terminal_event_observed"])
        self.assertFalse(record["recovery_succeeded"])
        self.assertEqual(record["recovery_mode"], "none")
        self.assertNotIn("unsafe_content", record)
        self.assertNotIn("authorization", record)

    def test_websocket_records_peer_close_code_without_reason(self):
        mask = b"\x01\x02\x03\x04"
        payload = b"\x03\xe9private reason"
        encoded = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        reader = io.BytesIO(bytes((0x88, 0x80 | len(payload))) + mask + encoded)
        writer = io.BytesIO()
        websocket = WebSocketConnection(reader, writer)

        self.assertIsNone(websocket.receive_text())
        self.assertEqual(websocket.peer_close_code, 1001)
        self.assertFalse(hasattr(websocket, "peer_close_reason"))


if __name__ == "__main__":
    unittest.main()
