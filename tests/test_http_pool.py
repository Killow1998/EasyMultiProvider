import threading
import json
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.error import URLError
from urllib.request import Request
from unittest.mock import patch

from easy_multi_provider import http_pool
from easy_multi_provider.router import _bounded_stream_response, _DeadlineResponse, _validated_responses_stream
from easy_multi_provider.stream_adapters import _reliable_responses_stream


class HTTPPoolTests(unittest.TestCase):
    def setUp(self):
        self.requests = []
        self.finish = threading.Event()
        owner = self

        class Handler(BaseHTTPRequestHandler):
            protocol_version = "HTTP/1.1"

            def log_message(self, *args):
                pass

            def handle(self):
                try:
                    super().handle()
                except ConnectionError:
                    pass  # Cancellation deliberately closes unread responses.

            def do_POST(self):
                self.rfile.read(int(self.headers.get("Content-Length", 0)))
                self.do_GET()

            def do_GET(self):
                owner.requests.append((self.client_address[1], self.path,
                                       self.headers.get("Authorization")))
                if self.path == "/drop":
                    self.close_connection = True
                    return
                if self.path == "/responses":
                    self.send_response(200)
                    self.send_header("Content-Type", "text/event-stream")
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    event = {"type": "response.completed", "response": {
                        "id": "resp_test", "object": "response", "status": "completed",
                        "output": [{"id": "msg_test", "type": "message", "role": "assistant",
                                    "status": "completed", "content": [{"type": "output_text", "text": "OK"}]}],
                    }}
                    data = ("data: " + json.dumps(event) + "\n\n").encode()
                    self.wfile.write(("%x\r\n" % len(data)).encode() + data + b"\r\n0\r\n\r\n")
                    self.wfile.flush()
                    return
                if self.path == "/redirect":
                    self.send_response(302)
                    self.send_header("Location", "/unexpected")
                    self.send_header("Content-Length", "0")
                    self.end_headers()
                    return
                if self.path == "/stream":
                    self.send_response(200)
                    self.send_header("Transfer-Encoding", "chunked")
                    self.end_headers()
                    data = b"data: first\n\n"
                    self.wfile.write(("%x\r\n" % len(data)).encode() + data + b"\r\n")
                    self.wfile.flush()
                    owner.finish.wait(3)
                    try:
                        self.wfile.write(b"0\r\n\r\n")
                        self.wfile.flush()
                    except OSError:
                        pass
                    return
                self.send_response(200)
                self.send_header("Content-Length", "2")
                self.end_headers()
                self.wfile.write(b"OK")

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()
        self.url = "http://127.0.0.1:%d" % self.server.server_port
        http_pool.close_pools()

    def tearDown(self):
        self.finish.set()
        http_pool.close_pools()
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(2)

    def open(self, path="/", **kwargs):
        return http_pool.open_request(Request(self.url + path, **kwargs), timeout=2)

    def test_complete_responses_reuse_socket_without_sharing_authorization(self):
        for key in ("Bearer first", "Bearer second"):
            with self.open(headers={"Authorization": key}) as response:
                self.assertEqual(response.read(), b"OK")
        self.assertEqual(self.requests[0][0], self.requests[1][0])
        self.assertEqual([r[2] for r in self.requests], ["Bearer first", "Bearer second"])

    def test_first_sse_line_does_not_wait_for_end_or_full_buffer(self):
        with self.open("/stream") as response, ThreadPoolExecutor(1) as executor:
            pending = executor.submit(next, response)
            try:
                self.assertEqual(pending.result(timeout=1), b"data: first\n")
            finally:
                self.finish.set()
            self.assertEqual(list(response), [b"\n"])
        with self.open() as response:
            response.read()
        self.assertEqual(self.requests[0][0], self.requests[1][0])

    def test_cancelled_stream_is_not_reused(self):
        response = self.open("/stream")
        self.assertEqual(next(response), b"data: first\n")
        response.close()
        self.finish.set()
        with self.open() as response:
            response.read()
        self.assertNotEqual(self.requests[0][0], self.requests[1][0])

    def test_terminal_cleanup_does_not_wait_for_a_peer_that_keeps_body_open(self):
        response = self.open("/stream")
        self.assertEqual(next(response), b"data: first\n")
        with ThreadPoolExecutor(1) as executor:
            try:
                executor.submit(response.finish).result(timeout=1)
                self.assertFalse(self.finish.is_set())
            finally:
                self.finish.set()
        with self.open() as response:
            response.read()
        self.assertNotEqual(self.requests[0][0], self.requests[1][0])

    def test_completed_responses_reuse_through_router_stream_wrappers(self):
        for _ in range(2):
            response = _bounded_stream_response(_DeadlineResponse(self.open("/responses"), None))
            chunks = list(_reliable_responses_stream(lambda: _validated_responses_stream(response)))
            self.assertIn(b"response.completed", b"".join(chunks))
        self.assertEqual(self.requests[0][0], self.requests[1][0])

    def test_post_is_not_replayed_and_redirect_does_not_forward_credentials(self):
        with self.assertRaises(URLError):
            self.open("/drop", data=b"one request")
        with self.assertRaises(URLError):
            self.open("/redirect", headers={"Authorization": "Bearer private"})
        self.assertEqual([r[1] for r in self.requests], ["/drop", "/redirect"])

    def test_new_proxy_settings_select_another_pool(self):
        with patch.object(http_pool, "proxy_for_url", return_value=None):
            with self.open() as response:
                response.read()
        with patch.object(http_pool, "proxy_for_url", return_value=self.url):
            with self.open() as response:
                response.read()
        self.assertNotEqual(self.requests[0][0], self.requests[1][0])
