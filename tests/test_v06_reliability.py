import io
import json
import threading
import time
import unittest
from concurrent.futures import ThreadPoolExecutor
from http.client import HTTPConnection
from pathlib import Path
from tempfile import TemporaryDirectory
from unittest.mock import patch

import zstandard

from easy_multi_provider import router
from easy_multi_provider.router import (
    _reliable_responses_stream,
    _response_failure_frame,
    _sse_frame,
)
from easy_multi_provider.server import AppState, BoundedThreadingHTTPServer, ObservationRing, make_handler
from easy_multi_provider.transport import WebSocketConnection, sse_json_events
from easy_multi_provider.transport_failures import (
    CONNECT_TIMEOUT,
    PHASE_CONNECT,
    TransportFailure,
)


def _event(event_type, **value):
    return _sse_frame(event_type, {"type": event_type, **value})


class StreamReliabilityTests(unittest.TestCase):
    def test_deterministic_failure_frame_fails_closed_without_retry(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _response_failure_frame(
                        "upstream closed before terminal",
                        error_class="stream_incomplete",
                    ),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1])
        self.assertEqual([event["type"] for event in events], ["response.failed"])
        self.assertEqual(len(terminals), 1)
        self.assertFalse(terminals[0]["success"])
        self.assertFalse(terminals[0]["output_emitted"])
        self.assertFalse(terminals[0]["tool_activity"])
        self.assertTrue(terminals[0]["terminal_event_observed"])
        self.assertFalse(terminals[0]["recovery_succeeded"])

    def test_partial_output_close_fails_once_without_replay(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event("response.output_text.delta", delta="visible"),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(factory, terminal_callback=terminals.append)
            )
        )

        self.assertEqual(attempts, [1])
        self.assertEqual(
            [event["type"] for event in events],
            ["response.created", "response.output_text.delta", "response.failed"],
        )
        self.assertEqual(len(terminals), 1)
        self.assertEqual(terminals[0]["error_class"], "stream_incomplete")
        self.assertTrue(terminals[0]["output_emitted"])
        self.assertFalse(terminals[0]["recovery_succeeded"])

    def test_tool_activity_prevents_generation_replay(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event(
                        "response.output_item.added",
                        item={"type": "function_call", "call_id": "call_fixture"},
                    ),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1])
        self.assertEqual(events[-1]["type"], "response.failed")
        self.assertEqual(sum(event["type"] == "response.failed" for event in events), 1)
        self.assertEqual(terminals[0]["error_class"], "stream_incomplete")
        self.assertTrue(terminals[0]["tool_activity"])

    def test_duplicate_terminal_is_suppressed(self):
        terminals = []

        def factory():
            return iter(
                [
                    _event("response.created", response={"id": "resp_fixture"}),
                    _event("response.completed", response={"id": "resp_fixture"}),
                    _event("response.completed", response={"id": "resp_duplicate"}),
                ]
            )

        events = list(
            sse_json_events(
                _reliable_responses_stream(factory, terminal_callback=terminals.append)
            )
        )

        self.assertEqual(sum(event["type"] == "response.completed" for event in events), 1)
        self.assertEqual(len(terminals), 1)

    def test_completed_event_with_contradictory_nested_terminal_never_succeeds(self):
        cases = (
            ({"status": "incomplete", "incomplete_details": {"reason": "max_output_tokens"}}, "output_limit"),
            ({"status": "failed"}, "stream_error"),
            ({"status": "completed", "error": {"status": 429}}, "rate_limit"),
            ({"status": "unexpected"}, "malformed_terminal"),
        )
        for response, expected_class in cases:
            with self.subTest(response=response):
                terminals = []

                def factory():
                    return iter([_event("response.completed", response=response)])

                events = list(
                    sse_json_events(
                        _reliable_responses_stream(
                            factory, terminal_callback=terminals.append
                        )
                    )
                )

                self.assertFalse(any(event["type"] == "response.completed" for event in events))
                self.assertEqual(sum(event["type"] == "response.failed" for event in events), 1)
                self.assertEqual(len(terminals), 1)
                self.assertFalse(terminals[0]["success"])
                self.assertEqual(terminals[0]["error_class"], expected_class)

    def test_pre_output_recovery_is_attempted_at_most_once(self):
        attempts = []
        terminals = []

        def factory():
            attempts.append(len(attempts) + 1)
            raise TransportFailure(CONNECT_TIMEOUT, 504, PHASE_CONNECT)

        events = list(
            sse_json_events(
                _reliable_responses_stream(
                    factory,
                    terminal_callback=terminals.append,
                    replay_safe=True,
                )
            )
        )

        self.assertEqual(attempts, [1, 2])
        self.assertEqual(sum(event["type"] == "response.failed" for event in events), 1)
        self.assertEqual(len(terminals), 1)
        self.assertFalse(terminals[0]["recovery_succeeded"])
        self.assertEqual(terminals[0]["recovery_mode"], "pre_output_retry")

    def test_production_responses_proxy_does_not_replay_pre_output_failure(self):
        terminals = []
        first = iter(
            [
                _event("response.created", response={"id": "resp_fixture"}),
                _response_failure_frame(
                    "upstream closed before terminal",
                    error_class="stream_incomplete",
                ),
            ]
        )
        second = iter(
            [
                _event("response.created", response={"id": "resp_fixture"}),
                _event("response.output_text.delta", delta="visible"),
                _event("response.completed", response={"id": "resp_fixture"}),
            ]
        )
        provider = {
            "id": "provider-fixture",
            "protocol": "responses",
            "auth_mode": "api_key",
        }
        model = {"id": "provider-fixture/model-fixture", "upstream_id": "model-fixture"}
        body = {"model": model["id"], "input": [], "stream": True}

        with patch(
            "easy_multi_provider.router.forward_responses_stream",
            side_effect=[first, second],
        ) as upstream:
            metadata, result = router._proxy_resolved(
                provider,
                model,
                body,
                {},
                terminal_callback=terminals.append,
            )
            events = list(sse_json_events(result))

        self.assertEqual(upstream.call_count, 1)
        self.assertEqual(metadata["kind"], "stream")
        self.assertEqual(
            [event["type"] for event in events],
            ["response.failed"],
        )
        self.assertTrue(terminals[0]["terminal_event_observed"])
        self.assertFalse(terminals[0]["recovery_succeeded"])


