import gzip
import io
import json
import socket
import struct
import threading
import unittest
from contextlib import contextmanager
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from types import SimpleNamespace
from unittest.mock import patch

import zstandard

from easy_multi_provider import server, transport


MiB = 1024 * 1024


def frame(payload, opcode=1, final=True):
    # A zero mask is valid and keeps large synthetic test frames inexpensive.
    head = bytes([(0x80 if final else 0) | opcode])
    if len(payload) < 126:
        head += bytes([0x80 | len(payload)])
    else:
        head += b"\xff" + struct.pack("!Q", len(payload))
    return head + b"\0\0\0\0" + payload


class Journal:
    def __init__(self):
        self.events = []

    def event(self, level, name, **fields):
        self.events.append((name, fields))

    def exception_event(self, *args):
        raise AssertionError("unexpected server exception")


@contextmanager
def serving(route):
    state = SimpleNamespace(
        mark_service_ready=lambda: None,
        session_token="request-limit-test",
        journal=Journal(),
        codex=SimpleNamespace(
            route=route, route_compact=route,
            prepare_native_websocket=lambda *args, **kwargs: (None, 0, 0),
        ),
    )
    handler = server.make_handler(state)
    handler.log_message = lambda *args: None
    service = ThreadingHTTPServer(("127.0.0.1", 0), handler)
    service.daemon_threads = True
    worker = threading.Thread(target=service.serve_forever, daemon=True)
    worker.start()
    try:
        yield service.server_address, state
    finally:
        service.shutdown()
        service.server_close()
        worker.join(timeout=2)


def post(address, path, body, encoding="identity", authenticated=True):
    connection = HTTPConnection(*address, timeout=15)
    headers = {"Content-Type": "application/json", "Content-Encoding": encoding}
    if authenticated:
        headers["Cookie"] = "emp_session=request-limit-test"
    try:
        connection.request("POST", path, body, headers)
        response = connection.getresponse()
        return response.status, json.loads(response.read()), response.getheader("Connection")
    finally:
        connection.close()


