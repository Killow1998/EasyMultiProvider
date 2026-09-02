import gzip
import io
import json
import unittest
import zlib
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from contextlib import redirect_stdout
from http.client import HTTPConnection

import zstandard
import websocket

from easy_multi_provider.request_limits import RequestLimits
from easy_multi_provider.transport import decode_content, RequestBodyTooLarge, WebSocketConnection
from tests.test_request_limits import serving, post, frame


class AdaptiveRequestLimitTests(unittest.TestCase):
    def limits(self, available=16384, maximum=256):
        return RequestLimits(baseline=64, maximum=maximum, available_memory=lambda: available)

    def test_growth_is_per_request_and_reservations_are_released(self):
        limits = self.limits()
        first = limits.request("http")
        first.ensure(64)
        self.assertEqual(limits.snapshot()["notices"], [])
        first.ensure(65)
        self.assertEqual(first.limit, 128)
        first.ensure(129)
        self.assertEqual(first.limit, 256)
        self.assertEqual(limits.reserved, 256 * 8)
        self.assertEqual([n["limit_bytes"] for n in limits.snapshot()["notices"]], [128, 256])
        self.assertEqual(limits.request("http").limit, 64)
        first.release()
        first.release()
        self.assertEqual(limits.reserved, 0)

    def test_concurrent_allowances_cannot_spend_the_same_memory(self):
        limits = self.limits(available=4096)
        first, second, third = [limits.request("http") for _ in range(3)]
        first.ensure(65)
        second.ensure(65)
        with self.assertRaises(RequestBodyTooLarge) as caught:
            third.ensure(65)
        self.assertEqual(caught.exception.reason, "memory_limit")
        self.assertEqual(third.limit, 64)
        self.assertEqual(limits.reserved, 2048)
        first.release()
        third.ensure(65)
        second.release()
        third.release()
        self.assertEqual(limits.reserved, 0)

    def test_hard_limit_and_missing_memory_data_fail_closed(self):
        limits = self.limits()
        budget = limits.request("http")
        with self.assertRaises(RequestBodyTooLarge) as caught:
            budget.ensure(257)
        self.assertEqual(caught.exception.reason, "hard_limit")
        limits.available_memory = lambda: (_ for _ in ()).throw(OSError("unavailable"))
        with self.assertRaises(RequestBodyTooLarge) as caught:
            budget.ensure(65)
        self.assertEqual(caught.exception.reason, "memory_limit")
        self.assertEqual(limits.reserved, 0)

    def test_parallel_admission_reserves_atomically(self):
        limits = self.limits(available=4096)
        limits.available_memory = lambda: (time.sleep(.005), 4096)[1]
        start = threading.Barrier(8)
        budgets = [limits.request("http") for _ in range(8)]

        def enter(budget):
            start.wait(timeout=5)
            try:
                budget.ensure(65)
                return True
            except RequestBodyTooLarge:
                return False

        with redirect_stdout(io.StringIO()), ThreadPoolExecutor(max_workers=8) as executor:
            admitted = list(executor.map(enter, budgets))
        self.assertEqual(sum(admitted), 2)
        for budget in budgets:
            budget.release()
        self.assertEqual(limits.reserved, 0)

    def test_each_supported_compression_expands_without_replaying_or_truncating(self):
        raw = b"preserve-history-" * 10
        for encoding, encode in (
            ("identity", lambda body: body), ("gzip", gzip.compress),
            ("deflate", zlib.compress), ("zstd", zstandard.ZstdCompressor().compress),
            ("gzip, zstd", lambda body: zstandard.ZstdCompressor().compress(gzip.compress(body))),
        ):
            with self.subTest(encoding=encoding):
                limits = self.limits()
                budget = limits.request("http")
                self.assertEqual(decode_content(encode(raw), encoding, 64, budget=budget), raw)
                self.assertEqual(budget.limit, 256)
                budget.release()
                self.assertEqual(limits.reserved, 0)

    def test_websocket_fragments_grow_under_the_same_budget(self):
        limits = self.limits()
        budget = limits.request("websocket")
        wire = frame(b"a" * 60, final=False) + frame(b"ping", opcode=9) + frame(b"b" * 70, opcode=0)
        connection = WebSocketConnection(io.BytesIO(wire), io.BytesIO())
        self.assertEqual(connection.receive_text(budget=budget), "a" * 60 + "b" * 70)
        self.assertEqual(budget.limit, 256)
        budget.release()

    def test_active_frame_timeout_does_not_apply_to_idle_ping(self):
        changes = []
        wire = frame(b"ping", opcode=9) + frame(b"hello")
        connection = WebSocketConnection(io.BytesIO(wire), io.BytesIO(), changes.append)
        self.assertEqual(connection.receive_text(), "hello")
        self.assertEqual(changes, [30, None])
        changes.clear()
        connection = WebSocketConnection(io.BytesIO(frame(b"abc")[:-1]), io.BytesIO(), changes.append)
        with self.assertRaises(EOFError):
            connection.receive_text()
        self.assertEqual(changes, [30, None])

    def test_http_releases_after_success_and_invalid_json_and_management_stays_fixed(self):
        calls = []

        def route(body, headers, **kwargs):
            calls.append(body["input"])
            return {"kind": "body", "status": 200}, b"{}"

        with serving(route) as (address, state):
            limits = state.request_limits = self.limits()
            for raw, status in ((json.dumps({"input": "x" * 100}).encode(), 200), (b"x" * 100, 400)):
                self.assertEqual(post(address, "/v1/responses", gzip.compress(raw), "gzip")[0], status)
            deadline = time.monotonic() + 2
            while limits.reserved and time.monotonic() < deadline:
                time.sleep(.001)
            connection = HTTPConnection(*address)
            connection.request("GET", "/api/request-limits", headers={"Cookie": "emp_session=request-limit-test"})
            response = connection.getresponse()
            report = json.loads(response.read())
            connection.close()
            self.assertEqual(response.status, 200)
            self.assertEqual(report["reserved_bytes"], 0)
            self.assertEqual(len(calls), 1)
            self.assertNotIn("x" * 100, json.dumps(report))
            connection = HTTPConnection(*address)
            connection.request("GET", "/api/request-limits")
            response = connection.getresponse()
            self.assertEqual(response.status, 401)
            response.read()
            connection.close()

    def test_websocket_releases_reservation_before_waiting_for_next_turn(self):
        def route(*args, **kwargs):
            return {"kind": "body", "status": 200}, b'{"id":"test-response","status":"completed","output":[]}'

        with serving(route) as (address, state):
            limits = state.request_limits = self.limits(maximum=512)
            client = websocket.create_connection("ws://127.0.0.1:%d/v1/responses" % address[1],
                header={"Cookie": "emp_session=request-limit-test"}, suppress_origin=True,
                http_no_proxy=["127.0.0.1"], timeout=5)
            try:
                client.send(json.dumps({"type": "response.create", "model": "test", "input": "x" * 100}))
                while True:
                    event = json.loads(client.recv())
                    self.assertNotIn(event.get("type"), ("error", "response.failed"), event)
                    if event.get("type") == "response.completed":
                        break
                deadline = time.monotonic() + 2
                while limits.reserved and time.monotonic() < deadline:
                    time.sleep(.001)
                self.assertEqual(limits.reserved, 0)
                self.assertEqual(limits.snapshot()["notices"][-1]["kind"], "expanded")
            finally:
                client.close()

    def test_notice_history_is_bounded_and_console_contains_no_request_content(self):
        limits = self.limits()
        output = io.StringIO()
        with redirect_stdout(output):
            for _ in range(25):
                budget = limits.request("http")
                budget.ensure(65)
                budget.release()
        self.assertEqual(len(limits.snapshot()["notices"]), 20)
        self.assertIn("expanded", output.getvalue())
        self.assertEqual(set(limits.snapshot()["notices"][0]), {
            "id", "timestamp", "kind", "transport", "previous_bytes", "limit_bytes", "reason"
        })


if __name__ == "__main__":
    unittest.main()
