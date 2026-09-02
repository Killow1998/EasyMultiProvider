import unittest

from easy_multi_provider.diagnostic_analytics import summarize_route_observations


class DiagnosticAnalyticsTest(unittest.TestCase):
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

        result = summarize_route_observations(records)

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

        health = summarize_route_observations(records)["health"]

        self.assertEqual(health["status_502_count"], 1)
        self.assertEqual(health["status_503_count"], 1)
        self.assertEqual(health["status_504_count"], 1)
        self.assertEqual(health["status_503_rate"], 33.3)

    @staticmethod
    def record(
        model_id,
        speed_mode,
        status,
        ttft_ms,
        tokens_per_second,
        error_class="none",
    ):
        return {
            "route": "responses",
            "model_id": model_id,
            "speed_mode": speed_mode,
            "status": status,
            "error_class": error_class,
            "ttft_ms": ttft_ms,
            "tokens_per_second": tokens_per_second,
            "observed_at": "2026-09-02T12:00:00+00:00",
        }


if __name__ == "__main__":
    unittest.main()
