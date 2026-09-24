import json
import tempfile
import unittest
from pathlib import Path

from tools.benchmark_emp_usage_scan import (
    BenchmarkError,
    canonical_checkpoints,
    canonical_scan_status,
    canonical_usage,
    strict_json_equal,
    synthetic_ledger_row,
    validate_parameters,
    validate_total_requests,
    write_rollout,
)


class Fixture:
    @staticmethod
    def header(model):
        return [
            {"type": "session_meta", "timestamp": "2026-09-08T01:00:00Z",
             "payload": {"id": "fixture-session", "model_provider": "openai"}},
            {"type": "event_msg", "timestamp": "2026-09-08T01:00:00Z",
             "payload": {"type": "task_started", "turn_id": "fixture-turn"}},
            {"type": "turn_context", "timestamp": "2026-09-08T01:00:00Z",
             "payload": {"model": model, "turn_id": "fixture-turn"}},
        ]

    @staticmethod
    def usage(count, second, output, reasoning):
        usage = {
            "input_tokens": count,
            "cached_input_tokens": 40,
            "output_tokens": output,
            "reasoning_output_tokens": reasoning,
            "total_tokens": count + output,
        }
        return {
            "type": "event_msg",
            "timestamp": f"2026-09-08T01:00:{second:02d}Z",
            "payload": {"type": "token_count", "info": {
                "last_token_usage": usage,
                "total_token_usage": usage,
            }},
        }


class UsageScanBenchmarkTests(unittest.TestCase):
    def test_parameter_bounds_are_explicit(self):
        validate_parameters(1, 0)
        validate_parameters(100_000, 120_000)
        for values in ((0, 0), (1_000_001, 0), (1, -1), (1, 1_000_001)):
            with self.subTest(values=values), self.assertRaises(BenchmarkError):
                validate_parameters(*values)

    def test_seed_cost_matches_dollar_rates_in_nanos(self):
        row = synthetic_ledger_row(1, 1_800_000_000)
        input_tokens, output_tokens = row[7], row[8]
        self.assertEqual(row[13], input_tokens * 2_000 + output_tokens * 8_000)
        self.assertEqual(len(row), 23)
        inconsistent = synthetic_ledger_row(0, 1_800_000_000)
        self.assertIsNone(inconsistent[13])

    def test_management_total_must_include_seed_history_and_forwarding(self):
        validate_total_requests({"totals": {"requests": 5_001}}, 1_000 + 4_000 + 1)
        with self.assertRaisesRegex(BenchmarkError, "usage_total_requests_mismatch"):
            validate_total_requests({"totals": {"requests": 5_000}}, 5_001)

    def test_rollout_uses_complete_jsonl_rows_and_fixture_usage_shape(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "sessions" / "rollout.jsonl"
            size = write_rollout(path, 3, 1_788_500_000, Fixture)
            rows = [json.loads(line) for line in path.read_text(encoding="utf-8").splitlines()]
        self.assertEqual(len(rows), 6)
        self.assertGreater(size, 0)
        self.assertEqual(rows[2]["payload"]["model"], "benchmark/model")
        usage_rows = [row for row in rows if row.get("payload", {}).get("type") == "token_count"]
        self.assertEqual(len(usage_rows), 3)
        self.assertEqual(
            [row["payload"]["info"]["last_token_usage"]["input_tokens"] for row in usage_rows],
            [100, 101, 102],
        )

    def test_canonical_usage_normalizes_only_scan_wall_clock(self):
        base = {
            "history": {"running": False, "last_scan_at": 10},
            "groups": [
                {"category": "native", "owner": "b"},
                {"category": "external", "owner": "a"},
            ],
            "periods": [{"start": 120}, {"start": 60}],
            "issues": [],
            "sources": [],
            "totals": {"input_tokens": 42},
        }
        other = json.loads(json.dumps(base))
        other["history"]["last_scan_at"] = 20
        self.assertEqual(canonical_usage(base), canonical_usage(other))
        other["groups"].reverse()
        self.assertNotEqual(canonical_usage(base), canonical_usage(other))
        other["groups"].reverse()
        other["totals"]["input_tokens"] = 43
        self.assertNotEqual(canonical_usage(base), canonical_usage(other))

    def test_strict_json_equality_preserves_numeric_types_and_array_order(self):
        self.assertTrue(strict_json_equal({"value": 3.0}, {"value": 3.0}))
        self.assertFalse(strict_json_equal({"value": 3}, {"value": 3.0}))
        self.assertFalse(strict_json_equal({"values": [1, 2]}, {"values": [2, 1]}))

    def test_checkpoint_normalization_keeps_state_and_prefix(self):
        with tempfile.TemporaryDirectory() as first, tempfile.TemporaryDirectory() as second:
            homes = []
            rows = []
            for directory, device, inode in ((first, 1, 11), (second, 2, 22)):
                home = Path(directory)
                rollout = home / "sessions" / "rollout.jsonl"
                rollout.parent.mkdir()
                rollout.write_text("fixture\n", encoding="utf-8")
                homes.append(home)
                rows.append((str(rollout), json.dumps({
                    "identity": [device, inode, "same-prefix"],
                    "offset": 8,
                    "size": 8,
                    "mtime": 1_790_000_000_000_000_000,
                    "tail": "same-tail",
                    "state": {"turn": "same-turn"},
                })))
            left = canonical_checkpoints([rows[0]], homes[0])
            right = canonical_checkpoints([rows[1]], homes[1])
        self.assertEqual(left, right)
        changed = json.loads(rows[1][1])
        changed["offset"] = 7
        self.assertNotEqual(left, canonical_checkpoints(
            [(rows[1][0], json.dumps(changed))], homes[1]
        ))

    def test_scan_status_keeps_counts_but_normalizes_clock(self):
        left = canonical_scan_status({
            "running": False, "queued": False, "files": 1, "updated": 1,
            "errors": 0, "last_scan_at": 10,
        })
        right = canonical_scan_status({
            "running": False, "queued": False, "files": 1, "updated": 1,
            "errors": 0, "last_scan_at": 20,
        })
        self.assertEqual(left, right)
        right["updated"] = 0
        self.assertNotEqual(left, right)


if __name__ == "__main__":
    unittest.main()
