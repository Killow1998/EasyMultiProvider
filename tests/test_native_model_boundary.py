import json
import unittest
from unittest.mock import patch

import easy_multi_provider.router as router
from easy_multi_provider.server import _native_websocket_metadata_events


class _Response:
    status = 200
    headers = {
        "Content-Type": "application/json",
        "openai-model": "gpt-6-astra",
        "x-openai-model": "gpt-6-astra",
    }

    def __init__(self, body):
        self._body = body
        self._read = False

    def read(self, size=-1):
        if self._read:
            return b""
        self._read = True
        return self._body

    def __enter__(self):
        return self

    def __exit__(self, *args):
        return None


class _StreamResponse:
    status = 200
    headers = {"Content-Type": "text/event-stream"}

    def __init__(self, frame):
        self.frame = frame

    def __iter__(self):
        yield self.frame[:17]
        yield self.frame[17:]

    def close(self):
        return None


class NativeModelBoundaryTests(unittest.TestCase):
    def _provider(self):
        return {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.test/v1",
        }

    def _model(self):
        return {"id": "native/gpt-6-astra", "upstream_id": "gpt-6-astra"}

    def test_http_headers_map_only_known_expected_basename(self):
        provider = self._provider()
        body = {"model": "native/gpt-6-astra", "input": "hello"}
        response = _Response(b'{"status":"completed","output":[]}')
        with patch.object(router, "_request", return_value=response) as request:
            router.forward_responses(provider, body, self._model(), {})

        self.assertEqual(request.call_args.args[1]["model"], "gpt-6-astra")
        self.assertEqual(
            provider["_emp_response_headers"]["openai-model"],
            "native/gpt-6-astra",
        )
        self.assertEqual(
            provider["_emp_response_headers"]["x-openai-model"],
            "native/gpt-6-astra",
        )

    def test_actual_upstream_reroute_is_not_hidden(self):
        provider = self._provider()
        provider["_emp_requested_model"] = "native/gpt-6-astra"
        provider["_emp_expected_upstream_model"] = "gpt-6-astra"
        headers = {"openai-model": "gpt-5.2", "x-request-id": "fixture"}

        self.assertEqual(
            router._rewrite_native_model_headers(headers, provider), headers
        )
        event = {"type": "response.metadata", "headers": headers}
        self.assertIs(router._rewrite_native_model_event(event, provider), event)

    def test_sse_event_headers_map_without_rewriting_response_model(self):
        provider = self._provider()
        event = {
            "type": "response.completed",
            "response": {
                "status": "completed",
                "model": "gpt-6-astra",
                "headers": {
                    "openai-model": "gpt-6-astra",
                    "x-openai-model": "gpt-6-astra",
                },
                "output": [],
            },
        }
        frame = (
            b"event: response.completed\n"
            + b"data: "
            + json.dumps(event, separators=(",", ":")).encode()
            + b"\n\n"
        )

        output = b"".join(router._rewrite_native_model_stream([frame], {
            **provider,
            "_emp_requested_model": "native/gpt-6-astra",
            "_emp_expected_upstream_model": "gpt-6-astra",
        }))
        payload = json.loads(next(line[6:] for line in output.splitlines() if line.startswith(b"data: ")))
        self.assertEqual(
            payload["response"]["headers"]["openai-model"],
            "native/gpt-6-astra",
        )
        self.assertEqual(payload["response"]["model"], "gpt-6-astra")

    def test_stream_forwarding_applies_the_same_request_local_mapping(self):
        provider = self._provider()
        body = {"model": "native/gpt-6-astra", "input": "hello", "stream": True}
        event = {
            "type": "response.completed",
            "response": {
                "status": "completed",
                "model": "gpt-6-astra",
                "headers": {"openai-model": "gpt-6-astra"},
                "output": [],
            },
        }
        frame = ("data: " + json.dumps(event) + "\n\n").encode()
        response = _StreamResponse(frame)
        with patch.object(router, "_request", return_value=response):
            output = b"".join(
                router.forward_responses_stream(provider, body, self._model(), {})
            )
        payload = json.loads(next(line[6:] for line in output.splitlines() if line.startswith(b"data: ")))
        self.assertEqual(
            payload["response"]["headers"]["openai-model"],
            "native/gpt-6-astra",
        )

    def test_native_websocket_metadata_maps_model_but_keeps_catalog_event(self):
        events = _native_websocket_metadata_events(
            {"openai-model": "gpt-6-astra", "x-models-etag": "etag"},
            requested_model="native/gpt-6-astra",
            expected_upstream_model="gpt-6-astra",
        )
        self.assertEqual(
            events,
            [
                {
                    "type": "response.metadata",
                    "headers": {"openai-model": "native/gpt-6-astra"},
                },
                {
                    "type": "codex.response.metadata",
                    "headers": {"x-models-etag": "etag"},
                },
            ],
        )

    def test_reused_native_websocket_metadata_does_not_replay_stale_101_state(self):
        events = _native_websocket_metadata_events(
            {
                "openai-model": "gpt-6-astra",
                "x-openai-model": "gpt-6-astra",
                "x-codex-turn-state": "stale-turn-state",
                "x-models-etag": "stale-catalog",
                "x-rate-limit-remaining": "stale-rate-limit",
                "x-reasoning-included": "true",
            },
            requested_model="native/gpt-6-astra",
            expected_upstream_model="gpt-6-astra",
            reused=True,
        )
        self.assertEqual(
            events,
            [
                {
                    "type": "response.metadata",
                    "headers": {
                        "openai-model": "native/gpt-6-astra",
                        "x-openai-model": "native/gpt-6-astra",
                    },
                }
            ],
        )


if __name__ == "__main__":
    unittest.main()
