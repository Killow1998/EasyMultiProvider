import unittest
from datetime import datetime, timezone

from easy_multi_provider.diagnostic_analytics import summarize_route_observations


class DiagnosticAnalyticsTest(unittest.TestCase):
    NOW = datetime(2026, 9, 3, 12, 0, tzinfo=timezone.utc)

    def test_fallback_attempt_is_not_an_additional_failed_request(self):
        attempt = self.record("model-a", "standard", 502, None, None, "tls_failure")
        attempt.update(request_id="same-request", recovery_mode="native_http_fallback")
        for status, error in [(200, "none"), (502, "tls_failure")]:
            with self.subTest(status=status):
                final = self.record("model-a", "standard", status, 1000, 40, error)
                final.update(request_id="same-request", input_tokens=1000, cached_input_tokens=800)
                result = summarize_route_observations([attempt, final], now=self.NOW)
                self.assertEqual(result["health"]["sample_count"], 1)
                self.assertEqual(result["health"]["fallback_attempt_count"], 1)
                self.assertEqual(result["health"]["status_502_count"], int(status == 502))
                self.assertEqual(result["cache"]["models"][0]["call_count"], 1)
        pending = summarize_route_observations([attempt], now=self.NOW)
        self.assertEqual(pending["health"]["sample_count"], 0)
        self.assertEqual(pending["health"]["fallback_attempt_count"], 1)

    def test_independent_client_retries_still_count_as_separate_requests(self):
        first = self.record("model-a", "standard", 502, None, None, "tls_failure")
        second = self.record("model-a", "standard", 200, 1000, 40)
        first["request_id"] = second["request_id"] = "reused-client-id"
        health = summarize_route_observations([first, second], now=self.NOW)["health"]
        self.assertEqual(health["sample_count"], 2)
        self.assertEqual(health["status_502_rate"], 50)
        self.assertEqual(health["fallback_attempt_count"], 0)

    def test_old_measurements_remain_health_history_not_current_speed(self):
        old = self.record("old-model", "standard", 200, 10, 9000)
        old.pop("performance_schema")
        result = summarize_route_observations([old], now=self.NOW)
        self.assertEqual(result["models"], [])
        self.assertEqual(result["health"]["success_count"], 1)

    def test_previous_schema_keeps_ttft_without_mixing_old_tps(self):
        old = self.record("gpt-5.6-sol", "standard", 200, 5000, 9999)
        old["performance_schema"] = 2

        model = summarize_route_observations([old], now=self.NOW)["models"][0]

        self.assertEqual(model["ttft_ms"], 5000)
        self.assertEqual(model["ttft_samples"], 1)
        self.assertIsNone(model["tokens_per_second"])
        self.assertEqual(model["tps_samples"], 0)

    def test_health_and_model_medians_keep_fast_separate(self):
        records = [
            self.record("gpt-5.6-sol", "standard", 200, 5000, 50),
            self.record("gpt-5.6-sol", "standard", 200, 7000, 60),
            self.record("gpt-5.6-sol", "fast", 200, 3000, 80),
            self.record(
                "gemini-3.7-flash",
                "unknown",
                429,
                None,
                None,
                error_class="rate_limit",
            ),
            self.record(
                "gpt-5.6-luna",
                "standard",
                502,
                None,
                None,
                error_class="upstream_5xx",
            ),
            self.record(
                "gpt-5.6-sol",
                "standard",
                None,
                None,
                None,
                error_class="client_disconnect",
            ),
            self.record("codex-auto-review", "standard", 200, 100, 1000),
        ]

        result = summarize_route_observations(records, now=self.NOW)

        self.assertEqual(result["health"]["sample_count"], 6)
        self.assertEqual(result["health"]["success_count"], 4)
        self.assertEqual(result["health"]["status_429_rate"], 16.7)
        self.assertEqual(result["health"]["status_502_rate"], 16.7)
        self.assertEqual(result["health"]["cancelled_count"], 1)
        self.assertEqual(
            result["health"]["failure_classes"],
            [
                {"error_class": "rate_limit", "count": 1, "rate": 16.7},
                {"error_class": "upstream_5xx", "count": 1, "rate": 16.7},
            ],
        )
        models = {
            (item["model_id"], item["speed_mode"]): item
            for item in result["models"]
        }
        self.assertEqual(models[("gpt-5.6-sol", "standard")]["ttft_ms"], 6000.0)
        self.assertEqual(
            models[("gpt-5.6-sol", "standard")]["tokens_per_second"], 55.0
        )
        self.assertEqual(models[("gpt-5.6-sol", "fast")]["ttft_ms"], 3000.0)
        self.assertNotIn(("codex-auto-review", "standard"), models)

    def test_health_keeps_gateway_network_and_timeout_statuses_separate(self):
        records = [
            self.record("model-a", "unknown", 502, None, None, "upstream_5xx"),
            self.record("model-a", "unknown", 503, None, None, "proxy_unavailable"),
            self.record("model-a", "unknown", 504, None, None, "connect_timeout"),
        ]

        health = summarize_route_observations(records, now=self.NOW)["health"]

        self.assertEqual(health["status_502_count"], 1)
        self.assertEqual(health["status_503_count"], 1)
        self.assertEqual(health["status_504_count"], 1)
        self.assertEqual(health["status_503_rate"], 33.3)

    def test_speed_uses_recent_window_but_keeps_previous_window_for_trend(self):
        records = [
            self.record(
                "gpt-5.6-sol",
                "standard",
                200,
                9000,
                20,
                observed_at="2026-08-20T12:00:00+00:00",
            )
            for _ in range(5)
        ]
        records.extend(
            self.record("gpt-5.6-sol", "standard", 200, 6000, 50)
            for _ in range(20)
        )
        records.append(
            self.record(
                "gpt-5.6-sol",
                "standard",
                502,
                100,
                1000,
                error_class="upstream_5xx",
            )
        )
        records.extend(
            self.record("gpt-5.6-sol", "standard", 200, 4000, 75)
            for _ in range(20)
        )

        result = summarize_route_observations(records, now=self.NOW)
        model = result["models"][0]

        self.assertEqual(result["performance_window"], {"calls": 20, "days": 7})
        self.assertEqual(model["call_count"], 20)
        self.assertEqual(model["retained_call_count"], 40)
        self.assertEqual(model["ttft_ms"], 4000.0)
        self.assertEqual(model["previous_ttft_ms"], 6000.0)
        self.assertEqual(model["ttft_change_percent"], 33.3)
        self.assertEqual(model["tokens_per_second"], 75.0)
        self.assertEqual(model["previous_tokens_per_second"], 50.0)
        self.assertEqual(model["tps_change_percent"], 50.0)

    @staticmethod
    def record(
        model_id,
        speed_mode,
        status,
        ttft_ms,
        tokens_per_second,
        error_class="none",
        observed_at="2026-09-02T12:00:00+00:00",
    ):
        return {
            "performance_schema": 3,
            "route": "responses",
            "model_id": model_id,
            "speed_mode": speed_mode,
            "status": status,
            "error_class": error_class,
            "ttft_ms": ttft_ms,
            "tokens_per_second": tokens_per_second,
            "observed_at": observed_at,
        }


if __name__ == "__main__":
    unittest.main()
