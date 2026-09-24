import base64
import hashlib
import importlib.util
from pathlib import Path
import unittest
from unittest.mock import patch


SCRIPT = Path(__file__).resolve().parents[1] / "tools" / "benchmark_emp_idle_websockets.py"
SPEC = importlib.util.spec_from_file_location("benchmark_emp_idle_websockets", SCRIPT)
idle_ws = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(idle_ws)


class BenchmarkIdleWebSocketTests(unittest.TestCase):
    class FakeSocket:
        def __init__(self, response):
            self.response = response
            self.request = b""
            self.closed = False

        def settimeout(self, _timeout):
            pass

        def sendall(self, request):
            self.request += request

        def recv(self, size):
            result, self.response = self.response[:size], self.response[size:]
            return result

        def close(self):
            self.closed = True

    def _handshake_response(self, status=101, valid_accept=True,
                            connection_header="keep-alive, uPgRaDe"):
        if status != 101:
            return f"HTTP/1.1 {status} Forbidden\r\nContent-Length: 0\r\n\r\n".encode()
        accept = base64.b64encode(hashlib.sha1(
            (idle_ws.WEBSOCKET_KEY +
             "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
        ).digest())
        if not valid_accept:
            accept = b"invalid"
        return (
            b"HTTP/1.1 101 Switching Protocols\r\n"
            b"Upgrade: WebSocket\r\n"
            + f"Connection: {connection_header}\r\n".encode("ascii")
            + b"Sec-WebSocket-Accept: " + accept + b"\r\n\r\n"
        )

    def test_valid_101_accepts_mixed_case_connection_tokens_and_keeps_socket(self):
        fake = self.FakeSocket(self._handshake_response())
        with patch.object(idle_ws.socket, "create_connection", return_value=fake):
            connection, status, error = idle_ws._websocket_handshake(4200, "emp_session=fixture")
        self.assertIsNotNone(connection)
        self.assertIs(connection, fake)
        self.assertEqual(status, 101)
        self.assertIsNone(error)
        self.assertIn(b"GET /v1/responses HTTP/1.1", fake.request)
        self.assertIn(b"Cookie: emp_session=fixture", fake.request)
        connection.close()

    def test_non_101_is_a_safe_handshake_failure(self):
        fake = self.FakeSocket(self._handshake_response(status=403))
        with patch.object(idle_ws.socket, "create_connection", return_value=fake):
            connection, status, error = idle_ws._websocket_handshake(4200, "emp_session=fixture")
        self.assertIsNone(connection)
        self.assertEqual(status, 403)
        self.assertEqual(error, "handshake_rejected")
        self.assertTrue(fake.closed)

    def test_invalid_accept_is_not_counted_as_an_admitted_websocket(self):
        fake = self.FakeSocket(self._handshake_response(valid_accept=False))
        with patch.object(idle_ws.socket, "create_connection", return_value=fake):
            connection, status, error = idle_ws._websocket_handshake(4200, "emp_session=fixture")
        self.assertIsNone(connection)
        self.assertEqual(status, 101)
        self.assertEqual(error, "invalid_websocket_handshake")
        self.assertTrue(fake.closed)

    def test_parameter_limits_match_production_gate(self):
        idle_ws.validate_parameters(224, 64, 250, 20)
        for values in ((0, 1, 1, 0), (225, 64, 1, 0), (2, 3, 1, 0), (2, 1, 0, 0)):
            with self.subTest(values=values), self.assertRaises(idle_ws.benchmark.BenchmarkError):
                idle_ws.validate_parameters(*values)

    def test_release_slack_requires_observable_process_metrics(self):
        before = {"descriptors": 10, "threads": 5}
        self.assertTrue(idle_ws.resource_release_ok(
            before, {"descriptors": 18, "threads": 13}
        ))
        self.assertFalse(idle_ws.resource_release_ok(
            before, {"descriptors": 19, "threads": 5}
        ))
        self.assertFalse(idle_ws.resource_release_ok(
            {"descriptors": None, "threads": None},
            {"descriptors": None, "threads": None},
        ))

    def test_latency_report_has_p95_gate_metrics_without_request_data(self):
        summary = idle_ws.percentile_summary([1.0, 2.0, 3.0, 4.0], 2.0, 0)
        self.assertAlmostEqual(summary["p95_ms"], 3.85)
        self.assertAlmostEqual(summary["p99_ms"], 3.97)
        self.assertEqual(summary["max_ms"], 4.0)
        self.assertEqual(summary["errors"], 0)
        self.assertNotIn("emp_session=fixture", repr(summary))


if __name__ == "__main__":
    unittest.main()
