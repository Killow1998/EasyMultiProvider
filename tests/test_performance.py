import unittest

from easy_multi_provider.performance import ResponsesPerformanceTracker


class FakeClock:
    def __init__(self):
        self.value = 10.0

    def __call__(self):
        return self.value

    def advance(self, seconds):
        self.value += seconds


class ResponsesPerformanceTrackerTests(unittest.TestCase):
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
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {
                    "usage": {
                        "output_tokens": 110,
                        "output_tokens_details": {"reasoning_tokens": 0},
                    }
                },
            }
        )
        self.assertEqual(
            tracker.diagnostics(),
            {
                "ttft_ms": 5000,
                "upstream_first_token_ms": 4800,
                "output_tokens": 110,
                "generation_ms": 2000,
                "tokens_per_second": 55.0,
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
        tracker.observe_chunk(
            b'data: {"type":"response.completed","response":{"usage":'
            b'{"output_tokens":200,"output_tokens_details":'
            b'{"reasoning_tokens":0}}}}\n\n'
        )
        self.assertEqual(tracker.diagnostics()["ttft_ms"], 2000)
        self.assertEqual(tracker.diagnostics()["tokens_per_second"], 50.0)

    def test_non_stream_usage_does_not_claim_ttft_or_tps(self):
        tracker = ResponsesPerformanceTracker(started=0.0, clock=lambda: 10.0)
        tracker.observe_bytes(b'{"usage":{"output_tokens":42}}')
        self.assertEqual(tracker.diagnostics(), {"output_tokens": 42})

    def test_tps_excludes_reasoning_tokens_before_visible_output(self):
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
        tracker.observe_event(
            {
                "type": "response.completed",
                "response": {
                    "usage": {
                        "output_tokens": 1010,
                        "output_tokens_details": {"reasoning_tokens": 900},
                    }
                },
            }
        )
        self.assertEqual(
            tracker.diagnostics(),
            {
                "ttft_ms": 50000,
                "upstream_first_token_ms": 49800,
                "output_tokens": 1010,
                "generation_ms": 2000,
                "tokens_per_second": 55.0,
            },
        )

    def test_missing_reasoning_breakdown_does_not_guess_tps(self):
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
        self.assertNotIn("tokens_per_second", tracker.diagnostics())

    def test_short_buffered_output_burst_does_not_claim_tps(self):
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
        self.assertEqual(diagnostics["generation_ms"], 30)
        self.assertNotIn("tokens_per_second", diagnostics)


if __name__ == "__main__":
    unittest.main()
