import unittest

from easy_multi_provider.performance import (
    ResponsesPerformanceTracker,
    declared_response_model,
    request_tokens_per_second,
)


class FakeClock:
    def __init__(self):
        self.value = 10.0

    def __call__(self):
        return self.value

    def advance(self, seconds):
        self.value += seconds


class ResponsesPerformanceTrackerTests(unittest.TestCase):
    def test_response_model_uses_only_protocol_fields(self):
        tracker = ResponsesPerformanceTracker()
        tracker.observe_upstream_event(
            {
                "type": "response.created",
                "response": {"model": "actual-upstream-v2", "output": "PRIVATE"},
            }
        )
        tracker.observe_upstream_event({"model": "later-model"})
        self.assertEqual(tracker.diagnostics()["response_model"], "actual-upstream-v2")
        self.assertEqual(
            tracker.diagnostics()["response_model_source"], "upstream_response"
        )
        self.assertNotIn("PRIVATE", repr(tracker.diagnostics()))
        self.assertEqual(
            declared_response_model(
                {"type": "message_start", "message": {"model": "claude-real"}}
            ),
            "claude-real",
        )
        self.assertIsNone(declared_response_model({"content": {"model": "hidden"}}))

    def test_canonical_events_measure_ttft_and_tps_without_content(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(started=clock(), clock=clock)
        clock.advance(0.2)
        tracker.mark_upstream_started()
        clock.advance(4.0)
        tracker.observe_event({"type": "response.created"})
        clock.advance(0.8)
        tracker.observe_event(
            {"type": "response.output_text.delta", "delta": "private output"}
        )
        clock.advance(2.0)
        tracker.observe_event({"type": "response.output_text.delta", "delta": "last"})
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {
                    "usage": {
                        "output_tokens": 111,
                        "output_tokens_details": {"reasoning_tokens": 0},
                    }
                },
            }
        )
        self.assertEqual(
            tracker.diagnostics(),
            {
                "performance_schema": 3,
                "ttft_ms": 5000,
                "upstream_first_token_ms": 4800,
                "output_tokens": 111,
                "reasoning_tokens": 0,
                "generation_ms": 2000,
                "tokens_per_second": 15.86,
            },
        )
        self.assertNotIn("private", repr(tracker.diagnostics()))

    def test_split_sse_frames_are_detected_without_json_buffering(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(started=clock(), clock=clock)
        clock.advance(1.0)
        tracker.observe_chunk(b'data: {"type":"response.output_text.')
        clock.advance(1.0)
        tracker.observe_chunk(b'delta","delta":"secret"}\n\n')
        clock.advance(4.0)
        tracker.observe_chunk(b'data: {"type":"response.output_text.delta","delta":"last"}\n\n')
        tracker.observe_chunk(
            b'data: {"type":"response.completed","response":{"usage":'
            b'{"output_tokens":201,"output_tokens_details":'
            b'{"reasoning_tokens":0}}}}\n\n'
        )
        self.assertEqual(tracker.diagnostics()["ttft_ms"], 2000)
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 33.5)

    def test_non_stream_usage_reports_request_throughput_without_ttft(self):
        tracker = ResponsesPerformanceTracker(started=0.0, clock=lambda: 10.0)
        tracker.observe_bytes(b'{"usage":{"output_tokens":42}}')
        self.assertEqual(
            tracker.diagnostics(),
            {
                "performance_schema": 3,
                "output_tokens": 42,
                "tokens_per_second": 4.2,
            },
        )

    def test_tps_includes_hidden_reasoning_tokens_and_time_like_omp(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(started=clock(), clock=clock)
        clock.advance(0.2)
        tracker.mark_upstream_started()
        clock.advance(40.0)
        tracker.observe_event(
            {"type": "response.reasoning_summary_text.delta", "delta": "summary"}
        )
        clock.advance(9.8)
        tracker.observe_event(
            {"type": "response.output_text.delta", "delta": "answer"}
        )
        clock.advance(2.0)
        tracker.observe_event({"type": "response.output_text.delta", "delta": "last"})
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {
                    "usage": {
                        "output_tokens": 1011,
                        "output_tokens_details": {"reasoning_tokens": 900},
                    }
                },
            }
        )
        self.assertEqual(
            tracker.diagnostics(),
            {
                "performance_schema": 3,
                "ttft_ms": 50000,
                "upstream_first_token_ms": 49800,
                "output_tokens": 1011,
                "reasoning_tokens": 900,
                "generation_ms": 2000,
                "tokens_per_second": 19.44,
            },
        )

    def test_tps_does_not_require_a_reasoning_breakdown(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(started=clock(), clock=clock)
        tracker.observe_event({"type": "response.output_text.delta", "delta": "answer"})
        clock.advance(1.0)
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {"usage": {"output_tokens": 100}},
            }
        )
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 100.0)

    def test_short_buffered_output_uses_full_request_duration(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(started=clock(), clock=clock)
        clock.advance(10.0)
        tracker.observe_event(
            {"type": "response.output_text.delta", "delta": "short answer"}
        )
        clock.advance(0.03)
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {
                    "usage": {
                        "output_tokens": 52,
                        "output_tokens_details": {"reasoning_tokens": 0},
                    }
                },
            }
        )
        diagnostics = tracker.diagnostics()
        self.assertEqual(diagnostics["ttft_ms"], 10000)
        self.assertEqual(diagnostics["generation_ms"], 0)
        self.assertEqual(diagnostics["tokens_per_second"], 5.18)

    def test_empty_delta_tool_shell_and_done_do_not_start_ttft(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(clock=clock)
        for event in (
            {"type": "response.output_text.delta", "delta": ""},
            {"type": "response.output_item.added", "item": {"type": "function_call"}},
            {"type": "response.output_text.done", "text": "buffered answer"},
        ):
            tracker.observe_event(event)
        self.assertNotIn("ttft_ms", tracker.diagnostics())
        clock.advance(3)
        tracker.observe_event({"type": "response.function_call_arguments.delta", "delta": "{}"})
        self.assertEqual(tracker.diagnostics()["ttft_ms"], 3000)

    def test_terminal_delay_is_part_of_complete_request_throughput(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(clock=clock)
        tracker.observe_event({"type": "response.output_text.delta", "delta": "first"})
        clock.advance(2)
        tracker.observe_event({"type": "response.output_text.delta", "delta": "last"})
        clock.advance(20)
        tracker.observe_event({"type": "response.completed", "response": {"usage": {
            "output_tokens": 101, "output_tokens_details": {"reasoning_tokens": 0}}}})
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 4.59)
        self.assertEqual(tracker.diagnostics()["generation_ms"], 2000)

    def test_usage_only_comes_from_terminal_response_not_embedded_output(self):
        import json
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(clock=clock)
        for event in (
            {"type": "response.output_text.delta", "delta": '"output_tokens":999999',
             "example": {"output_tokens": 999999, "reasoning_tokens": 0}},
            {"type": "response.output_text.delta", "delta": "end"},
            {"type": "response.completed", "response": {"usage": {"output_tokens": 10}}},
        ):
            clock.advance(1)
            raw = ('data: ' + json.dumps(event) + '\r\n\r\n').encode()
            for offset in range(0, len(raw), 7):
                tracker.observe_chunk(raw[offset:offset+7])
        self.assertEqual(tracker.diagnostics()["output_tokens"], 10)
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 3.33)

    def test_incomplete_response_with_reported_usage_is_measurable(self):
        clock = FakeClock()
        tracker = ResponsesPerformanceTracker(clock=clock)
        clock.advance(2)
        tracker.observe_event(
            {
                "type": "response.incomplete",
                "response": {"usage": {"output_tokens": 30}},
            }
        )
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 15.0)

    def test_omp_rate_rejects_missing_tokens_and_unstable_durations(self):
        self.assertEqual(request_tokens_per_second(120, 2000), 60.0)
        self.assertIsNone(request_tokens_per_second(0, 2000))
        self.assertIsNone(request_tokens_per_second(120, 99))
        self.assertIsNone(request_tokens_per_second(float("inf"), 2000))

    def test_large_measurement_frame_never_blocks_forwarding(self):
        chunks = [b'data: ' + b'x' * (1024 * 1024 + 1), b'\n\n']
        tracker = ResponsesPerformanceTracker()
        self.assertEqual(list(tracker.observe_stream(iter(chunks))), chunks)
        self.assertFalse(tracker._pending)
        self.assertNotIn("tokens_per_second", tracker.diagnostics())

    def test_invalid_usage_metadata_cannot_interrupt_forwarding(self):
        tracker = ResponsesPerformanceTracker()
        tracker.observe_event({"type": []})
        tracker.observe_event({"type": "response.completed", "response": {"usage": {
            "output_tokens": float('inf'), "output_tokens_details": {"reasoning_tokens": float('inf')}}}})
        self.assertNotIn('tokens_per_second', tracker.diagnostics())


if __name__ == "__main__":
    unittest.main()
