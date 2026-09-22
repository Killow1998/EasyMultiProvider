"""Usage UI contract against running Python and Rust, using existing fixtures."""
import json
import os
from pathlib import Path
import shutil
import sys
import tempfile
import time
import unittest
from contextlib import ExitStack

from easy_multi_provider.diagnostic_journal import NullJournal
from easy_multi_provider.usage_ledger import UsageLedger
from easy_multi_provider.usage_pricing import PriceCatalog
from tests.rust_e2e_support import EmpProcess, Upstream
from tests.test_usage_history import header, usage, TURN
from tests.test_usage_ledger import RATES, event


@unittest.skipUnless(os.environ.get("EMP_RUST_BINARY"), "set EMP_RUST_BINARY for process E2E")
class RustUsageEndToEnd(unittest.TestCase):
    maxDiff = None
    def setUp(self):
        self.stack = ExitStack()
        self.addCleanup(self.stack.close)
        self.root = Path(self.stack.enter_context(tempfile.TemporaryDirectory(prefix="emp-usage-e2e-")))
        self.upstream = Upstream()
        self.stack.callback(self.upstream.close)
        # Shared persisted value: default serde_json float parsing previously
        # changed the final bit instead of preserving Python's parsed number.
        self.fetched = float(str(int(time.time()) - 1) + ".5514197")

    def fixture(self, name, *, seed=None, rollout=None):
        root = self.root / name
        home = root / "codex"
        home.mkdir(parents=True)
        state = root / "state"
        state.mkdir()
        (state / "api_prices.json").write_text(json.dumps({
            "fetched_at": self.fetched, "prices": {"upstream-model": RATES, "new-model": RATES}}))
        (root / "native.json").write_text('{"models":[]}')
        (home / "auth.json").write_text(json.dumps({"tokens": {
            "access_token": "e2e-native-token", "account_id": "e2e-owner"}}))
        config = {"native_catalog_path": str(root / "native.json"), "providers": [
            {"id": mode, "name": mode, "base_url": self.upstream.base_url,
             "protocol": "responses", "auth_mode": "forward" if mode == "native" else "api_key",
             **({} if mode == "native" else {"api_key": "e2e-provider-token"})}
            for mode in ("external", "native")], "models": [
                {"id": mode + "/alias", "provider": mode, "upstream_id": "upstream-model", "enabled": True}
                for mode in ("external", "native")]}
        (root / "config.json").write_text(json.dumps(config))
        if seed:
            shutil.copyfile(seed, state / "usage.sqlite3")
        if rollout:
            (home / "sessions").mkdir()
            (home / "sessions/rollout.jsonl").write_text(rollout)
        return root, home

    def start(self, name, root, home):
        command = ([sys.executable, "-m", "easy_multi_provider"] if name == "python"
                   else [str(Path(os.environ["EMP_RUST_BINARY"]).resolve())])
        backend = EmpProcess.from_config(command, root / "config.json", home)
        self.stack.callback(backend.close)
        return backend

    def query(self, backend, *, scan=False, category="all"):
        previous = None
        if scan:
            previous = json.loads(backend.request("GET", "/api/usage")[2])["history"]["last_scan_at"]
            self.assertEqual(backend.request("POST", "/api/usage/scan", {})[0], 202)
        deadline = time.monotonic() + 8
        while True:
            status, _, raw = backend.request("GET", "/api/usage?start=0&end=2000000000&category=" + category)
            self.assertEqual(status, 200, raw)
            result = json.loads(raw)
            history = result["history"]
            if history["last_scan_at"] and not history["running"] and (not scan or history["last_scan_at"] != previous):
                break
            self.assertLess(time.monotonic(), deadline, result)
            time.sleep(.02)
        self.assertEqual(history["errors"], 0, result)
        history["last_scan_at"] = "<scan timestamp>"
        return result

    @staticmethod
    def normalize_live_times(value):
        # Live request timestamps and their minute buckets are nondeterministic.
        value = json.loads(json.dumps(value))
        if value["first_record_at"] is not None:
            value["first_record_at"] = "<request timestamp>"
        for period in value["periods"]:
            period["start"] = "<request minute>"
        return value

    def test_existing_python_database_and_late_price_snapshot(self):
        prices = PriceCatalog(self.root / "seed-prices.json", NullJournal())
        prices._set({"upstream-model": RATES}, self.fetched)
        ledger = UsageLedger(self.root / "seed.sqlite3", prices, NullJournal())
        for index, extra in enumerate(({}, {"service_tier": "priority"}, {},
                                      {"input_tokens": None}, {"cached_input_tokens": 1001})):
            ledger.record(event(usage_category="external", usage_owner="external", upstream_model="upstream-model",
                                **extra), 100 + index)
        # A model which becomes priced only on the following service startup.
        ledger.record(event(usage_category="external", usage_owner="external", upstream_model="new-model"), 110)
        results = []
        for name in ("python", "rust"):
            root, home = self.fixture(name, seed=ledger.path)
            backend = self.start(name, root, home)
            result = self.query(backend)
            self.assertEqual(result["totals"]["priced_requests"], 4, result)
            results.append(result)
            for query in ("start=bad", "start=0&end=0", "category=bad"):
                self.assertEqual(backend.request("GET", "/api/usage?" + query)[0], 400)
        self.assertEqual(results[0], results[1])

    def test_http_usage_cost_deduplication_and_restart(self):
        outcomes = []
        for name in ("python", "rust"):
            root, home = self.fixture(name)
            backend = self.start(name, root, home)
            for index, (mode, stream, tier, tokens) in enumerate((
                ("external", False, "default", (1000, 800, 100, 60)),
                ("external", True, "priority", (1000, 800, 100, 60)),
                ("native", False, "default", (201000, 1000, 10, 0)),
                ("native", True, "default", (1000, 800, 100, 60)),
            )):
                total, cached, output, reasoning = tokens
                payload = {"id": "resp-usage-" + str(index), "object": "response", "model": "upstream-model",
                           "status": "completed", "output": [], "service_tier": tier,
                           "usage": {"input_tokens": total, "output_tokens": output,
                                     "input_tokens_details": {"cached_tokens": cached},
                                     "output_tokens_details": {"reasoning_tokens": reasoning}}}
                for _repeat in range(2):
                    wire = payload
                    if stream:
                        wire = ("event: response.completed\ndata: " + json.dumps({"type": "response.completed", "response": payload}) + "\n\n").encode()
                    self.upstream.configure(wire, content_type="text/event-stream" if stream else "application/json")
                    status, _, raw = backend.request("POST", "/v1/responses", {
                        "model": mode + "/alias", "input": "PRIVATE PROMPT", "stream": stream, "service_tier": tier},
                        headers={"chatgpt-account-id": "e2e-owner"})
                    self.assertEqual(status, 200, raw)
                    self.upstream.requests.get(timeout=5)
            before = self.query(backend)
            self.assertEqual(before["totals"]["requests"], 4, before)
            self.assertEqual(before["totals"]["cost_nanos"], 805_960_000, before)
            backend.close()
            backend = self.start(name, root, home)
            after = self.query(backend)
            self.assertEqual(before, after)
            outcomes.append(self.normalize_live_times(after))
            raw = (root / "state/usage.sqlite3").read_bytes()
            for secret in (b"PRIVATE PROMPT", b"e2e-provider-token", b"e2e-native-token", b"e2e-owner"):
                self.assertNotIn(secret, raw)
        self.assertEqual(outcomes[0], outcomes[1])

    def test_rollout_append_archive_and_realtime_reconciliation(self):
        outcomes = []
        first = header("external/alias") + [usage(), usage(second=2)]
        pending = json.dumps(usage(120, 3, cumulative={"input_tokens": 220, "output_tokens": 40}))
        for name in ("python", "rust"):
            root, home = self.fixture(name, rollout="".join(json.dumps(row) + "\n" for row in first) + pending[:30])
            backend = self.start(name, root, home)
            initial = self.query(backend)
            self.assertEqual(initial["totals"]["input_tokens"], 100)
            with (home / "sessions/rollout.jsonl").open("a") as output:
                output.write(pending[30:] + "\n")
            appended = self.query(backend, scan=True)
            self.assertEqual(appended["totals"]["input_tokens"], 220)
            (home / "archived_sessions").mkdir()
            shutil.copy2(home / "sessions/rollout.jsonl", home / "archived_sessions/copy.jsonl")
            archived = self.query(backend, scan=True)
            self.assertEqual(archived["totals"], appended["totals"])
            self.upstream.configure({"id": "reconciled-response", "status": "completed", "output": [],
                "model": "upstream-model", "usage": {"input_tokens": 100, "output_tokens": 20,
                "input_tokens_details": {"cached_tokens": 40}, "output_tokens_details": {"reasoning_tokens": 10}}})
            status, _, raw = backend.request("POST", "/v1/responses", {
                "model": "external/alias", "input": "hello", "stream": False},
                headers={"x-codex-turn-metadata": json.dumps({"turn_id": TURN})})
            self.assertEqual(status, 200, raw)
            self.upstream.requests.get(timeout=5)
            reconciled = self.query(backend)
            self.assertEqual(reconciled["totals"]["input_tokens"], 220, reconciled)
            outcomes.append((initial, appended, archived, self.normalize_live_times(reconciled)))
        self.assertEqual(outcomes[0], outcomes[1])

    def test_compaction_and_failed_requests_keep_unknown_costs(self):
        outcomes = []
        for name in ("python", "rust"):
            root, home = self.fixture(name)
            backend = self.start(name, root, home)
            exchanges = []
            for mode in ("native", "external"):
                self.upstream.configure({"id": "compact-" + mode, "object": "response", "status": "completed",
                    "model": "upstream-model", "output": [{"type": "message", "role": "assistant", "content": [
                        {"type": "output_text", "text": "Keep this summary"}]}],
                    "usage": {"input_tokens": 1000, "output_tokens": 100,
                              "input_tokens_details": {"cached_tokens": 800},
                              "output_tokens_details": {"reasoning_tokens": 60}}})
                status, _, raw = backend.request("POST", "/v1/responses/compact", {
                    "model": mode + "/alias", "input": "summarize", "stream": False})
                self.assertEqual(status, 200, raw)
                self.upstream.requests.get(timeout=5)
                for stream in (False, True):
                    self.upstream.configure({"error": {"message": "quota exhausted", "type": "rate_limit_exceeded"}}, 429)
                    status, _, raw = backend.request("POST", "/v1/responses", {
                        "model": mode + "/alias", "input": "hello", "stream": stream})
                    requests = []
                    while not self.upstream.requests.empty():
                        requests.append(self.upstream.requests.get(timeout=5)[0])
                    exchanges.append((status, json.loads(raw), requests))
            outcome = self.query(backend)
            self.assertEqual(outcome["totals"]["requests"], 6, outcome)
            self.assertEqual(outcome["totals"]["priced_requests"], 1, outcome)
            outcomes.append((exchanges, self.normalize_live_times(outcome)))
        self.assertEqual(outcomes[0], outcomes[1])


if __name__ == "__main__":
    unittest.main()