class RequestLimitTests(unittest.TestCase):
    def test_idle_websockets_leave_capacity_for_health_requests(self):
        class TinyBoundedServer(server.BoundedThreadingHTTPServer):
            request_limit = 4
            request_limit_max = 4
            websocket_limit = 2
            websocket_limit_max = 2

        state = SimpleNamespace(
            mark_service_ready=lambda: None,
            session_token="request-limit-test",
            journal=Journal(),
            codex=SimpleNamespace(
                route=lambda *args, **kwargs: self.fail("idle websocket must not route"),
                prepare_native_websocket=lambda *args, **kwargs: (None, 0, 0),
            ),
        )
        handler = server.make_handler(state)
        handler.log_message = lambda *args: None
        service = TinyBoundedServer(("127.0.0.1", 0), handler)
        worker = threading.Thread(target=service.serve_forever, daemon=True)
        worker.start()
        clients = []

        def open_websocket():
            client = socket.create_connection(service.server_address, timeout=3)
            reader = client.makefile("rb")
            client.sendall((
                "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n"
                "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                "Sec-WebSocket-Version: 13\r\n"
                "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                "Cookie: emp_session=request-limit-test\r\n\r\n"
            ).encode() % service.server_address[1])
            self.assertIn(b"101", reader.readline())
            while reader.readline() not in (b"\r\n", b""):
                pass
            clients.append((client, reader))
            return client, reader

        try:
            open_websocket()
            open_websocket()

            connection = HTTPConnection(*service.server_address, timeout=3)
            try:
                connection.request("GET", "/healthz")
                response = connection.getresponse()
                self.assertEqual(response.status, 200)
                self.assertEqual(json.loads(response.read()), {"status": "ok"})
            finally:
                connection.close()

            _overflow, overflow_reader = open_websocket()
            first, length = overflow_reader.read(2)
            self.assertEqual(first, 0x88)
            close_payload = overflow_reader.read(length)
            self.assertEqual(struct.unpack("!H", close_payload[:2])[0], 1013)
        finally:
            for client, reader in clients:
                reader.close()
                client.close()
            service.shutdown()
            service.server_close()
            worker.join(timeout=2)

    def test_listener_slots_expand_under_pressure_until_the_hard_cap(self):
        expansions = []
        slots = server._AdaptiveSlotPool(
            initial=2,
            maximum=4,
            growth=2,
            on_expand=lambda previous, current: expansions.append(
                (previous, current)
            ),
        )
        self.assertTrue(slots.acquire())
        self.assertTrue(slots.acquire())
        self.assertTrue(slots.acquire())
        self.assertTrue(slots.acquire())
        self.assertFalse(slots.acquire())
        self.assertEqual(expansions, [(2, 4)])
        self.assertEqual(
            slots.snapshot(),
            {"active": 4, "capacity": 4, "maximum": 4},
        )
        for _ in range(4):
            slots.release()

        def broken_reporter(*_args):
            raise OSError("journal")

        diagnostic_failure = server._AdaptiveSlotPool(
            initial=1,
            maximum=2,
            growth=1,
            on_expand=broken_reporter,
        )
        self.assertTrue(diagnostic_failure.acquire())
        self.assertTrue(diagnostic_failure.acquire())
        diagnostic_failure.release()
        diagnostic_failure.release()

    def test_large_compressed_history_reaches_responses_and_compaction_unchanged(self):
        # This is larger than the former 32 MiB decoded-body limit.
        raw = b'{"model":"test/model","input":"' + b"x" * (40 * MiB) + b'TAIL"}'
        encoded = zstandard.ZstdCompressor().compress(raw)
        del raw
        counts = []

        def route(body, headers, **kwargs):
            self.assertEqual(len(body["input"]), 40 * MiB + 4)
            self.assertTrue(body["input"].startswith("xxxx"))
            self.assertTrue(body["input"].endswith("TAIL"))
            counts.append(len(body["input"]))
            return {"kind": "body", "status": 200}, b'{"output":[]}'

        with serving(route) as (address, state):
            for path in ("/v1/responses", "/v1/responses/compact"):
                with self.subTest(path=path):
                    self.assertEqual(post(address, path, encoded, "zstd")[0], 200)
        self.assertEqual(counts, [40 * MiB + 4] * 2)

    def test_large_websocket_history_is_not_limited_by_response_frame_size(self):
        payload = b"x" * (20 * MiB) + b"TAIL"
        connection = transport.WebSocketConnection(io.BytesIO(frame(payload)), io.BytesIO())
        text = connection.receive_text()
        self.assertEqual(len(text), len(payload))
        self.assertTrue(text.endswith("TAIL"))

    def test_decode_limit_accepts_exact_boundary_and_rejects_expansion(self):
        for encoding, encode in (
            ("identity", lambda value: value),
            ("gzip", gzip.compress),
            ("zstd", zstandard.ZstdCompressor().compress),
        ):
            with self.subTest(encoding=encoding):
                self.assertEqual(transport.decode_content(encode(b"x" * 64), encoding, 64), b"x" * 64)
                with self.assertRaises(transport.RequestBodyTooLarge):
                    transport.decode_content(encode(b"x" * 65), encoding, 64)

    def test_http_expansion_returns_413_and_content_free_diagnostics(self):
        def forbidden(*args, **kwargs):
            self.fail("oversized body must not reach routing")

        with serving(forbidden) as (address, state), patch.object(server, "MAX_PROXY_REQUEST_BYTES", 64):
            for encoding, encode in (("gzip", gzip.compress), ("zstd", zstandard.ZstdCompressor().compress)):
                with self.subTest(encoding=encoding):
                    status, body, connection = post(address, "/v1/responses", encode(b"sensitive-history" * 8), encoding)
                    self.assertEqual(status, 413)
                    self.assertEqual(body["error"]["code"], "request_too_large")
                    self.assertEqual(body["error"]["limit_bytes"], 64)
                    self.assertEqual(connection, "close")
            rejected = [fields for name, fields in state.journal.events if name == "request_rejected"]
            self.assertEqual(len(rejected), 2)
            self.assertTrue(all(item["reason"] == "decoded_body_too_large" for item in rejected))
            self.assertNotIn("sensitive-history", json.dumps(state.journal.events))

    def test_http_wire_limit_is_checked_before_reading_and_auth_still_runs_first(self):
        with serving(lambda *args, **kwargs: self.fail("must not route")) as (address, state), patch.object(server, "MAX_PROXY_REQUEST_BYTES", 64):
            for authenticated, expected in ((False, 401), (True, 413)):
                connection = HTTPConnection(*address, timeout=3)
                headers = {"Content-Type": "application/json", "Content-Length": "65"}
                if authenticated:
                    headers["Cookie"] = "emp_session=request-limit-test"
                try:
                    connection.request("POST", "/v1/responses", headers=headers)
                    response = connection.getresponse()
                    self.assertEqual(response.status, expected)
                    response.read()
                finally:
                    connection.close()

    def test_websocket_limit_counts_fragments_but_not_ping_frames(self):
        wire = frame(b"a" * 64, final=False) + frame(b"ping", opcode=9) + frame(b"", opcode=0)
        with patch.object(transport, "MAX_PROXY_REQUEST_BYTES", 64):
            connection = transport.WebSocketConnection(io.BytesIO(wire), io.BytesIO())
            self.assertEqual(connection.receive_text(), "a" * 64)
            overflow = frame(b"a" * 32, final=False) + frame(b"b" * 33, opcode=0)
            connection = transport.WebSocketConnection(io.BytesIO(overflow), io.BytesIO())
            with self.assertRaises(transport.WebSocketRequestTooLarge) as caught:
                connection.receive_text()
            self.assertEqual(caught.exception.code, 1009)

    def test_websocket_nonzero_mask_round_trips_without_content_changes(self):
        payload = (b"masked-history-" * 4096) + b"TAIL"
        mask = b"\x01\x7f\x80\xff"
        encoded = bytes(value ^ mask[index % 4] for index, value in enumerate(payload))
        head = b"\x81\xff" + struct.pack("!Q", len(payload)) + mask
        connection = transport.WebSocketConnection(
            io.BytesIO(head + encoded), io.BytesIO()
        )
        self.assertEqual(connection.receive_text().encode(), payload)

    def test_websocket_overflow_sends_explicit_error_and_close_code(self):
        with serving(lambda *args, **kwargs: self.fail("must not route")) as (address, state), patch.object(transport, "MAX_PROXY_REQUEST_BYTES", 64):
            with socket.create_connection(address, timeout=3) as client, client.makefile("rb") as reader:
                client.sendall((
                    "GET /v1/responses HTTP/1.1\r\nHost: 127.0.0.1:%d\r\n"
                    "Upgrade: websocket\r\nConnection: Upgrade\r\n"
                    "Sec-WebSocket-Version: 13\r\n"
                    "Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n"
                    "Cookie: emp_session=request-limit-test\r\n\r\n"
                ).encode() % address[1])
                self.assertIn(b"101", reader.readline())
                while reader.readline() not in (b"\r\n", b""):
                    pass
                # The length header alone must reject an oversized frame.
                client.sendall(b"\x81\xc1")
                first, length = reader.read(2)
                self.assertEqual(first, 0x81)
                if length == 126:
                    length = struct.unpack("!H", reader.read(2))[0]
                error = json.loads(reader.read(length))
                self.assertEqual(error["status"], 413)
                self.assertEqual(error["error"]["code"], "request_too_large")
                first, length = reader.read(2)
                self.assertEqual(first, 0x88)
                self.assertEqual(struct.unpack("!H", reader.read(length)[:2])[0], 1009)
            rejected = [f for name, f in state.journal.events if name == "request_rejected"]
            self.assertEqual(rejected[0]["reason"], "websocket_message_too_large")

    def test_response_frame_limit_and_unsupported_encoding_are_unchanged(self):
        with patch.object(transport, "MAX_WEBSOCKET_MESSAGE_BYTES", 64):
            connection = transport.WebSocketConnection(io.BytesIO(), io.BytesIO())
            with self.assertRaises(transport.WebSocketProtocolError) as caught:
                connection.send_json({"text": "x" * 65})
            self.assertNotIsInstance(caught.exception, transport.WebSocketRequestTooLarge)
        with serving(lambda *args, **kwargs: self.fail("must not route")) as (address, _):
            self.assertEqual(post(address, "/v1/responses", b"{}", "unsupported")[0], 400)

    def test_management_body_limit_is_not_increased(self):
        with serving(lambda *args, **kwargs: self.fail("must not route")) as (address, _):
            status, body, _ = post(address, "/api/config", zstandard.ZstdCompressor().compress(b"x" * (5 * MiB + 1)), "zstd")
            self.assertEqual(status, 413)
            self.assertEqual(body["error"]["limit_bytes"], 5 * MiB)


if __name__ == "__main__":
    unittest.main()