class BoundaryDiagnosticsTests(unittest.TestCase):
    def test_lifecycle_diagnostics_are_bounded_and_content_free(self):
        ring = ObservationRing()
        ring.record(
            {
                "route": "responses",
                "resolved_protocol": "responses",
                "transport": "websocket",
                "close_code": 1006,
                "error_class": "upstream_close_after_output",
                "output_emitted": True,
                "tool_activity": False,
                "terminal_event_observed": False,
                "recovery_succeeded": False,
                "recovery_mode": "none",
                "unsafe_content": "must-not-survive",
                "authorization": "must-not-survive",
            }
        )

        record = ring.snapshot()["records"][0]

        self.assertEqual(record["close_code"], 1006)
        self.assertTrue(record["output_emitted"])
        self.assertFalse(record["tool_activity"])
        self.assertFalse(record["terminal_event_observed"])
        self.assertFalse(record["recovery_succeeded"])
        self.assertEqual(record["recovery_mode"], "none")
        self.assertNotIn("unsafe_content", record)
        self.assertNotIn("authorization", record)

    def test_websocket_records_peer_close_code_without_reason(self):
        mask = b"\x01\x02\x03\x04"
        payload = b"\x03\xe9private reason"
        encoded = bytes(byte ^ mask[index % 4] for index, byte in enumerate(payload))
        reader = io.BytesIO(bytes((0x88, 0x80 | len(payload))) + mask + encoded)
        writer = io.BytesIO()
        websocket = WebSocketConnection(reader, writer)

        self.assertIsNone(websocket.receive_text())
        self.assertEqual(websocket.peer_close_code, 1001)
        self.assertFalse(hasattr(websocket, "peer_close_reason"))


