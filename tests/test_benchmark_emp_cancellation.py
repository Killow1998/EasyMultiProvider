import importlib.util
import json
from pathlib import Path
import socket
import sys
import unittest


SCRIPT = Path(__file__).resolve().parents[1] / "tools" / "benchmark_emp_cancellation.py"
SPEC = importlib.util.spec_from_file_location("benchmark_emp_cancellation", SCRIPT)
cancellation = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = cancellation
SPEC.loader.exec_module(cancellation)


class BenchmarkCancellationTests(unittest.TestCase):
    def test_fake_upstream_observes_client_eof_at_all_disconnect_phases(self):
        upstream = cancellation.CancellationFakeUpstream()
        try:
            for index, scenario in enumerate(cancellation.SCENARIOS):
                marker = f"fake-upstream-phase-{index}"
                plan = upstream.begin(scenario, marker)
                payload = json.dumps({
                    "model": cancellation.benchmark.UPSTREAM_MODEL,
                    "input": marker, "stream": True,
                }, separators=(",", ":")).encode()
                connection = socket.create_connection(
                    ("127.0.0.1", upstream.server_port), timeout=2,
                )
                connection.settimeout(2)
                connection.sendall(
                    b"POST /v1/responses HTTP/1.1\r\n"
                    + f"Host: 127.0.0.1:{upstream.server_port}\r\n".encode()
                    + b"Authorization: Bearer "
                    + cancellation.benchmark.FIXTURE_PROVIDER_KEY.encode()
                    + b"\r\nContent-Type: application/json\r\nConnection: close\r\n"
                    + f"Content-Length: {len(payload)}\r\n\r\n".encode()
                    + payload
                )
                reader = connection.makefile("rb")
                if scenario == "before_headers":
                    self.assertTrue(plan.request_seen.wait(2))
                else:
                    response_head = []
                    while True:
                        line = reader.readline()
                        response_head.append(line)
                        if line in (b"\r\n", b"\n", b""):
                            break
                    self.assertIn(b"HTTP/1.1 200", response_head[0])
                    if scenario == "before_first_event":
                        self.assertTrue(plan.headers_ready.wait(2))
                    else:
                        event_lines = []
                        observed_delta = False
                        while not observed_delta:
                            line = reader.readline()
                            if not line:
                                break
                            if line in (b"\r\n", b"\n"):
                                data = next((entry[5:].lstrip() for entry in event_lines
                                             if entry.startswith(b"data:")), None)
                                event_lines = []
                                if data is not None:
                                    observed_delta = (
                                        json.loads(data).get("type") == "response.output_text.delta"
                                    )
                            else:
                                event_lines.append(line.rstrip(b"\r\n"))
                        self.assertTrue(observed_delta)
                        self.assertTrue(plan.delta_ready.wait(2))
                reader.close()
                connection.close()
                self.assertTrue(plan.peer_closed_event.wait(2), plan.snapshot())
                self.assertEqual(plan.snapshot()["requests"], 1)
                self.assertFalse(plan.snapshot()["unexpected_request_shape"])
        finally:
            upstream.close()

    def test_sse_fixture_contains_only_prefix_through_visible_delta(self):
        events = cancellation._sse_prefix_events()
        types = [kind for kind, _wire in events]
        self.assertEqual(types[-1], "response.output_text.delta")
        self.assertEqual(types, [
            "response.created",
            "response.output_item.added",
            "response.content_part.added",
            "response.output_text.delta",
        ])
        self.assertNotIn("response.completed", types)
        self.assertTrue(all(wire.endswith(b"\n\n") for _kind, wire in events))
        self.assertNotIn(cancellation.REPLY_TEXT.encode(), repr(types).encode())

    def test_phase_observations_cover_headers_first_event_and_visible_delta(self):
        before_headers = {
            "status": None, "content_type": None, "event_types": [],
            "output_delta_seen": False,
        }
        before_event = {
            "status": None, "content_type": None, "event_types": [],
            "output_delta_seen": False,
        }
        after_delta = {
            "status": 200, "content_type": "text/event-stream",
            "event_types": [
                "response.created", "response.output_item.added",
                "response.content_part.added", "response.output_text.delta",
            ],
            "output_delta_seen": True,
        }
        self.assertTrue(cancellation.phase_observed("before_headers", before_headers, False))
        self.assertTrue(cancellation.phase_observed("before_first_event", before_event, True))
        self.assertTrue(cancellation.phase_observed("after_delta", after_delta, True))
        self.assertFalse(cancellation.phase_observed("after_delta", before_event, True))
        self.assertFalse(cancellation.phase_observed("unknown", after_delta, True))

    def test_resource_release_requires_all_three_process_metrics(self):
        baseline = {"rss_bytes": 100, "threads": 4, "descriptors": 8}
        self.assertTrue(cancellation.resource_release_ok(
            baseline, {"rss_bytes": 100 + cancellation.RSS_SLACK_BYTES,
                       "threads": 6, "descriptors": 12}
        ))
        self.assertFalse(cancellation.resource_release_ok(
            baseline, {"rss_bytes": 100, "threads": 7, "descriptors": 8}
        ))
        self.assertFalse(cancellation.resource_release_ok(
            baseline, {"rss_bytes": 100, "threads": 4, "descriptors": 13}
        ))
        self.assertFalse(cancellation.resource_release_ok(
            baseline, {"rss_bytes": None, "threads": 4, "descriptors": 8}
        ))

    def test_limit_snapshot_semantic_keeps_only_status_and_reserved_count(self):
        result = {
            "status": 200,
            "body": json.dumps({
                "reserved_bytes": 0,
                "limits": {"max_request_bytes": 999},
                "private": "PRIVATE_REQUEST_BODY_SENTINEL",
            }).encode(),
        }
        semantic = cancellation._management_semantic(result)
        self.assertEqual(semantic, {"status": 200, "reserved_bytes": 0})
        self.assertNotIn("PRIVATE_REQUEST_BODY_SENTINEL", json.dumps(semantic))

    def test_percentile_report_has_three_percentiles_and_no_payload_fields(self):
        summary = cancellation.percentile_report([1.0, 2.0, 3.0, 4.0])
        self.assertEqual(summary["count"], 4)
        self.assertEqual(summary["p50_ms"], 2.5)
        self.assertAlmostEqual(summary["p95_ms"], 3.85)
        self.assertAlmostEqual(summary["p99_ms"], 3.97)
        self.assertNotIn("body", summary)
        self.assertNotIn("headers", summary)

    def test_upstream_observation_hides_correlation_marker(self):
        marker = "PRIVATE_REQUEST_BODY_SENTINEL"
        plan = cancellation.UpstreamObservation("after_delta", marker)
        rendered = json.dumps(plan.snapshot())
        self.assertNotIn(marker, rendered)
        self.assertEqual(plan.snapshot()["requests"], 0)

    def test_validation_rejects_unbounded_runs_and_timeout(self):
        cancellation.validate_parameters(5, 1, 5.0)
        for values in ((0, 0, 5.0), (1001, 0, 5.0), (1, -1, 5.0), (1, 0, 5.1)):
            with self.subTest(values=values), self.assertRaises(
                cancellation.benchmark.BenchmarkError
            ):
                cancellation.validate_parameters(*values)

    def test_python_cancellation_baseline_is_reference_only_but_other_failures_gate(self):
        fatal, reference = cancellation.classify_runtime_errors(
            "python", "after_delta_", [
                "upstream_socket_not_closed",
                "all_release_conditions_not_observed_within_timeout",
                "request_reservation_not_released",
            ],
        )
        self.assertEqual(fatal, ["after_delta_request_reservation_not_released"])
        self.assertEqual(reference, [
            "after_delta_upstream_socket_not_closed",
            "after_delta_all_release_conditions_not_observed_within_timeout",
        ])
        rust_fatal, rust_reference = cancellation.classify_runtime_errors(
            "rust", "before_headers_", ["upstream_socket_not_closed"]
        )
        self.assertEqual(rust_fatal, ["before_headers_upstream_socket_not_closed"])
        self.assertEqual(rust_reference, [])

    def test_report_summary_contains_counts_and_only_safe_outcomes(self):
        result = {
            "disconnect_to_release_ms": 2.0,
            "upstream": {"requests": 1, "rejected_requests": 0, "peer_closed": True},
            "request_limits": {"reserved_after": 0},
            "resources": {"returned_near_baseline": True},
            "release_conditions_within_timeout": {
                "upstream_socket_closed": True,
                "request_reservation_returned": True,
                "process_resources_returned": True,
            },
            "observation": {
                "status": 200, "content_type": "text/event-stream",
                "event_types": ["response.output_text.delta"],
                "output_delta_seen": True,
            },
            "errors": [],
        }
        summary = cancellation._summarize_scenario([result, result])
        self.assertEqual(summary["iterations"], 2)
        self.assertEqual(summary["upstream_requests"], 2)
        self.assertEqual(summary["upstream_retries"], 0)
        self.assertEqual(summary["request_reservation_zero_count"], 2)
        self.assertEqual(
            summary["release_condition_counts"], {
                "upstream_socket_closed": 2,
                "request_reservation_returned": 2,
                "process_resources_returned": 2,
            },
        )
        self.assertAlmostEqual(summary["release_latency"]["p95_ms"], 2.0)
        serialized = json.dumps(summary)
        self.assertNotIn(cancellation.benchmark.FIXTURE_CALLER_KEY, serialized)
        self.assertNotIn(cancellation.benchmark.FIXTURE_PROVIDER_KEY, serialized)
        self.assertNotIn(cancellation.REPLY_TEXT, serialized)


if __name__ == "__main__":
    unittest.main()
