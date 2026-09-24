import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "tools" / "benchmark_emp_stream_delay.py"
SPEC = importlib.util.spec_from_file_location("benchmark_emp_stream_delay", SCRIPT)
stream_delay = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stream_delay)


class BenchmarkStreamDelayTests(unittest.TestCase):
    def test_schedule_covers_reasoning_tool_text_and_terminal_in_order(self):
        events = stream_delay.scheduled_events()
        self.assertEqual(
            [event["sequence_number"] for event in events],
            list(range(len(events))),
        )
        types = [event["type"] for event in events]
        self.assertIn("response.reasoning_summary_text.delta", types)
        self.assertIn("response.function_call_arguments.delta", types)
        self.assertIn("response.output_text.delta", types)
        self.assertEqual(types[-1], "response.completed")
        self.assertEqual((len(events) - 1) * stream_delay.EVENT_SPACING_MS, 72)

    def test_semantic_check_detects_missing_reordered_or_corrupt_events(self):
        events = stream_delay.scheduled_events()
        valid = stream_delay._event_semantics(events)
        self.assertTrue(valid["ordered"])
        self.assertTrue(valid["reasoning_present"])
        self.assertTrue(valid["text_present"])
        self.assertTrue(valid["tool_arguments_present"])
        self.assertTrue(valid["completed"])
        self.assertTrue(valid["output_pair_present"])

        reordered = [events[1], events[0], *events[2:]]
        self.assertFalse(stream_delay._event_semantics(reordered)["ordered"])
        corrupt = [dict(event) for event in events]
        corrupt[8]["delta"] = "different"
        self.assertFalse(stream_delay._event_semantics(corrupt)["text_present"])
        self.assertFalse(stream_delay._event_semantics(events[:-1])["completed"])

    def test_latency_summary_has_percentiles_without_event_content(self):
        summary = stream_delay._latency_summary([1.0, 2.0, 3.0, 4.0])
        self.assertEqual(summary["count"], 4)
        self.assertAlmostEqual(summary["p50_ms"], 2.5)
        self.assertAlmostEqual(summary["p95_ms"], 3.85)
        self.assertAlmostEqual(summary["p99_ms"], 3.97)
        self.assertNotIn(stream_delay.TEXT_SENTINEL, repr(summary))
        self.assertNotIn(stream_delay.TOOL_ARGUMENTS, repr(summary))

    def test_event_timing_requires_send_timestamp_for_every_observed_frame(self):
        events = stream_delay.scheduled_events()
        due = {index: 1_000_000_000 + index * 6_000_000
               for index in range(len(events))}
        sent = {index: due[index] + 5_000
                for index in range(len(events))}
        observed = [sent[index] + 2_000 for index in range(len(events))]
        timings = stream_delay._event_timings(events, observed, {
            "due_times": due, "send_times": sent,
        })
        self.assertEqual(len(timings), len(events))
        self.assertAlmostEqual(timings[0]["added_delay_ms"], 0.007)
        self.assertAlmostEqual(timings[0]["upstream_send_to_downstream_ms"], 0.002)
        self.assertAlmostEqual(timings[0]["schedule_lateness_ms"], 0.005)

        del sent[4]
        self.assertEqual(
            len(stream_delay._event_timings(events, observed, {
                "due_times": due, "send_times": sent,
            })),
            len(events) - 1,
        )

    def test_parameter_validation_rejects_invalid_counts_and_limits(self):
        stream_delay.validate_parameters(1, 0, 5.0)
        for values in ((0, 0, 5.0), (1, -1, 5.0), (1, 0, 0.0)):
            with self.subTest(values=values), self.assertRaises(
                stream_delay.benchmark.BenchmarkError
            ):
                stream_delay.validate_parameters(*values)


if __name__ == "__main__":
    unittest.main()