class BoundedConcurrencyTests(unittest.TestCase):
    def test_six_request_transport_metadata_is_local_and_overlapped(self):
        from collections import Counter

        specs = [
            ("native", False, "n0", "N" * (256 * 1024)),
            ("native", True, "n1", "native-stream-1"),
            ("native", False, "n2", "native-body-2"),
            ("external", False, "e0", "external-body-0"),
            ("external", True, "e1", "external-stream-1"),
            ("external", False, "e2", "external-body-2"),
        ]
        upstream_barrier = threading.Barrier(6, timeout=3)
        client_barrier = threading.Barrier(6, timeout=3)
        upstream_requests = {}
        upstream_lock = threading.Lock()
        active = 0
        peak_active = 0

        class FakeResponse(io.BytesIO):
            status = 200
            headers = {"Content-Type": "application/json"}

        def fake_upstream(request, timeout):
            nonlocal active, peak_active
            headers = {key.lower(): value for key, value in request.header_items()}
            encoded = request.data
            decoded = (
                zstandard.ZstdDecompressor().decompress(encoded)
                if headers.get("content-encoding") == "zstd"
                else encoded
            )
            payload = json.loads(decoded.decode("utf-8"))
            marker = payload["input"]
            with upstream_lock:
                upstream_requests[marker] = (encoded, decoded, headers)
                active += 1
                peak_active = max(peak_active, active)
            try:
                upstream_barrier.wait()
                time.sleep(0.03)
            finally:
                with upstream_lock:
                    active -= 1
            response = {
                "id": "response_" + marker[-2:],
                "object": "response",
                "status": "completed",
                "model": payload["model"],
                "output": [],
            }
            if headers.get("accept") == "text/event-stream":
                raw = _event("response.completed", response=response)
                result = FakeResponse(raw)
                result.headers = {"Content-Type": "text/event-stream"}
                return result
            return FakeResponse(json.dumps(response, separators=(",", ":")).encode())

        def send(server, spec):
            route, stream, suffix, content = spec
            model = "%s-fixture/%s" % (route, suffix)
            body = json.dumps(
                {"model": model, "input": content, "stream": stream},
                separators=(",", ":"),
            ).encode()
            client_barrier.wait()
            connection = HTTPConnection(*server.server_address, timeout=4)
            connection.request(
                "POST",
                "/v1/responses",
                body=body,
                headers={
                    "Content-Type": "application/json",
                    "Authorization": "Bearer fixture",
                    "Connection": "close",
                },
            )
            response = connection.getresponse()
            if stream:
                lines = []
                while len(lines) < 8:
                    line = response.readline()
                    if not line:
                        break
                    lines.append(line)
                    if b"response.completed" in line:
                        break
                raw = b"".join(lines)
            else:
                raw = response.read()
            connection.close()
            return model, response.status, raw

        with TemporaryDirectory() as directory:
            state = AppState(Path(directory) / "config.json", runtime_controller=object())
            state.config = {
                "providers": [
                    {
                        "id": "native-fixture",
                        "protocol": "responses",
                        "auth_mode": "forward",
                        "base_url": "https://native.invalid/backend-api/codex",
                    },
                    {
                        "id": "external-fixture",
                        "protocol": "responses",
                        "auth_mode": "api_key",
                        "api_key": "fixture-key",
                        "base_url": "https://external.invalid/v1",
                    },
                ],
                "models": [
                    {
                        "id": "%s-fixture/%s" % (route, suffix),
                        "provider": "%s-fixture" % route,
                        "upstream_id": "model-" + suffix,
                    }
                    for route, _, suffix, _ in specs
                ],
            }
            server = BoundedThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            started = time.monotonic()
            try:
                with patch("easy_multi_provider.router.urlopen", side_effect=fake_upstream), patch(
                    "easy_multi_provider.server.valid_caller_authorization", return_value=True
                ), ThreadPoolExecutor(max_workers=6) as pool:
                    results = list(pool.map(lambda spec: send(server, spec), specs))
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=3)
            elapsed = time.monotonic() - started

        self.assertEqual(peak_active, 6)
        self.assertLess(elapsed, 4.0)
        self.assertTrue(all(status == 200 for _, status, _ in results))
        self.assertEqual(len(upstream_requests), 6)
        records = [
            record
            for record in state.diagnostics.snapshot()["records"]
            if record["status"] is not None or record["terminal_event_observed"]
        ]
        self.assertEqual(len(records), 6)
        self.assertEqual(
            Counter(record["model_id"] for record in records),
            Counter({model: 1 for model, _, _ in results}),
        )
        self.assertEqual(
            sorted(record["model_id"] for record in records),
            sorted(model for model, _, _ in results),
        )
        by_model = {record["model_id"]: record for record in records}
        self.assertEqual(set(by_model), {model for model, _, _ in results})
        for route, _, suffix, content in specs:
            model = "%s-fixture/%s" % (route, suffix)
            upstream_model = "model-" + suffix
            encoded, decoded, headers = upstream_requests[content]
            expected = json.dumps(
                {"model": upstream_model, "input": content, "stream": bool(next(s[1] for s in specs if s[2] == suffix))},
                ensure_ascii=False,
                separators=(",", ":"),
            ).encode()
            self.assertEqual(decoded, expected)
            record = by_model[model]
            self.assertEqual(record["decoded_request_bytes"], len(decoded))
            self.assertEqual(record["upstream_request_bytes"], len(encoded))
            if route == "native":
                self.assertEqual(headers.get("content-encoding"), "zstd")
                self.assertEqual(record["upstream_content_encoding"], "zstd")
            else:
                self.assertNotIn("content-encoding", headers)
                self.assertEqual(encoded, decoded)
                self.assertEqual(record["upstream_content_encoding"], "identity")


if __name__ == "__main__":
    unittest.main()
