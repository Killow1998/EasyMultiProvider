import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.quota_history import QuotaHistoryStore


def snapshot(used_primary=20, used_secondary=40):
    return {
        "rate_limits": {
            "limitId": "codex",
            "primary": {
                "usedPercent": used_primary,
                "windowDurationMins": 300,
                "resetsAt": 2_000_000,
            },
            "secondary": {
                "usedPercent": used_secondary,
                "windowDurationMins": 10_080,
                "resetsAt": 3_000_000,
            },
        }
    }


class QuotaHistoryStoreTests(unittest.TestCase):
    def test_empty_history_and_requested_ranges(self):
        with tempfile.TemporaryDirectory() as directory:
            store = QuotaHistoryStore(Path(directory) / "history.sqlite3")

            result = store.query("@native", "1h", now=1_000_000)

        self.assertEqual(result["range"], "1h")
        self.assertEqual(result["retention_days"], 15)
        self.assertEqual(result["series"], [])

    def test_same_five_minute_bucket_is_replaced_and_old_samples_are_pruned(self):
        with tempfile.TemporaryDirectory() as directory:
            store = QuotaHistoryStore(Path(directory) / "history.sqlite3")
            now = 2_000_100
            store.append_snapshot("@native", snapshot(10, 30), observed_at=now - 16 * 86400)
            store.append_snapshot("@native", snapshot(20, 40), observed_at=now)
            store.append_snapshot("@native", snapshot(25, 45), observed_at=now + 120)

            result = store.query("@native", "all", now=now + 120)

        self.assertEqual(len(result["series"]), 2)
        primary = next(
            item for item in result["series"] if item["window_kind"] == "primary"
        )
        self.assertEqual(len(primary["points"]), 1)
        self.assertEqual(primary["points"][0]["remaining_percent"], 75.0)

    def test_range_query_excludes_older_points(self):
        with tempfile.TemporaryDirectory() as directory:
            store = QuotaHistoryStore(Path(directory) / "history.sqlite3")
            now = 2_000_000
            store.append_snapshot("ship", snapshot(10, 20), observed_at=now - 7200)
            store.append_snapshot("ship", snapshot(30, 40), observed_at=now)

            hour = store.query("ship", "1h", now=now)
            day = store.query("ship", "1d", now=now)

        self.assertEqual(len(hour["series"][0]["points"]), 1)
        self.assertEqual(len(day["series"][0]["points"]), 2)


if __name__ == "__main__":
    unittest.main()
