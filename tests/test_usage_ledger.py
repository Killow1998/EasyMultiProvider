"""Accounting regressions: billing subsets, routing, persistence and price outages."""

import io
import json
import sqlite3
import tempfile
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from contextlib import closing
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path
from types import SimpleNamespace
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.config import normalize, save
from easy_multi_provider.diagnostic_journal import NullJournal
from easy_multi_provider.performance import ResponsesPerformanceTracker, reported_usage
from easy_multi_provider.protocol_projection import _anthropic_usage, _chat_usage
from easy_multi_provider.server import AppState, make_handler
from easy_multi_provider.usage_ledger import UsageLedger, usage_identity
from easy_multi_provider.usage_pricing import PriceCatalog, estimate_tokens, model_price_key, normalize_prices
from tests.support import ensure_test_master_key

ensure_test_master_key()

RATES = normalize_prices({"model": {
    "input_cost_per_token": 0.000002, "output_cost_per_token": 0.000008,
    "cache_read_input_token_cost": 0.0000002, "cache_creation_input_token_cost": 0.0000025,
    "cache_creation_input_token_cost_above_1hr": 0.000004,
    "input_cost_per_token_above_200k_tokens": 0.000004,
    "output_cost_per_token_above_200k_tokens": 0.000012,
    "cache_read_input_token_cost_above_200k_tokens": 0.0000004,
    "input_cost_per_token_priority": 0.000004, "output_cost_per_token_priority": 0.000016,
    "cache_read_input_token_cost_priority": 0.0000004,
    "input_cost_per_token_above_200k_tokens_priority": 0.000008,
    "output_cost_per_token_above_200k_tokens_priority": 0.000024,
    "cache_read_input_token_cost_above_200k_tokens_priority": 0.0000008,
}})["model"]


def event(**extra):
    return {"route": "responses", "usage_category": "native", "usage_owner": "codex-native",
            "upstream_model": "gpt-example", "input_tokens": 1000, "cached_input_tokens": 800,
            "output_tokens": 100, "reasoning_tokens": 60, **extra}


