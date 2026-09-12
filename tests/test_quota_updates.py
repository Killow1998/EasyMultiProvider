"""Quota refreshes and browser notifications without real upstream requests."""

import json
import tempfile
import threading
import time
import unittest
from http.client import HTTPConnection
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.config import normalize, save
from easy_multi_provider.quota import QuotaError
from easy_multi_provider.server import AppState, BoundedThreadingHTTPServer, make_handler
from tests.support import ensure_test_master_key


ensure_test_master_key()


class QuotaUpdatesTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        self.config_path = root / "config.json"
        save(normalize({"account_store_path": str(root / "accounts")}), self.config_path)
        self.state = AppState(self.config_path, runtime_controller=object())
        self.state.codex_home = root / "native"
        self.connections = []
        self.server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(self.state))
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.addCleanup(self.stop_server)

    def stop_server(self):
        self.state.stop_quota_sampler()
        for connection in self.connections:
            connection.close()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(3)

    def get(self, path, authenticated=True, origin=None):
        connection = HTTPConnection(*self.server.server_address, timeout=3)
        self.connections.append(connection)
        headers = {"Cookie": "emp_session=" + self.state.session_token} if authenticated else {}
        if origin:
            headers["Origin"] = origin
        connection.request("GET", path, headers=headers)
        return connection.getresponse()

    def read_event(self, response):
        lines = []
        for _ in range(4):
            line = response.readline()
            if line in (b"\n", b""):
                break
            lines.append(line)
        return b"".join(lines)

    def test_stream_requires_session_and_same_origin(self):
        denied = self.get("/api/accounts/events", authenticated=False)
        self.assertEqual(denied.status, 401)
        denied.read()
        cross_origin = self.get("/api/accounts/events", origin="https://example.invalid")
        self.assertEqual(cross_origin.status, 403)
        cross_origin.read()

    def test_stream_notifies_after_refresh_and_reconnect_catches_up(self):
        response = self.get("/api/accounts/events")
        self.assertEqual(response.status, 200)
        self.assertEqual(response.getheader("Content-Type"), "text/event-stream")
        self.assertIn(b"quota-updated", self.read_event(response))
        quota = {"updated_at": 123, "rate_limits": {"primary": {"usedPercent": 25}}}
        with patch("easy_multi_provider.server.read_native_login_quota", return_value=quota):
            self.state.refresh_native_account()
        event = self.read_event(response)
        self.assertEqual(event, b"event: quota-updated\ndata: {}\n")
        self.assertNotIn(self.state.session_token.encode(), event)
        listing = self.get("/api/accounts")
        self.assertEqual(json.loads(listing.read())["native_account"]["quota"], quota)
        reopened = self.get("/api/accounts/events")
        self.assertIn(b"quota-updated", self.read_event(reopened))

    def test_failures_are_reported_by_code_then_cleared_on_success(self):
        with patch("easy_multi_provider.server.read_native_login_quota", side_effect=QuotaError(
            "private upstream response must not be exposed", "quota_auth_required"
        )):
            with self.assertRaises(QuotaError):
                self.state.refresh_native_account()
        response = self.get("/api/accounts")
        raw = response.read()
        self.assertNotIn(b"private upstream", raw)
        errors = json.loads(raw)["refresh_errors"]
        self.assertIn("quota_auth_required", errors.values())
        with patch("easy_multi_provider.server.read_native_login_quota", return_value={}):
            self.state.refresh_native_account()
        self.assertEqual(self.state.accounts_snapshot()["refresh_errors"], {})

    def test_stream_slots_leave_management_available_and_release_on_shutdown(self):
        self.state._quota_event_slots = threading.BoundedSemaphore(1)
        response = self.get("/api/accounts/events")
        self.read_event(response)
        excess = self.get("/api/accounts/events")
        self.assertEqual(excess.status, 503)
        excess.read()
        normal = self.get("/api/accounts")
        self.assertEqual(normal.status, 200)
        normal.read()
        self.state.stop_quota_sampler()
        self.assertEqual(response.read(), b"")
        self.assertTrue(self.state._quota_event_slots.acquire(timeout=2))

    def test_expired_session_ends_notification_stream(self):
        response = self.get("/api/accounts/events")
        self.read_event(response)
        self.state.session_expires_at = time.time() - 1
        self.state.notify_quota_update("native")
        self.assertEqual(response.read(), b"")

    def test_background_sampling_does_not_wait_for_slow_account(self):
        self.state.config["accounts"] = [
            {"id": "slow", "auth_file": "test-slow.enc"},
            {"id": "fast", "auth_file": "test-fast.enc"},
        ]
        slow_entered, fast_finished, release = (threading.Event() for _ in range(3))
        result = []

        def refresh(account_id):
            if account_id == "slow":
                slow_entered.set()
                if not release.wait(3):
                    raise AssertionError("slow quota was not released")
            else:
                fast_finished.set()

        with patch.object(self.state, "refresh_account", side_effect=refresh), patch(
            "easy_multi_provider.server.duplicate_account_status", return_value={}
        ):
            thread = threading.Thread(target=lambda: result.append(self.state.sample_quotas_once()))
            thread.start()
            try:
                self.assertTrue(slow_entered.wait(2))
                self.assertTrue(fast_finished.wait(2))
            finally:
                release.set()
                thread.join(3)
        self.assertEqual(result, [{"sampled": 2, "failed": 0}])
