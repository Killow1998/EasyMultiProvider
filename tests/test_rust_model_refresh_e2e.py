"""Subscription model refresh over real HTTP, including in-flight login changes."""
import json
import os
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from tests import test_rust_usage_e2e as usage_cases


@unittest.skipUnless(os.environ.get("EMP_RUST_BINARY"), "set EMP_RUST_BINARY for process E2E")
class RustModelRefreshEndToEnd(unittest.TestCase):
    maxDiff = None
    setUp = usage_cases.RustUsageEndToEnd.setUp
    fixture = usage_cases.RustUsageEndToEnd.fixture
    start = usage_cases.RustUsageEndToEnd.start
    catalog = {"models": [{"slug": "gpt-current", "display_name": "Current", "context_window": 272000,
        "max_context_window": 1000000, "effective_context_window_percent": 95, "auto_compact_token_limit": 244800}]}

    def configure(self, name):
        root, home = self.fixture(name)
        path = root / "config.json"
        config = json.loads(path.read_text())
        config["codex_base_url"] = self.upstream.base_url
        path.write_text(json.dumps(config))
        return root, home, self.start(name, root, home)

    def test_refresh_uses_each_owner_and_preserves_cache_on_failure(self):
        outcomes = []
        for name in ("python", "rust"):
            root, home, backend = self.configure(name)
            status, _, raw = backend.request("POST", "/api/accounts/import", {"id": "a", "prefix": "a",
                "auth_json": {"tokens": {"access_token": "subscription-token", "account_id": "subscription-owner"}}})
            self.assertEqual(status, 200, raw)
            exchanges = []
            for owner in ("@native", "a"):
                self.upstream.configure(self.catalog)
                status, _, raw = backend.request("POST", "/api/accounts/" + owner + "/models/refresh", {})
                self.assertEqual(status, 200, raw)
                path, headers, _ = self.upstream.requests.get(timeout=5)
                headers = {key.lower(): value for key, value in headers.items()}
                exchanges.append((path, {key: headers.get(key) for key in ("authorization", "chatgpt-account-id", "user-agent")}, json.loads(raw)))
                self.assertEqual(backend.request("GET", "/api/accounts/" + owner + "/models")[2], raw)
            self.assertEqual(exchanges[0][1]["authorization"], "Bearer e2e-native-token")
            self.assertEqual(exchanges[1][1]["authorization"], "Bearer subscription-token")
            self.assertEqual(exchanges[1][1]["chatgpt-account-id"], "subscription-owner")
            before = (root / "native.json").read_bytes()
            failures = []
            for status, payload in ((401, {"error": {"message": "PRIVATE AUTH DETAIL"}}),
                                    (200, {"models": [1]}), (200, {"data": []}), (200, b"bad JSON")):
                self.upstream.configure(payload, status)
                actual, _, raw = backend.request("POST", "/api/accounts/@native/models/refresh", {})
                failures.append((actual, json.loads(raw)))
                self.upstream.requests.get(timeout=5)
                self.assertTrue(self.upstream.requests.empty(), "catalog refresh must not replay credentials")
                self.assertNotIn(b"PRIVATE", raw)
                self.assertEqual((root / "native.json").read_bytes(), before)
            self.assertEqual(backend.request("POST", "/api/accounts/a/models/refresh", {}, auth=False)[0], 401)
            self.assertEqual(backend.request("POST", "/api/accounts/absent/models/refresh", {})[0], 400)
            outcomes.append((exchanges, failures))
        self.assertEqual(outcomes[0], outcomes[1])

    def test_refresh_rejects_native_login_replacement_before_write(self):
        outcomes = []
        for name in ("python", "rust"):
            root, home, backend = self.configure(name)
            before = (root / "native.json").read_bytes()
            self.upstream.configure(self.catalog)
            release = threading.Event()
            self.upstream.reply_gate = release
            try:
                with ThreadPoolExecutor(max_workers=1) as worker:
                    pending = worker.submit(backend.request, "POST", "/api/accounts/@native/models/refresh", {})
                    self.upstream.requests.get(timeout=5)
                    (home / "auth.json").write_text(json.dumps({"tokens": {"access_token": "replaced-token", "account_id": "different-owner"}}))
                    release.set()
                    status, _, raw = pending.result(timeout=8)
            finally:
                release.set()
                self.upstream.reply_gate = None
            self.assertEqual(status, 400, raw)
            self.assertEqual((root / "native.json").read_bytes(), before)
            outcomes.append(json.loads(raw))
        self.assertEqual(outcomes[0], outcomes[1])


if __name__ == "__main__":
    unittest.main()
