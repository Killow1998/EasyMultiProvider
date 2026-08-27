import json
import unittest

from easy_multi_provider.router_errors import (
    ContextLengthError,
    RouterError,
    UpstreamHTTPError,
)
from easy_multi_provider.stream_adapters import (
    _reliable_responses_stream,
    _sse_frame,
)
from easy_multi_provider.transport_failures import (
    CONNECT_TIMEOUT,
    FIRST_EVENT_TIMEOUT,
    IDLE_AFTER_OUTPUT,
    LOCAL_DEADLINE,
    PHASE_CONNECT,
    PHASE_FIRST_EVENT,
    PHASE_STREAMING,
    STREAM_INCOMPLETE,
    UPSTREAM_504,
    TransportFailure,
    protocol_fallback_allowed,
)
from easy_multi_provider.transport import (
    MAX_SSE_EVENT_BYTES,
    TransportError,
    sse_json_events,
)


def _event(event_type, **value):
    return _sse_frame(event_type, {"type": event_type, **value})


class StreamAdapterTransportMatrixTests(unittest.TestCase):
    def test_sse_parser_limits_aggregate_multiline_event_size(self):
        piece = "a" * (MAX_SSE_EVENT_BYTES // 2 + 100)
        chunks = [
            b'data: {"parts":[\n',
            ('data: "' + piece + '",\n').encode(),
            ('data: "' + piece + '"\n').encode(),
            b'data: ]}\n',
            b'\n',
        ]

        with self.assertRaises(TransportError):
            list(sse_json_events(chunks))

    def test_pre_output_retry_buffer_is_bounded(self):
        piece = "a" * (600 * 1024)
        stream = iter(
            [
                _event(
                    "response.created",
                    response={"status": "in_progress", "pad": piece},
                ),
                _event(
                    "response.created",
                    response={"status": "in_progress", "pad": piece},
                ),
                _event(
                    "response.completed",
                    response={"status": "completed", "output": []},
                ),
            ]
        )

        events = list(sse_json_events(
            _reliable_responses_stream(lambda: stream, replay_safe=False)
        ))

        self.assertEqual([event["type"] for event in events], ["response.failed"])

    def test_native_tool_argument_events_do_not_create_phantom_open_calls(self):
        completed_tool = {
            "id": "tool-1",
            "type": "function_call",
            "status": "completed",
            "call_id": "call-1",
            "name": "lookup",
            "arguments": "{}",
        }
        stream = iter(
            [
                _event(
                    "response.output_item.added",
                    output_index=0,
                    item={**completed_tool, "status": "in_progress"},
                ),
                _event(
                    "response.function_call_arguments.delta",
                    item_id="tool-1",
                    output_index=0,
                    delta="{}",
                ),
                _event(
                    "response.function_call_arguments.done",
                    item_id="tool-1",
                    output_index=0,
                    arguments="{}",
                ),
                _event(
                    "response.output_item.done",
                    output_index=0,
                    item=completed_tool,
                ),
                _event(
                    "response.completed",
                    response={"status": "completed", "output": [completed_tool]},
                ),
            ]
        )

        events = list(
            sse_json_events(
                _reliable_responses_stream(lambda: stream, replay_safe=False)
            )
        )

        self.assertEqual(events[-1]["type"], "response.completed")

    def test_transport_matrix(self):
        completed = _event(
            "response.completed",
            response={"status": "completed", "output": []},
        )

        def raising_once(error):
            attempts = []

            def factory():
                attempts.append(len(attempts) + 1)
                if len(attempts) == 1:
                    raise error
                return iter([completed])

            return attempts, factory

        def first_event_timeout_factory():
            attempts = []

            def factory():
                attempts.append(len(attempts) + 1)
                if len(attempts) == 1:
                    def stream():
                        raise TransportFailure(
                            FIRST_EVENT_TIMEOUT,
                            504,
                            PHASE_FIRST_EVENT,
                        )
                        yield b""

                    return stream()
                return iter([completed])

            return attempts, factory

        def output_idle_factory():
            def stream():
                yield _event("response.output_text.delta", delta="visible")
                raise TransportFailure(IDLE_AFTER_OUTPUT, 504, PHASE_STREAMING)

            return stream()

        def incomplete_factory():
            return iter([_event("response.created", response={"status": "in_progress"})])

        def unfinished_tool_factory():
            return iter(
                [
                    _event(
                        "response.output_item.added",
                        item={
                            "id": "tool-1",
                            "type": "function_call",
                            "status": "in_progress",
                        },
                    ),
                    completed,
                ]
            )

        cases = []
        attempts, factory = raising_once(
            TransportFailure(CONNECT_TIMEOUT, 504, PHASE_CONNECT)
        )
        cases.append(("connect timeout retries", factory, attempts, True, 2, None))
        attempts, factory = first_event_timeout_factory()
        cases.append(("first event timeout retries", factory, attempts, True, 2, None))
        cases.extend(
            [
                (
                    "upstream 504 does not retry",
                    lambda: (_ for _ in ()).throw(
                        TransportFailure(UPSTREAM_504, 504, PHASE_CONNECT)
                    ),
                    [],
                    False,
                    1,
                    UPSTREAM_504,
                ),
                (
                    "local deadline is not upstream 504",
                    lambda: (_ for _ in ()).throw(
                        RouterError("upstream request timed out", 504)
                    ),
                    [],
                    False,
                    1,
                    LOCAL_DEADLINE,
                ),
                (
                    "explicit failed does not retry",
                    lambda: iter(
                        [
                            _event(
                                "response.failed",
                                response={
                                    "status": "failed",
                                    "error": {
                                        "status": 504,
                                        "error_class": UPSTREAM_504,
                                    },
                                },
                            )
                        ]
                    ),
                    [],
                    False,
                    1,
                    UPSTREAM_504,
                ),
                (
                    "explicit incomplete does not retry",
                    lambda: iter(
                        [
                            _event(
                                "response.incomplete",
                                response={
                                    "status": "incomplete",
                                    "incomplete_details": {
                                        "reason": "max_output_tokens"
                                    },
                                },
                            )
                        ]
                    ),
                    [],
                    False,
                    1,
                    "output_limit",
                ),
                (
                    "context failure does not retry",
                    lambda: (_ for _ in ()).throw(
                        ContextLengthError(
                            {"provider_id": "fixture", "input_estimate": 9}
                        )
                    ),
                    [],
                    False,
                    1,
                    "context_length_exceeded",
                ),
                (
                    "http status does not leak content or retry",
                    lambda: (_ for _ in ()).throw(
                        UpstreamHTTPError("request-secret", 503, "upstream_capacity")
                    ),
                    [],
                    False,
                    1,
                    "upstream_5xx",
                ),
                (
                    "idle after output does not retry",
                    output_idle_factory,
                    [],
                    False,
                    1,
                    IDLE_AFTER_OUTPUT,
                ),
                (
                    "missing terminal is incomplete and does not retry",
                    incomplete_factory,
                    [],
                    False,
                    1,
                    STREAM_INCOMPLETE,
                ),
                (
                    "unfinished tool cannot complete",
                    unfinished_tool_factory,
                    [],
                    False,
                    1,
                    STREAM_INCOMPLETE,
                ),
                (
                    "invalid tool JSON cannot complete",
                    lambda: iter(
                        [
                            _event(
                                "response.completed",
                                response={
                                    "status": "completed",
                                    "output": [
                                        {
                                            "id": "tool-1",
                                            "type": "function_call",
                                            "status": "completed",
                                            "call_id": "call-1",
                                            "name": "lookup",
                                            "arguments": "{",
                                        }
                                    ],
                                },
                            )
                        ]
                    ),
                    [],
                    False,
                    1,
                    "protocol_error",
                ),
            ]
        )

        for name, factory, attempts, expect_success, expected_attempts, expected_class in cases:
            observed = []
            with self.subTest(case=name):
                events = list(
                    sse_json_events(
                        _reliable_responses_stream(
                            factory,
                            terminal_callback=observed.append,
                            replay_safe=True,
                        )
                    )
                )
                if attempts:
                    self.assertEqual(attempts, list(range(1, expected_attempts + 1)))
                else:
                    self.assertEqual(expected_attempts, 1)
                self.assertEqual(len(observed), 1)
                terminal = observed[0]
                self.assertEqual(terminal["success"], expect_success)
                if expect_success:
                    self.assertEqual(events[-1]["type"], "response.completed")
                    self.assertEqual(terminal["retry_count"], 1)
                else:
                    self.assertEqual(terminal["error_class"], expected_class)
                    self.assertEqual(
                        events[-1]["type"],
                        "response.incomplete"
                        if expected_class == "output_limit"
                        else "response.failed",
                    )
                    self.assertEqual(terminal["retry_count"], 0)
                self.assertNotIn("request-secret", json.dumps(events))

        for status in (404, 405, 415, 501):
            self.assertTrue(protocol_fallback_allowed(status))
        for status in (408, 429, 500, 502, 503, 504):
            self.assertFalse(protocol_fallback_allowed(status))
        self.assertFalse(protocol_fallback_allowed(404, output_emitted=True))
        self.assertFalse(protocol_fallback_allowed(404, terminal_event_observed=True))


if __name__ == "__main__":
    unittest.main()
