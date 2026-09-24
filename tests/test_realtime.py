import io
import json
import queue
import tempfile
import threading
import time
import unittest
from http.client import HTTPConnection
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch
from urllib.error import HTTPError

import websocket

from tests.support import ensure_test_master_key
from easy_multi_provider.accounts import AccountError
from easy_multi_provider.config import normalize, save
from easy_multi_provider.diagnostic_journal import NullJournal
from easy_multi_provider.integration import IntegrationManager
from easy_multi_provider.realtime import (
    MAX_REALTIME_REQUEST_BYTES,
    RealtimeCall,
    RealtimeError,
    RealtimeResponse,
    forward_native_realtime_call,
    open_native_realtime_sideband,
    parse_realtime_multipart,
    read_realtime_call,
)
from easy_multi_provider.server import AppState, make_handler


ensure_test_master_key()

BOUNDARY = "codex-realtime-call-boundary"


def multipart_body(sdp="v=0\r\no=offer\r\n", session=None, extra_parts=()):
    session = session or {"model": "gpt-live", "delegation": {"type": "client"}}
    parts = [
        ("sdp", "application/sdp", sdp),
        ("session", "application/json", json.dumps(session, separators=(",", ":"))),
        *extra_parts,
    ]
    body = bytearray()
    for name, content_type, value in parts:
        body.extend(("--%s\r\n" % BOUNDARY).encode("ascii"))
        body.extend(
            ('Content-Disposition: form-data; name="%s"\r\n' % name).encode(
                "ascii"
            )
        )
        body.extend(("Content-Type: %s\r\n\r\n" % content_type).encode("ascii"))
        body.extend(value.encode("utf-8"))
        body.extend(b"\r\n")
    body.extend(("--%s--\r\n" % BOUNDARY).encode("ascii"))
    return bytes(body)


class FakeResponse:
    def __init__(self, status=201, body=b"v=answer\r\n", headers=None):
        self.status = status
        self.body = io.BytesIO(body)
        self.headers = headers or {
            "Content-Type": "application/sdp",
            "Location": "/v1/live/rtc_voice_test",
        }
        self.closed = False

    def read(self, size=-1):
        return self.body.read(size)

    def close(self):
        self.closed = True


class FakeSideband:
    def __init__(self):
        self.incoming = queue.Queue()
        self.incoming.put('{"type":"session.started"}')
        self.sent = []
        self.timeout = 1
        self.closed = False

    def settimeout(self, value):
        self.timeout = value

    def recv(self):
        try:
            return self.incoming.get(timeout=self.timeout)
        except queue.Empty as exc:
            raise TimeoutError from exc

    def send_text(self, value):
        self.sent.append(value)
        if json.loads(value).get("type") == "session.close":
            self.incoming.put('{"type":"session.closed"}')

    def close(self):
        self.closed = True
        self.incoming.put(None)


class CapturingJournal(NullJournal):
    def __init__(self):
        self.events = []

    def event(self, level, event, **fields):
        self.events.append((level, event, fields))


class RealtimeParsingTests(unittest.TestCase):
    def test_parses_exact_two_part_codex_request(self):
        body = multipart_body()
        call = parse_realtime_multipart(body, BOUNDARY)
        self.assertEqual(call.sdp, "v=0\r\no=offer\r\n")
        self.assertEqual(call.session["delegation"], {"type": "client"})

    def test_rejects_wrong_top_level_content_type(self):
        body = multipart_body()
        with self.assertRaises(RealtimeError) as raised:
            read_realtime_call(
                {
                    "Content-Type": "application/json",
                    "Content-Length": str(len(body)),
                },
                io.BytesIO(body),
            )
        self.assertEqual(raised.exception.status, 415)
        self.assertEqual(raised.exception.code, "realtime_invalid_content_type")
        self.assertTrue(raised.exception.close)

    def test_rejects_declared_request_over_limit_before_reading(self):
        stream = io.BytesIO(b"must-not-be-read")
        with self.assertRaises(RealtimeError) as raised:
            read_realtime_call(
                {
                    "Content-Type": "multipart/form-data; boundary=" + BOUNDARY,
                    "Content-Length": str(MAX_REALTIME_REQUEST_BYTES + 1),
                },
                stream,
            )
        self.assertEqual(raised.exception.status, 413)
        self.assertEqual(stream.tell(), 0)

    def test_rejects_extra_or_unknown_multipart_fields(self):
        body = multipart_body(extra_parts=(("token", "text/plain", "secret"),))
        with self.assertRaises(RealtimeError) as raised:
            parse_realtime_multipart(body, BOUNDARY)
        self.assertEqual(raised.exception.code, "realtime_invalid_multipart")


