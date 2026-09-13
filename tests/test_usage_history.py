"""Realistic rollout boundaries, numeric deduplication and incremental persistence."""

import json
import shutil
import sqlite3
import tempfile
import unittest
from contextlib import closing
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.diagnostic_journal import NullJournal
from easy_multi_provider.usage_history import UsageHistoryScanner, history_routes, parse_record
from easy_multi_provider.usage_ledger import UsageLedger, usage_context
from easy_multi_provider.usage_pricing import PriceCatalog

TURN = "01a07fda-68d2-7d03-bb62-75a0abe349c2"


def record(kind, payload, second=0):
    return {"type": kind, "timestamp": "2026-09-08T01:00:%02dZ" % second, "payload": payload}


def usage(count=100, second=1, output=20, reasoning=10, cumulative=None):
    last = {"input_tokens": count, "cached_input_tokens": 40, "output_tokens": output,
            "reasoning_output_tokens": reasoning, "total_tokens": count + output}
    return record("event_msg", {"type": "token_count", "info": {
        "last_token_usage": last, "total_token_usage": cumulative or last}}, second)


def header(model="gpt-example", fork=None):
    return [record("session_meta", {"id": "session", "model_provider": "openai", "forked_from_id": fork}),
            record("event_msg", {"type": "task_started", "turn_id": TURN}),
            record("turn_context", {"model": model, "turn_id": TURN})]


class HistoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.home = self.root / "codex"
        (self.home / "sessions").mkdir(parents=True)
        self.path = self.home / "sessions" / "rollout.jsonl"
        self.prices = PriceCatalog(self.root / "prices.json", NullJournal())
        self.prices._set({"gpt-example": {"input_cost_per_token": "0.000002", "output_cost_per_token": "0.000008",
                                       "cache_read_input_token_cost": "0.0000002"}}, 100)
        self.ledger = UsageLedger(self.root / "usage.sqlite3", self.prices, NullJournal())
        self.scanner = UsageHistoryScanner(self.ledger, lambda: self.home, lambda: {})

    def write(self, rows, mode="w"):
        with self.path.open(mode, encoding="utf-8") as stream:
            for row in rows:
                stream.write(json.dumps(row) + "\n")

    def totals(self):
        return self.ledger.query(0, 2_000_000_000)["totals"]

    def test_repeat_scan_restart_append_archive_and_fork_do_not_duplicate(self):
        self.write(header() + [usage(), usage(second=2)])
        self.scanner.scan()
        self.assertEqual(self.totals()["input_tokens"], 100)
        with patch.object(self.scanner, "_scan_file", wraps=self.scanner._scan_file) as scan:
            self.scanner.scan()
            self.assertEqual(self.scanner.status["updated"], 0)
            scan.assert_called_once()
        new = UsageHistoryScanner(UsageLedger(self.ledger.path, self.prices, NullJournal()), lambda: self.home, lambda: {})
        self.write([usage(120, 3, cumulative={"input_tokens": 220, "output_tokens": 40})], "a")
        new.scan()
        self.assertEqual(self.totals()["input_tokens"], 220)
        archive = self.home / "archived_sessions"
        archive.mkdir()
        shutil.copy2(self.path, archive / "copy.jsonl")
        self.write(header(fork="parent") + [usage(), usage(120, 3, cumulative={"input_tokens": 220, "output_tokens": 40})])
        new.scan()
        self.assertEqual(self.totals()["input_tokens"], 220)

    def test_partial_line_is_retried_and_truncation_rescans(self):
        self.write(header())
        line = json.dumps(usage())
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(line[:30])
        self.scanner.scan()
        self.assertEqual(self.totals()["requests"], 0)
        with self.path.open("a", encoding="utf-8") as stream:
            stream.write(line[30:] + "\n")
        self.scanner.scan()
        self.assertEqual(self.totals()["requests"], 1)
        self.write(header() + [usage(130, 5)])
        self.scanner.scan()
        self.assertEqual(self.totals()["input_tokens"], 230)

    def test_realtime_and_history_match_once_but_identical_calls_still_count(self):
        self.write(header() + [usage(), usage(second=3, cumulative={"input_tokens": 200, "output_tokens": 40})])
        self.scanner.scan()
        self.ledger.record({"route": "responses", "usage_category": "native", "upstream_model": "gpt-example",
            "usage_turn": TURN, "route_model": "gpt-example", "input_tokens": 100, "output_tokens": 20,
            "cached_input_tokens": 40, "reasoning_tokens": 10}, 1788829201)
        self.assertEqual(self.totals()["input_tokens"], 200)
        self.assertEqual(self.totals()["requests"], 2)

    def test_old_gateway_inconsistent_totals_are_unpriced_and_not_corrected(self):
        broken = usage(output=2, reasoning=10)
        broken["payload"]["info"]["last_token_usage"]["total_tokens"] = 112
        self.write(header() + [broken])
        self.scanner.scan()
        result = self.ledger.query(0, 2_000_000_000)
        self.assertEqual(result["totals"]["output_tokens"], 2)
        self.assertEqual(result["totals"]["priced_requests"], 0)
        self.assertEqual(result["issues"][0]["price_issue"], "inconsistent_usage")
        self.ledger.price_pending()
        self.assertEqual(self.totals()["priced_requests"], 0)

    def test_source_inference_keeps_historical_identity_separate(self):
        routes = history_routes({"accounts": [{"id": "private-account", "prefix": "egg"}],
            "providers": [{"id": "na2h", "auth_mode": "api_key"}],
            "models": [{"id": "na2h/gemini", "provider": "na2h", "upstream_model": "gemini"}]})
        for model, category in (("gpt-example", "native"), ("egg/gpt-example", "subscription"),
                                ("na2h/gemini", "external"), ("lost/gpt-example", "unknown")):
            state = {}
            for row in header(model):
                parse_record(row, state, routes)
            event, _ = parse_record(usage(), state, routes)
            self.assertEqual(event["usage_category"], category)
            self.assertTrue(event["usage_owner"].startswith("history:"))
            self.assertNotIn("private-account", json.dumps(event))

    def test_cumulative_only_fork_baseline_is_not_new_spending(self):
        state = {}
        for row in header(fork="parent"):
            parse_record(row, state, {})
        item = usage()
        item["payload"]["info"].pop("last_token_usage")
        self.assertIsNone(parse_record(item, state, {}))
        item = usage(150, 2)
        item["payload"]["info"].pop("last_token_usage")
        event, _ = parse_record(item, state, {})
        self.assertEqual(event["input_tokens"], 50)
        self.assertEqual(event["usage_issue"], "aggregate_usage")

    def test_cursor_transaction_rolls_back_on_insert_failure(self):
        self.write(header() + [usage()])
        with patch.object(self.ledger, "_insert", side_effect=sqlite3.OperationalError("disk full")):
            self.scanner.scan()
        self.assertIsNone(self.ledger.history_checkpoint(str(self.path)))
        self.scanner.scan()
        self.assertEqual(self.totals()["requests"], 1)

    def test_schema_upgrade_preserves_rows_and_creates_backup(self):
        self.ledger.record({"route": "responses", "usage_category": "native", "input_tokens": 1, "output_tokens": 2})
        with closing(sqlite3.connect(self.ledger.path)) as connection:
            connection.execute("DROP VIEW accounted_usage")
            connection.execute("DROP INDEX usage_match")
            connection.execute("DROP INDEX usage_turn")
            for field in ("origin", "usage_turn", "route_model", "match_key"):
                connection.execute("ALTER TABLE usage_events DROP COLUMN " + field)
            connection.commit()
        self.ledger = UsageLedger(self.ledger.path, self.prices, NullJournal())
        self.assertEqual(self.totals()["requests"], 1)
        with closing(sqlite3.connect(self.ledger.path.with_suffix(".pre-v011.sqlite3"))) as backup:
            self.assertEqual(backup.execute("SELECT COUNT(*) FROM usage_events").fetchone()[0], 1)

    def test_uses_transport_turn_metadata_without_conversation_content(self):
        metadata = json.dumps({"turn_id": TURN})
        for body, headers in (({"model": "x", "client_metadata": {"x-codex-turn-metadata": metadata}}, {}),
                              ({"model": "x"}, {"x-codex-turn-metadata": metadata})):
            self.assertEqual(usage_context(body, headers), {"usage_turn": TURN, "route_model": "x"})


if __name__ == "__main__":
    unittest.main()
