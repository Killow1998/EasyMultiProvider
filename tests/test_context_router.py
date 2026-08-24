import json
import unittest
from io import BytesIO
from urllib.error import HTTPError
from unittest.mock import patch

import easy_multi_provider.router as router
from easy_multi_provider.context_guard import ContextGuardBlocked
from easy_multi_provider.router import ContextLengthError, RouterError


class _Response:
    status = 200
    headers = {"Content-Type": "text/event-stream"}

    def __init__(self, chunks):
        self._chunks = list(chunks)
        self.closed = False

    def __iter__(self):
        return iter(self._chunks)

    def read(self, size=-1):
        return b"".join(self._chunks)

    def close(self):
        self.closed = True


class ContextRouterTests(unittest.TestCase):
    def setUp(self):
        self.provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }
        self.model = {
            "id": "demo/model",
            "provider": "demo",
            "upstream_id": "model",
        }

    def _check(self, payload, stream, operation):
        return {
            "provider_id": "demo",
            "model_id": "demo/model",
            "protocol": "responses",
            "input_estimate": 900,
            "estimated_tokens": 900,
            "context_limit": 1000,
            "safe_input_limit": 700,
            "confidence": 1.0,
            "source": "manual",
        }

    def test_explicit_context_http_error_is_non_retryable_and_safe(self):
        error = HTTPError(
            "https://example.com/v1/responses",
            400,
            "bad request",
            {"Content-Type": "application/json"},
            BytesIO(json.dumps({
                "error": {"code": "context_length_exceeded", "message": "prompt-secret"}
            }).encode()),
        )
        with patch.object(router, "urlopen", side_effect=error) as urlopen, patch.object(
            router.time, "sleep"
        ):
            with self.assertRaises(ContextLengthError) as raised:
                router._request(
                    dict(self.provider),
                    {"model": "model", "input": "prompt-secret"},
                    {},
                    context_check=self._check,
                )
        self.assertEqual(urlopen.call_count, 1)
        self.assertEqual(raised.exception.status, 413)
        self.assertIn("estimated input 900", str(raised.exception))
        self.assertNotIn("prompt-secret", str(raised.exception))
        self.assertEqual(raised.exception.context_observation["explicit_failure"], True)

    def test_preflight_block_does_not_call_upstream(self):
        def block(payload, stream, operation):
            class Assessment:
                def to_safe_dict(self):
                    return {
                        "provider_id": "demo",
                        "model_id": "demo/model",
                        "input_estimate": 5000,
                        "safe_input_limit": 1000,
                    }

            raise ContextGuardBlocked(Assessment())

        with patch.object(router, "urlopen") as urlopen:
            with self.assertRaises(ContextLengthError) as raised:
                router._request(
                    dict(self.provider),
                    {"model": "model", "input": "too-large"},
                    {},
                    context_check=block,
                )
        urlopen.assert_not_called()
        self.assertEqual(raised.exception.status, 413)
        self.assertIn("demo/model", str(raised.exception))

    def test_auto_fallback_does_not_emit_or_calibrate_rejected_candidate(self):
        config = {
            "providers": [{**self.provider, "protocol": "auto"}],
            "models": [self.model],
        }
        route_events = []
        checks = []

        def context(provider, model, protocol, payload, stream, operation):
            checks.append(protocol)
            return {
                "provider_id": provider["id"],
                "model_id": model["id"],
                "protocol": protocol,
                "input_estimate": 10,
                "estimated_tokens": 10,
                "context_limit": 1000,
                "safe_input_limit": 700,
                "confidence": 1.0,
                "source": "manual",
            }

        def rejected(provider, body, model, incoming, context_check=None):
            context_check({"model": "model", "input": "x"}, False, "")
            raise RouterError("protocol rejected", 404)

        def successful(provider, body, model, incoming, context_check=None):
            context_check({"model": "model", "input": "x"}, False, "")
            return (
                200,
                "application/json",
                b'{"status":"completed","output":[]}',
            )

        with patch.object(router, "chat_completion", side_effect=rejected), patch.object(
            router, "forward_responses", side_effect=successful
        ):
            metadata, _ = router.proxy(
                config,
                {"model": "demo/model", "input": "x"},
                {},
                route_events.append,
                context,
            )
        self.assertEqual(metadata["resolved_protocol"], "responses")
        self.assertEqual(checks, ["chat_completions", "responses"])
        self.assertEqual(len(route_events), 1)
        self.assertEqual(route_events[0]["resolved_protocol"], "responses")
        self.assertTrue(route_events[0]["success"])

    def test_stream_terminal_success_and_incomplete_are_distinct(self):
        for chunks, expected in (
            ([b'data: {"type":"response.completed"}\n\n'], "none"),
            ([b'data: {"type":"response.output_text.delta"}\n\n'], "stream_incomplete"),
        ):
            observed = []
            response = _Response(chunks)
            output = list(router._validated_responses_stream(
                response,
                observed.append,
                dict(self.provider, _context_observation=self._check({}, False, "")),
            ))
            self.assertTrue(output)
            self.assertEqual(observed[-1]["error_class"], expected)
            self.assertEqual(response.closed, True)


if __name__ == "__main__":
    unittest.main()