class RealtimeForwardingTests(unittest.TestCase):
    def test_converts_to_backend_json_and_preserves_complete_handshake(self):
        response = FakeResponse(
            status=201,
            body=b"v=answer\r\n",
            headers={
                "Content-Type": "application/sdp; charset=utf-8",
                "Location": "/v1/live/rtc_voice_123",
            },
        )
        captured = {}

        def open_request(request, timeout):
            captured["request"] = request
            captured["timeout"] = timeout
            return response

        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={
                "Authorization": "Bearer native-secret",
                "chatgpt-account-id": "acct-native",
            },
        ), patch("easy_multi_provider.realtime.open_request_status", side_effect=open_request):
            result = forward_native_realtime_call(
                "https://chatgpt.com/backend-api/codex",
                Path("unused-auth.json"),
                {
                    "Authorization": "Bearer caller-secret",
                    "OpenAI-Alpha": "quicksilver=v2",
                    "X-Session-Id": "session-voice",
                    "X-Ignored-Secret": "do-not-forward",
                },
                RealtimeCall(
                    "v=0\r\no=offer\r\n",
                    {"model": "gpt-live", "delegation": {"type": "client"}},
                ),
            )

        request = captured["request"]
        self.assertEqual(
            request.full_url,
            "https://chatgpt.com/backend-api/codex/realtime/calls"
            "?intent=quicksilver&architecture=avas",
        )
        self.assertEqual(json.loads(request.data), {
            "sdp": "v=0\r\no=offer\r\n",
            "session": {"model": "gpt-live", "delegation": {"type": "client"}},
        })
        request_headers = {key.lower(): value for key, value in request.header_items()}
        self.assertEqual(request_headers["authorization"], "Bearer native-secret")
        self.assertEqual(request_headers["chatgpt-account-id"], "acct-native")
        self.assertEqual(request_headers["openai-alpha"], "quicksilver=v2")
        self.assertEqual(request_headers["x-session-id"], "session-voice")
        self.assertNotIn("x-ignored-secret", request_headers)
        self.assertEqual(result.status, 201)
        self.assertEqual(result.content_type, "application/sdp; charset=utf-8")
        self.assertEqual(result.location, "/v1/live/rtc_voice_123")
        self.assertEqual(result.body, b"v=answer\r\n")
        call_id = result.location.rsplit("/", 1)[-1]
        self.assertEqual(
            "wss://api.openai.com/v1/live/" + call_id,
            "wss://api.openai.com/v1/live/rtc_voice_123",
        )
        self.assertTrue(response.closed)

    def test_reports_missing_native_subscription(self):
        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            side_effect=AccountError("missing"),
        ):
            with self.assertRaises(RealtimeError) as raised:
                forward_native_realtime_call(
                    "https://chatgpt.com/backend-api/codex",
                    Path("missing.json"),
                    {},
                    RealtimeCall("v=0\r\no=offer\r\n", {}),
                )
        self.assertEqual(raised.exception.status, 401)
        self.assertEqual(raised.exception.code, "native_subscription_unavailable")

    def test_preserves_upstream_auth_error_body_and_content_type(self):
        failure = HTTPError(
            "https://chatgpt.com/backend-api/codex/realtime/calls",
            401,
            "Unauthorized",
            {"Content-Type": "application/problem+json"},
            io.BytesIO(b'{"error":{"message":"expired"}}'),
        )
        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={"Authorization": "Bearer native-secret"},
        ), patch("easy_multi_provider.realtime.open_request_status", side_effect=failure):
            result = forward_native_realtime_call(
                "https://chatgpt.com/backend-api/codex",
                Path("unused.json"),
                {},
                RealtimeCall("v=0\r\no=offer\r\n", {}),
            )
        self.assertEqual(result.status, 401)
        self.assertEqual(result.content_type, "application/problem+json")
        self.assertEqual(result.body, b'{"error":{"message":"expired"}}')

    def test_preserves_non_success_status_content_type_and_location(self):
        response = FakeResponse(
            status=307,
            body=b"retry elsewhere",
            headers={
                "Content-Type": "text/plain",
                "Location": "/voice-temporarily-unavailable",
            },
        )
        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={"Authorization": "Bearer native-secret"},
        ), patch(
            "easy_multi_provider.realtime.open_request_status",
            return_value=response,
        ):
            result = forward_native_realtime_call(
                "https://chatgpt.com/backend-api/codex",
                Path("unused.json"),
                {},
                RealtimeCall("v=0\r\no=offer\r\n", {}),
            )
        self.assertEqual(result.status, 307)
        self.assertEqual(result.content_type, "text/plain")
        self.assertEqual(result.location, "/voice-temporarily-unavailable")
        self.assertEqual(result.body, b"retry elsewhere")

    def test_rejects_success_without_location_before_sideband(self):
        response = FakeResponse(headers={"Content-Type": "application/sdp"})
        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={"Authorization": "Bearer native-secret"},
        ), patch("easy_multi_provider.realtime.open_request_status", return_value=response):
            with self.assertRaises(RealtimeError) as raised:
                forward_native_realtime_call(
                    "https://chatgpt.com/backend-api/codex",
                    Path("unused.json"),
                    {},
                    RealtimeCall("v=0\r\no=offer\r\n", {}),
                )
        self.assertEqual(raised.exception.status, 502)
        self.assertEqual(raised.exception.code, "realtime_upstream_invalid")

    def test_reports_backend_without_realtime_support(self):
        failure = HTTPError(
            "https://chatgpt.com/backend-api/codex/realtime/calls",
            404,
            "Not Found",
            {},
            io.BytesIO(b""),
        )
        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={"Authorization": "Bearer native-secret"},
        ), patch("easy_multi_provider.realtime.open_request_status", side_effect=failure):
            result = forward_native_realtime_call(
                "https://chatgpt.com/backend-api/codex",
                Path("unused.json"),
                {},
                RealtimeCall("v=0\r\no=offer\r\n", {}),
            )
        self.assertEqual(result.status, 404)
        self.assertEqual(
            json.loads(result.body)["error"]["code"],
            "native_realtime_unsupported",
        )

    def test_sideband_uses_native_auth_and_selected_network_proxy(self):
        connection = object()
        captured = {}

        def connect(target):
            captured["target"] = target
            return connection

        with patch(
            "easy_multi_provider.realtime.native_auth_headers",
            return_value={
                "Authorization": "Bearer native-secret",
                "chatgpt-account-id": "acct-native",
            },
        ), patch(
            "easy_multi_provider.realtime.proxy_for_url",
            return_value="socks5h://127.0.0.1:7891",
        ), patch(
            "easy_multi_provider.realtime.open_native_websocket",
            side_effect=connect,
        ):
            result = open_native_realtime_sideband(
                Path("unused.json"),
                {
                    "Authorization": "Bearer caller-secret",
                    "OpenAI-Alpha": "quicksilver=v2",
                    "X-Session-Id": "session-voice",
                },
                "rtc_voice_proxy",
            )
        target = captured["target"]
        self.assertIs(result, connection)
        self.assertEqual(
            target.url,
            "wss://api.openai.com/v1/live/rtc_voice_proxy",
        )
        self.assertEqual(target.proxy, "socks5h://127.0.0.1:7891")
        self.assertEqual(target.headers["Authorization"], "Bearer native-secret")
        self.assertEqual(target.headers["chatgpt-account-id"], "acct-native")
        self.assertEqual(target.headers["OpenAI-Alpha"], "quicksilver=v2")
        self.assertEqual(target.headers["x-session-id"], "session-voice")


class RealtimeServerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        root = Path(self.directory.name)
        codex_home = root / "codex"
        codex_home.mkdir()
        self.auth_path = codex_home / "auth.json"
        self.auth_path.write_text(
            json.dumps(
                {
                    "tokens": {
                        "access_token": "caller-native-secret",
                        "account_id": "acct-native",
                    }
                }
            ),
            encoding="utf-8",
        )
        config_path = root / "config.json"
        save(
            normalize(
                {
                    "providers": [
                        {
                            "id": "external",
                            "base_url": "https://external.invalid/v1",
                            "protocol": "responses",
                            "api_key": "external-secret",
                        }
                    ],
                    "models": [
                        {
                            "id": "external/model",
                            "provider": "external",
                            "upstream_id": "model",
                        }
                    ],
                }
            ),
            config_path,
        )
        manager = IntegrationManager(
            codex_home / "config.toml",
            codex_home / "easy-multi-provider" / "integration" / "lease.json",
            instance_id="realtime-test",
        )
        self.journal = CapturingJournal()
        self.state = AppState(
            config_path,
            integration_manager=manager,
            journal=self.journal,
        )
        self.server = ThreadingHTTPServer(
            ("127.0.0.1", 0), make_handler(self.state)
        )
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    def tearDown(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)
        self.directory.cleanup()

    def request(self, path, body, content_type, authorization=True, extra_headers=None):
        headers = {"Content-Type": content_type}
        if authorization:
            headers["Authorization"] = "Bearer caller-native-secret"
        headers.update(extra_headers or {})
        connection = HTTPConnection(*self.server.server_address, timeout=3)
        connection.request("POST", path, body, headers)
        response = connection.getresponse()
        result = (
            response.status,
            dict(response.getheaders()),
            response.read(),
        )
        connection.close()
        return result

    def test_legal_request_uses_dedicated_route_and_preserves_response(self):
        body = multipart_body(
            sdp="v=0\r\no=offer-private\r\n",
            session={"model": "gpt-live", "private": "session-secret"},
        )
        seen = {}

        def forward(base_url, auth_path, headers, call):
            seen["base_url"] = base_url
            seen["auth_path"] = auth_path
            seen["call"] = call
            return RealtimeResponse(
                201,
                "application/sdp",
                b"v=answer\r\n",
                "/v1/live/rtc_server_test",
            )

        with patch(
            "easy_multi_provider.server.forward_native_realtime_call",
            side_effect=forward,
        ), patch(
            "easy_multi_provider.server.valid_caller_authorization",
            return_value=True,
        ):
            status, headers, response_body = self.request(
                "/v1/live",
                body,
                "multipart/form-data; boundary=" + BOUNDARY,
            )
        self.assertEqual(status, 201)
        self.assertEqual(headers["Content-Type"], "application/sdp")
        self.assertEqual(headers["Location"], "/v1/live/rtc_server_test")
        self.assertEqual(response_body, b"v=answer\r\n")
        self.assertEqual(seen["call"].sdp, "v=0\r\no=offer-private\r\n")
        rendered_events = json.dumps(self.journal.events, ensure_ascii=False)
        self.assertNotIn("offer-private", rendered_events)
        self.assertNotIn("session-secret", rendered_events)
        self.assertNotIn("caller-native-secret", rendered_events)
        self.assertNotIn("external-secret", rendered_events)

    def test_unauthorized_request_is_rejected_before_forwarding(self):
        with patch(
            "easy_multi_provider.server.forward_native_realtime_call"
        ) as forward:
            status, _, body = self.request(
                "/v1/live",
                multipart_body(),
                "multipart/form-data; boundary=" + BOUNDARY,
                authorization=False,
            )
        self.assertEqual(status, 401)
        self.assertEqual(
            json.loads(body)["error"]["code"], "realtime_caller_unauthorized"
        )
        forward.assert_not_called()

    def test_missing_native_subscription_has_explicit_error(self):
        saved = self.auth_path.read_bytes()
        self.auth_path.unlink()
        try:
            status, _, body = self.request(
                "/v1/live",
                multipart_body(),
                "multipart/form-data; boundary=" + BOUNDARY,
                authorization=False,
            )
        finally:
            self.auth_path.write_bytes(saved)
        self.assertEqual(status, 401)
        self.assertEqual(
            json.loads(body)["error"]["code"],
            "native_subscription_unavailable",
        )

    def test_other_proxy_posts_still_require_json(self):
        with patch(
            "easy_multi_provider.server.valid_caller_authorization",
            return_value=True,
        ):
            status, _, body = self.request(
                "/v1/responses",
                multipart_body(),
                "multipart/form-data; boundary=" + BOUNDARY,
            )
        self.assertEqual(status, 400)
        self.assertEqual(
            json.loads(body)["error"]["message"],
            "Content-Type must be application/json",
        )

    def test_sideband_relays_events_and_graceful_close(self):
        upstream = FakeSideband()
        url = "ws://127.0.0.1:%d/v1/live/rtc_relay_test" % self.server.server_port
        with patch(
            "easy_multi_provider.server.valid_caller_authorization",
            return_value=True,
        ), patch(
            "easy_multi_provider.server.open_native_realtime_sideband",
            return_value=upstream,
        ):
            client = websocket.create_connection(
                url,
                timeout=3,
                header={"Authorization": "Bearer caller-native-secret"},
                suppress_origin=True,
            )
            try:
                self.assertEqual(
                    json.loads(client.recv())["type"],
                    "session.started",
                )
                client.send(
                    json.dumps(
                        {"type": "session.close", "private": "sideband-secret"}
                    )
                )
                self.assertEqual(
                    json.loads(client.recv())["type"],
                    "session.closed",
                )
            finally:
                client.close()
        deadline = time.monotonic() + 2
        while not upstream.closed and time.monotonic() < deadline:
            time.sleep(0.01)
        self.assertEqual(json.loads(upstream.sent[0])["type"], "session.close")
        self.assertTrue(upstream.closed)
        self.assertNotIn(
            "sideband-secret",
            json.dumps(self.journal.events, ensure_ascii=False),
        )

    def test_sideband_requires_caller_auth_before_upstream_connect(self):
        url = "ws://127.0.0.1:%d/v1/live/rtc_unauthorized" % self.server.server_port
        with patch(
            "easy_multi_provider.server.open_native_realtime_sideband"
        ) as connect:
            with self.assertRaises(websocket.WebSocketBadStatusException) as raised:
                websocket.create_connection(url, timeout=3, suppress_origin=True)
        self.assertEqual(raised.exception.status_code, 401)
        connect.assert_not_called()


if __name__ == "__main__":
    unittest.main()
