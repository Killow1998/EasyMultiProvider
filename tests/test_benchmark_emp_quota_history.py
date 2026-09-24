import unittest

from tools.benchmark_emp_quota_history import (
    ACCOUNT_ID,
    RANGE_NAMES,
    BenchmarkError,
    fixture_snapshot,
    plan_type_for,
    quota_history_queries,
    quota_history_query_path,
    quota_response_for_comparison,
    strict_json_equal,
    validate_sample_count,
)


def snapshot_factory(*, used_primary, used_secondary, plan_type):
    return {
        "plan_type": plan_type,
        "rate_limits": {
            "limitId": "codex",
            "primary": {
                "usedPercent": used_primary,
                "windowDurationMins": 300,
                "resetsAt": 10,
            },
            "secondary": {
                "usedPercent": used_secondary,
                "windowDurationMins": 10_080,
                "resetsAt": 20,
            },
        },
    }


class QuotaHistoryBenchmarkTests(unittest.TestCase):
    def test_ranges_and_sample_bounds_match_official_store(self):
        self.assertEqual(RANGE_NAMES, ("1h", "1d", "1w", "all"))
        validate_sample_count(1)
        validate_sample_count(3_000)
        validate_sample_count(4_320)
        for count in (0, 4_321):
            with self.subTest(count=count), self.assertRaises(BenchmarkError):
                validate_sample_count(count)

    def test_fixture_covers_official_window_reset_bucket_and_plan_shapes(self):
        sample = fixture_snapshot(1, 1_800_000_000, 3_000, snapshot_factory)
        self.assertEqual(sample["plan_type"], "plus")
        self.assertEqual(sample["rate_limits"]["primary"]["windowDurationMins"], 300)
        self.assertEqual(sample["rate_limits"]["secondary"]["windowDurationMins"], 10_080)
        self.assertEqual(sample["rate_limits"]["primary"]["resetsAt"], 1_800_018_000)
        self.assertEqual(sample["rate_limits"]["secondary"]["resetsAt"], 1_800_604_800)
        self.assertEqual(sample["rate_limits_by_limit_id"]["other"]["primary"]["windowDurationMins"], 60)
        self.assertIn("reset_credits", sample["credits"])
        self.assertEqual(
            [plan_type_for(index, 3_000) for index in (0, 1_000, 2_000)],
            ["plus", "ProLite", "pro"],
        )

    def test_management_paths_cover_the_four_real_ranges(self):
        queries = quota_history_queries()
        self.assertEqual(list(queries), list(RANGE_NAMES))
        self.assertEqual(
            queries["all"],
            "/api/accounts/%40native/quota-history?range=all",
        )
        self.assertEqual(
            quota_history_query_path("account/a", "1h"),
            "/api/accounts/account%2Fa/quota-history?range=1h",
        )
        with self.assertRaisesRegex(BenchmarkError, "unsupported_quota_history_range"):
            quota_history_query_path(ACCOUNT_ID, "2d")

    def test_strict_json_comparison_ignores_object_order_only(self):
        self.assertTrue(strict_json_equal({"a": 1, "b": [1, 2]}, {"b": [1, 2], "a": 1}))
        self.assertFalse(strict_json_equal({"value": 1}, {"value": 1.0}))
        self.assertFalse(strict_json_equal({"points": [1, 2]}, {"points": [2, 1]}))

    def test_response_shape_keeps_true_timestamps_integer_typed(self):
        payload = {
            "account_id": ACCOUNT_ID,
            "range": "1d",
            "start_at": 1_799_913_600,
            "end_at": 1_800_000_000,
            "sample_interval_seconds": 300,
            "retention_days": 15,
            "series": [],
            "plans": [],
        }
        canonical = quota_response_for_comparison(payload, "1d")
        self.assertEqual(canonical["start_at"], "now-minus-range")
        self.assertEqual(canonical["end_at"], "now")
        payload["end_at"] = float(payload["end_at"])
        with self.assertRaisesRegex(BenchmarkError, "bounds_not_integer"):
            quota_response_for_comparison(payload, "1d")

    def test_response_bounds_must_match_requested_range_and_current_time(self):
        payload = {
            "account_id": ACCOUNT_ID,
            "range": "1h",
            "start_at": 1_799_996_400,
            "end_at": 1_800_000_000,
            "sample_interval_seconds": 300,
            "retention_days": 15,
            "series": [],
            "plans": [],
        }
        with self.assertRaisesRegex(BenchmarkError, "range_bounds_mismatch"):
            quota_response_for_comparison({**payload, "start_at": payload["start_at"] + 1}, "1h")
        with self.assertRaisesRegex(BenchmarkError, "end_not_current"):
            quota_response_for_comparison(payload, "1h", current_epoch=1_700_000_000)


if __name__ == "__main__":
    unittest.main()
