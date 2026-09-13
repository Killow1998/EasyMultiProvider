"""Reported cache usage survives transport, projection, persistence and aggregation."""

import json
import tempfile
import unittest
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.config import normalize, save
from easy_multi_provider.diagnostic_analytics import summarize_route_observations
from easy_multi_provider.diagnostic_journal import create_journal, read_route_observations
from easy_multi_provider.performance import ResponsesPerformanceTracker
from easy_multi_provider.protocol_projection import _response_from_chat, _response_from_anthropic
from easy_multi_provider.server import AppState, ObservationRing
from easy_multi_provider.transport import sse_json_events


class CacheUsageTests(unittest.TestCase):
    NOW = datetime(2026, 9, 12, 12, 5, tzinfo=timezone.utc)

    @staticmethod
    def record(total=1000, cached=800, at="2026-09-12T11:01:00Z", **extra):
        return {"route": "responses", "model_id": "model", "provider_id": "provider",
                "speed_mode": "standard", "observed_at": at, "status": 200,
                "error_class": "none", "input_tokens": total, "cached_input_tokens": cached,
                **extra}

    def cache(self, records):
        return summarize_route_observations(records, now=self.NOW)["cache"]["models"]

    def test_weighted_rate_excludes_unknowns_and_includes_explicit_zero(self):
        records = [self.record(1000, 0), self.record(9000, 9000), self.record(100000, None)]
        model = self.cache(records)[0]
        self.assertEqual(model["rate"], 90.0)  # Arithmetic mean of request rates would be 50%.
        self.assertEqual(model["input_tokens"], 10000)
        self.assertEqual(model["sample_count"], 2)
        self.assertEqual(model["call_count"], 3)
        self.assertEqual(model["hit_count"], 1)
        self.assertEqual(model["periods"][0]["rate"], 90.0)

    def test_period_boundaries_use_absolute_time_and_skip_idle_buckets(self):
        records = [self.record(at="2026-09-12T04:09:59-07:00"),
                   self.record(at="2026-09-12T11:10:00Z"),
                   self.record(at="2026-09-12T12:01:00Z")]
        periods = self.cache(records)[0]["periods"]
        self.assertEqual(len(periods), 3)
        self.assertEqual([datetime.fromtimestamp(p["start"], timezone.utc).strftime("%H:%M")
                          for p in periods], ["12:00", "11:10", "11:00"])
        self.assertEqual([p["complete"] for p in periods], [False, True, True])
        self.assertTrue(all(p["end"] - p["start"] == 600 for p in periods))
        self.assertEqual(self.cache([]), [])

    def test_models_providers_modes_and_changed_endpoints_are_separate(self):
        rows = [self.record(), self.record(provider_id="another"), self.record(speed_mode="fast"),
                self.record(model_id="gemini"), self.record(endpoint_fingerprint="changed")]
        self.assertEqual(len(self.cache(rows)), 5)
        self.assertTrue(all(model["call_count"] == 1 for model in self.cache(rows)))

    def test_incomplete_calls_with_usage_count_but_invalid_or_missing_counts_do_not(self):
        rows = [self.record(1000, 300, status=502, error_class="stream_error")]
        for total, cached in [(0, 0), (1000, None), (-1, 0), (1000, 1001), (True, 0),
                              (1000, False), (1000.5, 10), (1000, "10"), (float("inf"), 1)]:
            rows.append(self.record(total, cached))
        model = self.cache(rows)[0]
        self.assertEqual(model["sample_count"], 1)
        self.assertEqual(model["rate"], 30)
        self.assertIsNone(self.cache([self.record(cached=None)])[0]["rate"])

    def test_old_invalid_future_and_internal_records_are_not_current_cache_samples(self):
        rows = [self.record(at="2026-09-01T10:00:00Z"), self.record(at="invalid"),
                self.record(at="2026-09-13T10:00:00Z"), self.record(model_id="codex-auto-review"),
                self.record(route="compact")]
        self.assertEqual(self.cache(rows), [])

    def test_stream_counts_only_terminal_usage_and_does_not_modify_wire(self):
        chunks = [b'data: {"type":"response.output_text.delta","delta":"private cached_tokens:999"}\n\n',
                  b'data: {"type":"response.completed","response":{"usage":{"input_tokens":1000,',
                  b'"input_tokens_details":{"cached_tokens":800}}}}\n\n']
        tracker = ResponsesPerformanceTracker()
        self.assertEqual(list(tracker.observe_stream(iter(chunks))), chunks)
        tracker.observe_event({"type": "response.completed", "response": {"usage": {"input_tokens": 1}}})
        self.assertEqual(tracker.diagnostics()["cached_input_tokens"], 800)
        self.assertEqual(tracker.diagnostics()["input_tokens"], 1000)
        self.assertNotIn("private", repr(tracker.diagnostics()))

    def test_nonstream_observation_keeps_cache_counts_before_dispatch_returns(self):
        events = []
        payload = {"status": "completed", "output": [], "usage": {
            "input_tokens": 400, "input_tokens_details": {"cached_tokens": 320}}}
        router._finish_nonstream(events.append, {"status": 200, "dialect": "portable_responses"},
                                 {"id": "external", "base_url": "https://example.invalid"},
                                 {"id": "external/model"}, json.dumps(payload).encode())
        self.assertEqual(len(events), 1)
        self.assertEqual(events[0]["cached_input_tokens"], 320)
        self.assertEqual(events[0]["input_tokens"], 400)

    def test_tracker_does_not_turn_missing_or_invalid_cache_usage_into_zero(self):
        for usage in ({}, {"input_tokens": 1000}, {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": None}},
                      {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 1001}},
                      {"input_tokens": True, "input_tokens_details": {"cached_tokens": 0}}):
            tracker = ResponsesPerformanceTracker()
            tracker.observe_bytes(json.dumps({"usage": usage}).encode())
            self.assertNotIn("cached_input_tokens", tracker.diagnostics())

    def test_standard_chat_and_deepseek_usage_are_supported_without_provider_names(self):
        for usage in (
            {"prompt_tokens": 1000, "prompt_tokens_details": {"cached_tokens": 800}},
            {"prompt_tokens": 1000, "prompt_cache_hit_tokens": 800, "prompt_cache_miss_tokens": 200},
        ):
            response = _response_from_chat({"choices": [{"message": {"content": "OK"}, "finish_reason": "stop"}],
                                            "usage": usage}, "external/model")
            tracker = ResponsesPerformanceTracker()
            tracker.observe_bytes(json.dumps(response).encode())
            self.assertEqual(tracker.diagnostics()["input_tokens"], 1000)
            self.assertEqual(tracker.diagnostics()["cached_input_tokens"], 800)

    def test_anthropic_counts_include_cache_reads_and_writes_in_denominator(self):
        response = _response_from_anthropic({"content": [{"type": "text", "text": "OK"}],
            "stop_reason": "end_turn", "usage": {"input_tokens": 100, "cache_read_input_tokens": 800,
                "cache_creation_input_tokens": 100, "output_tokens": 10}}, "external/model")
        self.assertEqual(response["usage"], {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 800, "cache_creation_tokens": 100},
                                            "output_tokens": 10, "total_tokens": 1010})

    def test_anthropic_stream_retains_start_usage_and_cumulative_output_delta(self):
        events = [
            {"type": "message_start", "message": {"usage": {"input_tokens": 100,
                "cache_read_input_tokens": 800, "cache_creation_input_tokens": 100, "output_tokens": 1}}},
            {"type": "content_block_delta", "delta": {"type": "text_delta", "text": "OK"}},
            {"type": "message_delta", "delta": {"stop_reason": "end_turn"}, "usage": {"output_tokens": 10}},
            {"type": "message_stop"},
        ]
        class Upstream:
            def __iter__(self):
                return iter(("".join("data: " + json.dumps(event) + "\n\n" for event in events)).encode().splitlines(keepends=True))

            def close(self):
                pass

        tracker = ResponsesPerformanceTracker()
        with patch.object(router, "_request", return_value=Upstream()):
            wire = list(tracker.observe_stream(router.stream_anthropic_completion(
                {"id": "external", "protocol": "anthropic_messages", "base_url": "https://example.invalid/v1"},
                {"model": "external/model", "input": "hello", "stream": True}, {"id": "external/model"}, {})))
        terminal = list(sse_json_events(wire))[-1]
        self.assertEqual(terminal["type"], "response.completed")
        self.assertEqual(terminal["response"]["usage"]["output_tokens"], 10)
        self.assertEqual(tracker.diagnostics()["cached_input_tokens"], 800)
        self.assertEqual(tracker.diagnostics()["input_tokens"], 1000)

    def test_numeric_cache_history_survives_restart_without_conversation_content(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            journal = create_journal(root)
            ring = ObservationRing(sink=lambda record: journal.event("info", "route_observation", **record))
            ring.record(self.record(prompt="private prompt", api_key="secret credential"))
            journal.close()
            restored = ObservationRing()
            for record in read_route_observations(root):
                restored.record(record)
            records = restored.snapshot()["records"]
            self.assertEqual(len(records), 1)
            self.assertEqual(self.cache(records)[0]["rate"], 80)
            self.assertNotIn("private prompt", repr(records))
            self.assertNotIn("secret credential", repr(records))

    def test_dispatch_and_restart_count_each_external_request_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "config.json"
            save(normalize({"native_catalog_path": str(root / "native.json"),
                "providers": [{"id": "external", "protocol": "responses", "base_url": "https://example.invalid/v1"}],
                "models": [{"id": "external/model", "provider": "external", "upstream_id": "model", "enabled": True}]}), path)
            state = AppState(path, runtime_controller=object())
            state.journal = create_journal(root)
            response = {"id": "resp_test", "object": "response", "status": "completed", "output": [],
                        "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 800}}}
            raw = json.dumps(response).encode()
            try:
                with patch.object(router, "forward_responses", return_value=(200, "application/json", raw)):
                    _, received = state.codex.route({"model": "external/model", "input": "hello"}, {})
                self.assertEqual(received, raw)
                snapshot = state.diagnostics_snapshot()
                self.assertEqual(snapshot["sample_count"], 1)
                self.assertEqual(snapshot["cache"]["models"][0]["rate"], 80)
            finally:
                state.journal.close()
            restarted = AppState(path, runtime_controller=object())
            snapshot = restarted.diagnostics_snapshot()
            self.assertEqual(snapshot["sample_count"], 1)
            self.assertEqual(snapshot["cache"]["models"][0]["rate"], 80)


if __name__ == "__main__":
    unittest.main()
