"""Native terminal truth through the real validation and parser boundaries."""
import io
import json
import unittest

from easy_multi_provider.router import _validated_responses_stream
from easy_multi_provider.stream_adapters import _reliable_responses_stream
from easy_multi_provider.transport import sse_json_events


class _Response(io.BytesIO):
    status = 200
    headers = {"Content-Type": "text/event-stream"}


def _frame(event):
    return ("data: " + json.dumps(event) + "\n\n").encode()


class NativeStreamFidelityTests(unittest.TestCase):
    def _events(self, wire, *, native=True, auth_mode="forward",
                content_type="text/event-stream"):
        reports = []
        provider = {"id": "native", "protocol": "responses", "auth_mode": auth_mode if native else "api_key",
                    "_context_observation": {"input_estimate": 64}}
        response = _Response(wire)
        response.headers = {"Content-Type": content_type}
        events = list(sse_json_events(_reliable_responses_stream(
            lambda: _validated_responses_stream(
                response, provider=provider,
            ), terminal_callback=reports.append, native_passthrough=native,
        )))
        return events, reports, provider["_context_observation"]

    @staticmethod
    def _server_search_body():
        # These server-owned items are valid ResponseItem variants in 0.155.
        output = [
            {"type": "tool_search_call", "id": "fixture-search-item",
             "call_id": "fixture-search-call", "execution": "server",
             "arguments": {"query": "fixture"}, "status": "completed"},
            {"type": "tool_search_output", "call_id": "fixture-search-call",
             "execution": "server", "status": "completed", "tools": []},
        ]
        return {"id": "fixture-native-server-search", "status": "completed", "output": output}

    def test_native_json_fallback_preserves_server_search_for_both_auth_modes(self):
        body = self._server_search_body()
        for mode in ("forward", "account", "api_key"):
            native = mode != "api_key"
            with self.subTest(auth_mode=mode):
                events, _, _ = self._events(
                    json.dumps(body).encode(), native=native, auth_mode=mode,
                    content_type="application/json",
                )
                if native:
                    self.assertEqual(events[-1]["type"], "response.completed")
                    self.assertEqual(events[-1]["response"]["output"], body["output"])
                else:
                    self.assertEqual(events[-1]["type"], "response.failed")

    def test_native_sse_terminal_preserves_server_search_for_both_auth_modes(self):
        body = self._server_search_body()
        for mode in ("forward", "account", "api_key"):
            native = mode != "api_key"
            with self.subTest(auth_mode=mode):
                events, _, _ = self._events(
                    _frame({"type": "response.completed", "response": body}),
                    native=native, auth_mode=mode,
                )
                if native:
                    self.assertEqual(events[-1]["type"], "response.completed")
                    self.assertEqual(events[-1]["response"]["output"], body["output"])
                else:
                    self.assertEqual(events[-1]["type"], "response.failed")

    def test_native_malformed_events_match_official_skip_and_continue(self):
        completed = {"type": "response.completed", "response": {
            "id": "fixture-completed", "status": "completed", "output": [],
        }}
        for auth_mode in ("forward", "account"):
            for data in (b"{not json}", b"[]", b"null"):
                with self.subTest(auth_mode=auth_mode, data=data):
                    events, _, _ = self._events(
                        b"data: " + data + b"\n\n" + _frame(completed), auth_mode=auth_mode,
                    )
                    self.assertEqual(events, [completed])

    def test_portable_malformed_events_still_fail_before_completion(self):
        completed = {"type": "response.completed", "response": {
            "id": "fixture-completed", "status": "completed", "output": [],
        }}
        for data in (b"{not json}", b"[]", b"null"):
            with self.subTest(data=data):
                events, _, _ = self._events(
                    b"data: " + data + b"\n\n" + _frame(completed), native=False,
                )
                self.assertNotIn("response.completed", [event.get("type") for event in events])
                self.assertEqual(events[-1].get("type"), "response.failed")

    def test_valid_pending_native_terminal_at_eof_is_retained(self):
        completed = {"type": "response.completed", "response": {
            "id": "fixture-completed", "status": "completed", "output": [],
        }}
        events, _, _ = self._events(_frame(completed).rstrip(b"\n") + b"\n")
        self.assertEqual(events, [completed])

    def test_native_context_failure_keeps_payload_and_context_accounting(self):
        failed = {"type": "response.failed", "response": {
            "id": "fixture-context", "status": "failed", "error": {
                "code": "context_length_exceeded", "message": "original context explanation",
            },
        }}
        events, reports, observation = self._events(_frame(failed))
        self.assertEqual(events, [failed])
        self.assertTrue(any(report.get("error_class") == "context_length_exceeded" for report in reports))
        self.assertTrue(observation.get("explicit_failure"))

    def test_sniffed_sse_limit_applies_to_each_event_in_a_coalesced_body(self):
        delta = {"type": "response.output_text.delta", "delta": "x" * 180_000}
        completed = {"type": "response.completed", "response": {
            "status": "completed", "output": [],
        }}
        response = _Response(_frame(delta) * 7 + _frame(completed))
        # This existing compatibility path sniffs SSE when an upstream sends
        # the wrong MIME type, then validates the fully read body as a chunk.
        response.headers = {"Content-Type": "application/octet-stream"}
        events = list(sse_json_events(_validated_responses_stream(
            response, provider={"id": "native", "protocol": "responses", "auth_mode": "forward"},
        )))
        self.assertEqual(events, [delta] * 7 + [completed])
