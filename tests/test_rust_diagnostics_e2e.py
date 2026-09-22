"""Unchanged diagnostic UI APIs against both running service implementations."""
import json
import os
from pathlib import Path
import shutil
import unittest
from datetime import datetime, timedelta, timezone

from easy_multi_provider.diagnostic_journal import DiagnosticJournal
from easy_multi_provider.server import ObservationRing
from tests import test_diagnostic_analytics as analytics_cases
from tests import test_rust_usage_e2e as usage_cases


@unittest.skipUnless(os.environ.get("EMP_RUST_BINARY"), "set EMP_RUST_BINARY for process E2E")
class RustDiagnosticsEndToEnd(unittest.TestCase):
    maxDiff = None
    setUp = usage_cases.RustUsageEndToEnd.setUp
    fixture = usage_cases.RustUsageEndToEnd.fixture
    start = usage_cases.RustUsageEndToEnd.start

    def test_cross_run_health_speed_and_cache_charts(self):
        source = self.root / "journal"
        journal = DiagnosticJournal(str(source), "0123456789abcdef")
        self.assertTrue(journal.open())
        ring = ObservationRing(sink=lambda fields: journal.event("info", "route_observation", **fields))
        reference = datetime.now(timezone.utc) - timedelta(days=1)
        for index in range(45):
            status, error = (200, "none") if index < 40 else [(429, "rate_limit"), (502, "tls_failure"),
                (503, "upstream_5xx"), (None, "client_disconnect"), (502, "tls_failure")][index - 40]
            item = analytics_cases.DiagnosticAnalyticsTest.record("model-a", "standard", status, 6000 if index < 20 else 4000,
                50 if index < 20 else 75, error_class=error, observed_at=(reference + timedelta(minutes=index)).isoformat())
            item.update(observation_id="fixture-%02d" % index, provider_id="external", input_tokens=1000,
                        cached_input_tokens=800 if index % 2 else 0, output_tokens=100,
                        context_observation={"decision": "allowed", "confidence": 1, "input_estimate": 1000},
                        thread_ref="persisted-thread-ref", protocol="responses", dialect="portable_responses")
            if index == 44:
                item["recovery_mode"] = "native_http_fallback"
            ring.record(item)
        journal.close()
        outcomes = []
        for name in ("python", "rust"):
            root, home = self.fixture(name)
            shutil.copytree(source, root / "state/logs")
            backend = self.start(name, root, home)
            status, _, raw = backend.request("GET", "/api/diagnostics")
            self.assertEqual(status, 200, raw)
            before = json.loads(raw)
            self.assertEqual(before["sample_count"], 45)
            self.assertEqual(before["health"]["sample_count"], 43)
            self.assertEqual(before["models"][0]["ttft_change_percent"], 33.3)
            self.assertEqual(before["cache"]["models"][0]["sample_count"], 44)
            backend.close()
            backend = self.start(name, root, home)
            self.assertEqual(before, json.loads(backend.request("GET", "/api/diagnostics")[2]))
            outcomes.append(before)
        self.assertEqual(outcomes[0], outcomes[1])

    def test_client_event_validation_and_content_free_journal(self):
        outcomes = []
        fixtures = [
            {"kind": "integration_load", "phase": "render_completed", "duration_ms": 125,
             "page_id": "page-123", "prompt": "PRIVATE PROMPT", "headers": {"Authorization": "Bearer PRIVATEKEY"}},
            {"kind": "window_error", "phase": "failed", "failure_class": "type_error", "http_status": 503,
             "page_id": "sk-0123456789abcdef0123456789", "line": "12", "column": 4.5,
             "body": "PRIVATE RESPONSE"},
            {"kind": "unhandled_rejection", "phase": "failed", "failure_class": "PRIVATE FAILURE"},
            {"kind": "bad", "phase": "failed"},
            {"kind": "window_error", "phase": "bad"},
        ]
        for name in ("python", "rust"):
            root, home = self.fixture(name)
            backend = self.start(name, root, home)
            result = []
            for fixture in fixtures:
                status, _, raw = backend.request("POST", "/api/client-events", fixture)
                result.append((status, json.loads(raw)))
            self.assertEqual([status for status, _ in result], [200, 200, 200, 400, 400])
            self.assertEqual(backend.request("POST", "/api/client-events", fixtures[0], auth=False)[0], 401)
            records = []
            for path in sorted((root / "state/logs").glob("*.jsonl")):
                data = path.read_text()
                self.assertNotIn("PRIVATE", data)
                self.assertNotIn("0123456789abcdef0123456789", data)
                if os.name != "nt":
                    self.assertEqual(path.stat().st_mode & 0o777, 0o600)
                for line in data.splitlines():
                    entry = json.loads(line)
                    if entry["event"] == "web_client_phase":
                        records.append((entry["level"], entry["fields"]))
            self.assertEqual(len(records), 3)
            outcomes.append((result, records))
        self.assertEqual(outcomes[0], outcomes[1])

    def test_real_generation_and_rejection_reach_health_charts(self):
        outcomes = []
        for name in ("python", "rust"):
            root, home = self.fixture(name)
            backend = self.start(name, root, home)
            attempts = []
            for mode in ("external", "native"):
                self.upstream.configure({"id": "resp-" + mode, "object": "response", "status": "completed",
                    "model": "upstream-model", "output": [], "usage": {"input_tokens": 1000,
                        "output_tokens": 100, "input_tokens_details": {"cached_tokens": 800},
                        "output_tokens_details": {"reasoning_tokens": 60}}})
                self.assertEqual(backend.request("POST", "/v1/responses", {
                    "model": mode + "/alias", "input": "PRIVATE PROMPT", "stream": False})[0], 200)
                self.upstream.requests.get(timeout=5)
                self.upstream.configure({"error": {"message": "PRIVATE UPSTREAM ERROR", "type": "rate_limit_exceeded"}}, 429)
                self.assertEqual(backend.request("POST", "/v1/responses", {
                    "model": mode + "/alias", "input": "PRIVATE PROMPT", "stream": False})[0], 429)
                attempts.append([])
                while not self.upstream.requests.empty():
                    attempts[-1].append(self.upstream.requests.get(timeout=5)[0])
            status, _, raw = backend.request("GET", "/api/diagnostics")
            self.assertEqual(status, 200, raw)
            observed = json.loads(raw)
            self.assertEqual(observed["sample_count"], 4, observed)
            self.assertEqual(observed["health"]["status_429_count"], 2, observed)
            self.assertNotIn(b"PRIVATE", raw)
            records = []
            for row in observed["records"]:
                # Wall-clock durations are observations, not fixed fixture outputs.
                self.assertGreaterEqual(row["duration_ms"], 0)
                self.assertLess(row["duration_ms"], 8000)
                records.append({key: row[key] for key in ("route", "provider_id", "model_id", "upstream_model",
                    "protocol", "dialect", "transport", "status", "error_class", "input_tokens",
                    "cached_input_tokens", "output_tokens", "speed_mode", "request_item_count",
                    "request_item_types", "content_part_types", "tool_pairing_status")})
            outcomes.append((observed["health"], records, attempts))
            for path in (root / "state/logs").glob("*.jsonl"):
                self.assertNotIn("PRIVATE", path.read_text())
        self.assertEqual(outcomes[0], outcomes[1])


if __name__ == "__main__":
    unittest.main()