class PriceCalculationTests(unittest.TestCase):
    def test_cache_and_reasoning_are_subsets_not_additional_billable_tokens(self):
        self.assertEqual(estimate_tokens(event(), RATES)[0], 1_360_000)
        self.assertEqual(estimate_tokens(event(), RATES, "priority")[0], 2_720_000)

    def test_cache_creation_ttls_are_priced_separately(self):
        usage = _anthropic_usage({"input_tokens": 100, "output_tokens": 100,
            "cache_read_input_tokens": 800, "cache_creation_input_tokens": 100,
            "cache_creation": {"ephemeral_5m_input_tokens": 60, "ephemeral_1h_input_tokens": 40}})
        normalized = reported_usage({"usage": usage})
        self.assertEqual(normalized["input_tokens"], 1000)
        self.assertEqual(normalized["cache_write_1h_tokens"], 40)
        self.assertEqual(estimate_tokens(normalized, RATES)[0], 1_470_000)

    def test_long_context_tier_applies_to_entire_request_including_cached_input(self):
        usage = event(input_tokens=201000, cached_input_tokens=1000, output_tokens=10, reasoning_tokens=0)
        self.assertEqual(estimate_tokens(usage, RATES)[0], 800_520_000)
        self.assertEqual(estimate_tokens(usage, RATES, "fast")[0], 1_601_040_000)
        self.assertEqual(estimate_tokens(event(input_tokens=200000, cached_input_tokens=0, output_tokens=10,
                                              reasoning_tokens=0), RATES)[0], 400_080_000)

    def test_unknown_rates_missing_usage_and_malformed_subsets_are_not_free(self):
        for usage in (event(input_tokens=None), event(cached_input_tokens=None), event(output_tokens=True),
                      event(cached_input_tokens=1001), event(reasoning_tokens=101),
                      event(cache_write_tokens=None), event(cache_write_tokens=500)):
            self.assertIsNone(estimate_tokens(usage, RATES)[0])
        self.assertIsNone(estimate_tokens(event(), RATES, "ultrafast")[0])
        self.assertIsNone(estimate_tokens(event(), RATES, "flex")[0])
        self.assertEqual(estimate_tokens(event(input_tokens=0, cached_input_tokens=0, output_tokens=0,
                                              reasoning_tokens=0), RATES)[0], 0)

    def test_components_use_their_own_context_thresholds(self):
        rates = {"input_cost_per_token": "0.000002", "output_cost_per_token": "0.000008",
                 "input_cost_per_token_above_128k_tokens": "0.000004",
                 "output_cost_per_token_above_200k_tokens": "0.000012"}
        usage = event(input_tokens=201000, cached_input_tokens=0, output_tokens=100, reasoning_tokens=0)
        self.assertEqual(estimate_tokens(usage, rates)[0], 805_200_000)

    def test_distinct_reasoning_rate_requires_a_reported_breakdown(self):
        rates = {**RATES, "output_cost_per_reasoning_token": "0.000016"}
        self.assertEqual(estimate_tokens(event(), rates)[0], 1_840_000)
        usage = event()
        usage.pop("reasoning_tokens")
        self.assertEqual(estimate_tokens(usage, rates)[1], "missing_reasoning_usage")

    def test_catalog_validation_and_exact_model_matching(self):
        for value in (-1, float("nan"), float("inf"), None, True, "not a price"):
            with self.subTest(value=value), self.assertRaises(ValueError):
                normalize_prices({"model": {"input_cost_per_token": value, "output_cost_per_token": 1}})
        prices = {"gemini/gemini-3.8-flash": RATES, "gpt-example": RATES}
        self.assertEqual(model_price_key("gemini-3.8-flash", prices), "gemini/gemini-3.8-flash")
        self.assertIsNone(model_price_key("🐱8", prices))
        self.assertIsNone(model_price_key("gemini-3.8-flash-custom", prices))

    def test_chat_projection_keeps_usage_without_token_double_counting(self):
        usage = _chat_usage({"prompt_tokens": 1000, "completion_tokens": 100,
                            "prompt_tokens_details": {"cached_tokens": 800},
                            "completion_tokens_details": {"reasoning_tokens": 60}})
        self.assertEqual(estimate_tokens(reported_usage({"usage": usage}), RATES)[0], 1_360_000)

    def test_tracker_does_not_turn_malformed_token_counts_into_billable_integers(self):
        for value in (True, 12.7, "12", -1):
            tracker = ResponsesPerformanceTracker()
            tracker.observe_event({"type": "response.completed", "response": {
                "usage": {"input_tokens": 100, "output_tokens": value}}})
            self.assertNotIn("output_tokens", tracker.diagnostics())


class UsageStorageTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.prices = PriceCatalog(self.root / "prices.json", NullJournal())
        self.prices._set({"gpt-example": RATES}, time.time())
        self.ledger = UsageLedger(self.root / "usage.sqlite3", self.prices, NullJournal())

    def test_categories_are_route_identity_not_display_name(self):
        for mode, category in (("forward", "native"), ("native", "native"), ("account", "subscription"), ("api_key", "external")):
            identity = usage_identity({"id": "saved-id", "auth_mode": mode},
                                      {"id": "saved-id/alias", "upstream_id": "gpt-example", "display_name": "🐱"})
            self.assertEqual(identity["usage_category"], category)
            self.assertEqual(identity["upstream_model"], "gpt-example")

    def test_concurrent_requests_deduplicate_response_but_not_distinct_calls(self):
        with ThreadPoolExecutor(max_workers=8) as workers:
            list(workers.map(lambda i: self.ledger.record(event(usage_response_id="resp_%d" % (i//2)), observed_at=100+i), range(40)))
        self.assertEqual(self.ledger.query(0, 1000)["totals"]["requests"], 20)
        self.ledger.record(event(recovery_mode="native_http_fallback"), observed_at=200)
        self.assertEqual(self.ledger.query(0, 1000)["totals"]["requests"], 20)

    def test_restart_keeps_history_price_snapshot_and_half_open_time_filters(self):
        self.ledger.record(event(prompt="PRIVATE TEXT", api_key="PRIVATE KEY"), observed_at=100)
        self.prices._set({"gpt-example": {**RATES, "output_cost_per_token": "0.000016"}}, time.time())
        self.ledger.record(event(usage_category="external", usage_owner="provider"), observed_at=200)
        restarted = UsageLedger(self.ledger.path, self.prices, NullJournal())
        result = restarted.query(100, 200)
        self.assertEqual(result["totals"]["requests"], 1)
        self.assertEqual(result["totals"]["cost_nanos"], 1_360_000)
        self.assertEqual(restarted.query(100, 201, "external")["totals"]["requests"], 1)
        raw = self.ledger.path.read_bytes()
        self.assertNotIn(b"PRIVATE", raw)
        with closing(sqlite3.connect(str(self.ledger.path))) as connection:
            rates = json.loads(connection.execute("SELECT rates FROM usage_events WHERE observed_at=100").fetchone()[0])
        self.assertEqual(rates["output"], "0.000008")

    def test_failed_requests_with_usage_count_but_unreported_usage_stays_unknown(self):
        self.ledger.record(event(status=502), observed_at=100)
        self.ledger.record(event(input_tokens=None, output_tokens=None, cached_input_tokens=None), observed_at=110)
        result = self.ledger.query(0, 200)["totals"]
        self.assertEqual((result["requests"], result["reported_requests"], result["priced_requests"]), (2, 1, 1))
        self.assertEqual(result["input_tokens"] + result["output_tokens"], 1100)

    def test_periods_preserve_a_local_boundary_inside_a_utc_hour(self):
        self.ledger.record(event(), observed_at=29*60+59)
        self.ledger.record(event(), observed_at=30*60)
        periods = self.ledger.query(0, 3600)["periods"]
        self.assertEqual([row["start"] for row in periods], [29*60, 30*60])
        self.assertEqual(sum(row["input_tokens"] for row in periods), 2000)

    def test_price_refresh_keeps_last_valid_catalog_when_offline_or_invalid(self):
        fake = SimpleNamespace(open=lambda *a, **k: io.BytesIO(json.dumps({"gpt-example": RATES}).encode()))
        with patch("easy_multi_provider.usage_pricing.build_opener", return_value=fake):
            self.assertTrue(self.prices.refresh())
        restored = PriceCatalog(self.prices.path, NullJournal())
        self.assertEqual(restored.quote(event())["cost_nanos"], 1_360_000)
        for raw in (b"{}", b"not json"):
            fake = SimpleNamespace(open=lambda *a, **k: io.BytesIO(raw))
            with patch("easy_multi_provider.usage_pricing.build_opener", return_value=fake):
                self.assertFalse(restored.refresh())
            self.assertEqual(restored.quote(event())["cost_nanos"], 1_360_000)
        with patch("easy_multi_provider.usage_pricing.build_opener", side_effect=OSError("offline")):
            self.assertFalse(restored.refresh())
        self.assertEqual(restored.snapshot()["error"], "refresh_failed")

    def test_price_worker_does_not_fetch_fresh_prices_on_restart(self):
        with patch.object(self.prices, "refresh") as refresh:
            self.prices.start()
            self.prices.stop()
            refresh.assert_not_called()

    def test_prices_arriving_later_only_fill_unknown_costs(self):
        self.ledger.record(event(), observed_at=100)
        self.ledger.record(event(upstream_model="new-model"), observed_at=110)
        self.prices._set({"gpt-example": {**RATES, "output_cost_per_token": "1"}, "new-model": RATES}, time.time())
        self.ledger.price_pending()
        self.assertEqual(self.ledger.query(0, 200)["totals"]["priced_requests"], 2)
        self.assertEqual(self.ledger.query(0, 200)["totals"]["cost_nanos"], 2_720_000)

    def test_price_refresh_is_due_at_24_hours_and_not_on_each_request(self):
        class Stop:
            def __init__(self):
                self.delays = []
            def is_set(self):
                return False
            def wait(self, seconds):
                self.delays.append(seconds)
                return len(self.delays) > 1
        self.prices.fetched_at = 100
        self.prices._stop = Stop()
        def refreshed():
            self.prices.fetched_at = 86500
            return True
        with patch("easy_multi_provider.usage_pricing.time.time", return_value=86500), patch.object(self.prices, "refresh", side_effect=refreshed) as refresh:
            self.prices._run()
        self.assertEqual(self.prices._stop.delays, [0, 86400])
        refresh.assert_called_once()

    def test_unwritable_ledger_reports_problem_without_breaking_model_calls(self):
        with patch.object(self.ledger, "_connect", side_effect=sqlite3.OperationalError("disk full")):
            self.ledger.record(event())
        self.assertTrue(self.ledger.write_error)


class UsageRoutingTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        path = self.root / "config.json"
        save(normalize({"native_catalog_path": str(self.root / "native.json"),
            "providers": [{"id": "external", "protocol": "responses", "base_url": "https://example.invalid/v1"}],
            "models": [{"id": "external/alias", "provider": "external", "upstream_id": "gpt-example", "enabled": True}]}), path)
        self.state = AppState(path, runtime_controller=object())
        self.state.usage_prices._set({"gpt-example": RATES}, time.time())

    def test_nonstream_and_sse_record_one_complete_usage_each(self):
        for stream in (False, True):
            response = {"id": "resp_%s" % stream, "object": "response", "status": "completed", "output": [],
                        "usage": {"input_tokens": 1000, "input_tokens_details": {"cached_tokens": 800},
                                  "output_tokens": 100, "output_tokens_details": {"reasoning_tokens": 60}}}
            if stream:
                wire = ('data: '+json.dumps({"type": "response.completed", "response": response})+'\n\n').encode()
                target, upstream = "forward_responses_stream", iter([wire])
            else:
                target, upstream = "forward_responses", (200, "application/json", json.dumps(response).encode())
            with patch.object(router, target, return_value=upstream), patch.object(router, "_request", side_effect=AssertionError("No live upstream in tests")):
                _, received = self.state.codex.route({"model": "external/alias", "input": "private", "stream": stream}, {})
                if stream:
                    list(received)
        data = self.state.usage.query(0, time.time()+1)
        self.assertEqual(data["totals"]["requests"], 2)
        self.assertEqual(data["totals"]["cost_nanos"], 2_720_000)
        self.assertEqual(data["groups"][0]["category"], "external")
        self.assertEqual(data["groups"][0]["model"], "gpt-example")

    def test_native_websocket_uses_resolved_account_and_actual_service_tier(self):
        for mode in ("forward", "account"):
            plan = SimpleNamespace(provider={"id": "test-"+mode, "auth_mode": mode},
                model={"id": "display/alias", "upstream_id": "gpt-example"},
                payload={}, target=SimpleNamespace(headers={}), requested_slug="display/alias",
                identity=SimpleNamespace(endpoint_fingerprint=""), context_observation={})
            self.state.record_native_websocket(plan, {"service_tier": "priority"}, time.monotonic(), 0, 0, 100,
                {"status": 200, "success": True}, False, True, False,
                performance={"input_tokens": 1000, "cached_input_tokens": 800,
                             "output_tokens": 100, "reasoning_tokens": 60, "service_tier": "default"})
        data = self.state.usage.query(0, time.time()+1)
        self.assertEqual(data["totals"]["requests"], 2)
        self.assertEqual(data["totals"]["cost_nanos"], 2_720_000)
        self.assertEqual({row["category"] for row in data["groups"]}, {"native", "subscription"})

    def test_management_api_rejects_bad_periods_and_requires_session(self):
        server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(self.state))
        thread = threading.Thread(target=server.serve_forever, daemon=True)
        thread.start()
        try:
            client = HTTPConnection(*server.server_address, timeout=5)
            client.request("GET", "/api/usage")
            response = client.getresponse(); response.read()
            self.assertEqual(response.status, 401)
            headers = {"Cookie": "emp_session=" + self.state.session_token}
            for query, expected in (("start=0&end=200", 200), ("start=nan&end=200", 400),
                                    ("start=200&end=100", 400), ("category=invalid", 400)):
                client.request("GET", "/api/usage?"+query, headers=headers)
                response = client.getresponse(); response.read()
                self.assertEqual(response.status, expected)
            client.close()
        finally:
            server.shutdown(); server.server_close(); thread.join(timeout=2)


if __name__ == "__main__":
    unittest.main()
