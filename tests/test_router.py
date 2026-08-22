import unittest
import json
import tempfile
from io import BytesIO
from pathlib import Path
from urllib.error import HTTPError, URLError
from unittest.mock import patch

from tests.support import ensure_test_master_key
import easy_multi_provider.router as router
from easy_multi_provider.capabilities import endpoint_fingerprint
from easy_multi_provider.router import (
    RouterError,
    _response_from_anthropic,
    _response_from_chat,
    find_route,
    proxy_compact,
    responses_to_anthropic,
    responses_to_chat,
)
from easy_multi_provider.transport import sse_json_events
from easy_multi_provider.vault import write_encrypted_json


ensure_test_master_key()


class RouterTests(unittest.TestCase):
    def setUp(self):
        self.config = {
            "providers": [
                {"id": "chatgpt", "enabled": True, "auth_mode": "forward", "protocol": "responses"},
                {"id": "demo", "enabled": True, "auth_mode": "api_key", "protocol": "chat_completions"},
            ],
            "models": [{"id": "demo/model", "provider": "demo", "enabled": True}],
        }

    def test_external_model_routes_to_configured_provider(self):
        provider, model = find_route(self.config, "demo/model")
        self.assertEqual(provider["id"], "demo")
        self.assertEqual(model["id"], "demo/model")

    def test_forward_provider_uses_codex_session_auth(self):
        headers = router._headers(
            {
                "id": "chatgpt-subscription",
                "protocol": "responses",
                "auth_mode": "forward",
            },
            {
                "Authorization": "Bearer session-token",
                "ChatGPT-Account-ID": "account-1",
                "Originator": "codex_cli_rs",
            },
            False,
        )
        self.assertEqual(headers["Authorization"], "Bearer session-token")
        self.assertEqual(headers["chatgpt-account-id"], "account-1")
        self.assertEqual(headers["originator"], "codex_cli_rs")
        self.assertEqual(headers["User-Agent"], "EasyMultiProvider/" + router.__version__)

    def test_native_search_uses_codex_base_url_and_caller_auth(self):
        captured = {}

        class Response:
            status = 200
            headers = {"Content-Type": "application/json"}

            def read(self, size=-1):
                return b'{"data":[]}' if not captured.get("read") else b""

            def close(self):
                captured["closed"] = True

        def open_request(request, timeout):
            captured["url"] = request.full_url
            captured["authorization"] = request.get_header("Authorization")
            captured["read"] = False
            response = Response()
            original_read = response.read

            def read(size=-1):
                value = original_read(size)
                captured["read"] = True
                return value

            response.read = read
            return response

        with patch.object(router, "urlopen", side_effect=open_request):
            status, content_type, raw = router.forward_native_search(
                {"codex_base_url": "https://chatgpt.com/backend-api/codex"},
                {"query": "codex"},
                {"Authorization": "Bearer session-token"},
            )

        self.assertEqual(status, 200)
        self.assertEqual(content_type, "application/json")
        self.assertEqual(raw, b'{"data":[]}')
        self.assertEqual(
            captured["url"],
            "https://chatgpt.com/backend-api/codex/alpha/search",
        )
        self.assertEqual(captured["authorization"], "Bearer session-token")

    def test_limited_discovery_read_stops_after_deadline(self):
        class DripResponse:
            def __init__(self):
                self.closed = False

            def read(self, size):
                return b"x"

            def close(self):
                self.closed = True

        response = DripResponse()
        with patch.object(router.time, "monotonic", side_effect=[0, 2]):
            with self.assertRaises(RouterError) as raised:
                router._read_limited(response, 100, "discovery", deadline=1)
        self.assertEqual(raised.exception.status, 504)
        self.assertTrue(response.closed)

    def test_upstream_body_read_uses_remaining_global_deadline(self):
        class Socket:
            timeout = None

            def settimeout(self, value):
                self.timeout = value

        class Raw:
            _sock = Socket()

        class FP:
            raw = Raw()

        class SlowBodyResponse:
            fp = FP()

            def read(self, size=-1):
                if self.fp.raw._sock.timeout < 60:
                    raise TimeoutError("The read operation timed out")
                return b"complete"

            def close(self):
                pass

        with patch.object(router.time, "monotonic", return_value=0):
            response = router._DeadlineResponse(SlowBodyResponse(), 180)
            self.assertEqual(response.read(), b"complete")
        self.assertEqual(SlowBodyResponse.fp.raw._sock.timeout, 180)

    def test_upstream_body_timeout_reaches_nested_urllib_tls_socket(self):
        class Socket:
            timeout = None

            def settimeout(self, value):
                self.timeout = value

        class SocketIO:
            _sock = Socket()

        class BufferedReader:
            raw = SocketIO()

        class HTTPResponse:
            fp = BufferedReader()

        class UrlOpenResponse:
            fp = HTTPResponse()

        router._set_response_timeout(UrlOpenResponse(), 17)

        self.assertEqual(SocketIO._sock.timeout, 17)

    def test_upstream_body_timeout_is_reported_as_gateway_timeout(self):
        class TimedOutResponse:
            def read(self, size=-1):
                raise TimeoutError("The read operation timed out")

            def close(self):
                pass

        with patch.object(router.time, "monotonic", return_value=0):
            response = router._DeadlineResponse(TimedOutResponse(), 180)
            with self.assertRaises(RouterError) as raised:
                response.read()
        self.assertEqual(raised.exception.status, 504)

    def test_limited_response_iteration_preserves_streaming_chunks(self):
        class StreamingResponse:
            status = 200
            headers = {"Content-Type": "text/event-stream"}

            def __iter__(self):
                yield b"data: first\n"
                yield b"\n"

            def read(self, size=-1):
                raise AssertionError("stream iteration must not use buffered read()")

            def close(self):
                pass

        response = router._LimitedResponse(StreamingResponse(), 1024)
        self.assertEqual(list(response), [b"data: first\n", b"\n"])

        with self.assertRaises(RouterError):
            list(router._LimitedResponse(StreamingResponse(), 4))

    def test_upstream_model_drops_repeated_local_provider_prefix(self):
        provider = {"id": "gemini"}
        model = {"upstream_id": "gemini/gemini-3.5-flash"}
        self.assertEqual(
            router._upstream_model(provider, model, "gemini/gemini-3.5-flash"),
            "gemini-3.5-flash",
        )

    def test_gemini_model_metadata_reads_token_limits(self):
        provider = {
            "id": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def read(self):
                return json.dumps({"inputTokenLimit": 1048576, "outputTokenLimit": 65536}).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            value = router.model_metadata(provider, "gemini-3.5-flash")
        request = opened.call_args.args[0]
        self.assertEqual(request.full_url, "https://generativelanguage.googleapis.com/v1beta/models/gemini-3.5-flash")
        self.assertEqual(request.get_header("X-goog-api-key"), "test-key")
        self.assertEqual(value["context_window"], 1048576)
        self.assertEqual(value["output_token_limit"], 65536)

    def test_gemini_model_metadata_reports_reasoning_levels(self):
        provider = {
            "id": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def read(self):
                return json.dumps({
                    "inputTokenLimit": 1048576,
                    "outputTokenLimit": 65536,
                    "thinking": True,
                }).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()):
            value = router.model_metadata(provider, "gemini-3.7-flash")
        self.assertTrue(value["supports_reasoning"])
        self.assertEqual(value["reasoning_levels"], [])

    def test_generic_model_discovery_reads_openai_models(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def read(self):
                return json.dumps({"data": [{"id": "demo-a"}, {"id": "not a model"}]}).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            value = router.discover_models(provider)
        request = opened.call_args.args[0]
        self.assertEqual(request.full_url, "https://example.com/v1/models")
        self.assertEqual(request.get_header("Authorization"), "Bearer test-key")
        self.assertEqual([item["upstream_id"] for item in value], ["demo-a"])

    def test_auto_model_discovery_does_not_guess_the_generation_protocol(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "auto",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def read(self):
                return json.dumps({"data": [{"id": "demo-a"}]}).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", side_effect=[FakeResponse(), FakeResponse()]):
            value = router.discover_models(provider)
        self.assertEqual(provider["protocol"], "auto")
        self.assertEqual(value[0]["upstream_id"], "demo-a")

    def test_auto_provider_prefers_chat_completions_without_model_discovery(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router,
            "chat_completion",
            return_value=(200, "application/json", b"{}"),
        ) as completion:
            metadata, result = router.proxy(
                config,
                {"model": "demo/glm", "input": "hello"},
                {},
            )
        self.assertEqual(metadata["status"], 200)
        self.assertEqual(metadata["resolved_protocol"], "chat_completions")
        self.assertEqual(result, b"{}")
        self.assertEqual(completion.call_args.args[0]["protocol"], "chat_completions")

    def test_auto_stream_prefers_chat_completions(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        stream = iter([b'event: response.completed\ndata: {"type":"response.completed"}\n\n'])
        with patch.object(
            router,
            "stream_chat_completion",
            return_value=stream,
        ) as completion:
            metadata, result = router.proxy(
                config,
                {"model": "demo/glm", "input": "hello", "stream": True},
                {},
            )
            list(result)
        self.assertEqual(metadata["resolved_protocol"], "chat_completions")
        self.assertEqual(completion.call_args.args[0]["protocol"], "chat_completions")

    def test_auto_provider_prioritizes_matching_observed_protocol(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "auto",
            "auth_mode": "api_key",
            "api_key": "test-key",
            "resolved_protocol": "chat_completions",
            "protocol_observation": {
                "endpoint_fingerprint": endpoint_fingerprint("https://example.com/v1"),
                "deployment_identity": "default",
                "upstream_model": "glm",
                "source": "observed",
                "confidence": 1.0,
                "observed_at": "2026-08-21T00:00:00+00:00",
            },
        }
        config = {
            "providers": [provider],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router, "chat_completion", return_value=(200, "application/json", b"{}")
        ) as chat, patch.object(router, "forward_responses") as responses:
            metadata, _ = router.proxy(config, {"model": "demo/glm", "input": []}, {})
        self.assertEqual(metadata["resolved_protocol"], "chat_completions")
        self.assertEqual(metadata["protocol_decision"], "observed_priority")
        chat.assert_called_once()
        responses.assert_not_called()

    def test_stale_observed_protocol_is_ignored(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "auto",
            "auth_mode": "api_key",
            "api_key": "test-key",
            "resolved_protocol": "chat_completions",
            "protocol_observation": {
                "endpoint_fingerprint": endpoint_fingerprint("https://other.example/v1"),
                "deployment_identity": "default",
                "upstream_model": "glm",
            },
        }
        config = {
            "providers": [provider],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router, "chat_completion", return_value=(200, "application/json", b"{}")
        ) as chat, patch.object(router, "forward_responses") as responses:
            metadata, _ = router.proxy(config, {"model": "demo/glm", "input": []}, {})
        self.assertEqual(metadata["resolved_protocol"], "chat_completions")
        self.assertEqual(metadata["protocol_decision"], "normal_order")
        chat.assert_called_once()
        responses.assert_not_called()

    def test_auto_fallback_is_limited_to_protocol_rejection_statuses(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        for status in (404, 405, 415, 501):
            with self.subTest(status=status), patch.object(
                router,
                "chat_completion",
                side_effect=RouterError("protocol rejected", status),
            ), patch.object(
                router, "forward_responses", return_value=(200, "application/json", b"{}")
            ) as responses:
                metadata, _ = router.proxy(config, {"model": "demo/glm", "input": []}, {})
                self.assertEqual(metadata["resolved_protocol"], "responses")
                responses.assert_called_once()

    def test_auto_does_not_fallback_on_auth_waf_rate_limit_server_or_timeout(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        for status in (401, 403, 429, 500, 502, 503, 504):
            with self.subTest(status=status), patch.object(
                router,
                "chat_completion",
                side_effect=RouterError("request failed", status),
            ), patch.object(router, "forward_responses") as responses:
                with self.assertRaises(RouterError) as raised:
                    router.proxy(config, {"model": "demo/glm", "input": []}, {})
                self.assertEqual(raised.exception.status, status)
                responses.assert_not_called()

    def test_auto_does_not_fallback_on_network_exception(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router, "chat_completion", side_effect=OSError("network-secret")
        ), patch.object(router, "forward_responses") as responses:
            with self.assertRaises(OSError):
                router.proxy(config, {"model": "demo/glm", "input": []}, {})
            responses.assert_not_called()

    def test_auto_final_failure_emits_selected_safe_observation(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        observations = []
        with patch.object(
            router,
            "chat_completion",
            side_effect=RouterError("auth-secret", 401),
        ), patch.object(router, "forward_responses") as responses:
            with self.assertRaises(RouterError):
                router.proxy(
                    config,
                    {"model": "demo/glm", "input": []},
                    {},
                    observations.append,
                )
        responses.assert_not_called()
        self.assertEqual(len(observations), 1)
        self.assertEqual(observations[0]["resolved_protocol"], "chat_completions")
        self.assertEqual(observations[0]["status"], 401)
        self.assertEqual(observations[0]["error_class"], "auth")
        self.assertEqual(observations[0]["protocol_decision"], "normal_order")
        self.assertFalse(observations[0]["protocol_fallback"])

    def test_explicit_protocol_never_falls_back(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "responses",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router, "forward_responses", return_value=(404, "application/json", b"{}")
        ) as responses, patch.object(router, "chat_completion") as chat:
            metadata, raw = router.proxy(config, {"model": "demo/glm", "input": []}, {})
        self.assertEqual(metadata["resolved_protocol"], "responses")
        self.assertEqual(metadata["status"], 404)
        self.assertEqual(raw, b"{}")
        responses.assert_called_once()
        chat.assert_not_called()

    def test_auto_stream_can_fallback_to_responses_after_eager_protocol_rejection(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        observations = []
        stream = iter([b'event: response.completed\ndata: {"type":"response.completed"}\n\n'])
        with patch.object(
            router,
            "stream_chat_completion",
            side_effect=RouterError("protocol rejected", 404),
        ), patch.object(router, "forward_responses_stream", return_value=stream) as responses:
            metadata, result = router.proxy(
                config,
                {"model": "demo/glm", "input": [], "stream": True},
                {},
                observations.append,
            )
            output = b"".join(result)
        self.assertIn(b"response.completed", output)
        responses.assert_called_once()
        self.assertEqual(observations[-1]["resolved_protocol"], "responses")
        self.assertTrue(observations[-1]["protocol_fallback"])
        self.assertTrue(observations[-1]["success"])

    def test_auto_provider_falls_back_when_responses_endpoint_is_unavailable(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1/responses",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router,
            "forward_responses",
            side_effect=RouterError("upstream returned 404: not found", 404),
        ), patch.object(
            router,
            "chat_completion",
            return_value=(200, "application/json", b"{}"),
        ) as completion:
            metadata, result = router.proxy(
                config,
                {"model": "demo/glm", "input": "hello"},
                {},
            )
        self.assertEqual(metadata["resolved_protocol"], "chat_completions")
        self.assertEqual(result, b"{}")
        self.assertEqual(completion.call_args.args[0]["protocol"], "chat_completions")

    def test_auto_provider_does_not_hide_authentication_failures(self):
        config = {
            "providers": [{
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "test-key",
            }],
            "models": [{"id": "demo/glm", "provider": "demo", "upstream_id": "glm"}],
        }
        with patch.object(
            router,
            "chat_completion",
            side_effect=RouterError("upstream returned 401", 401),
        ), patch.object(router, "forward_responses") as responses:
            with self.assertRaises(RouterError) as raised:
                router.proxy(config, {"model": "demo/glm", "input": "hello"}, {})
        self.assertEqual(raised.exception.status, 401)
        responses.assert_not_called()

    def test_chat_request_retries_without_unsupported_reasoning_effort(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }
        first = HTTPError(
            "https://example.com/v1/chat/completions",
            400,
            "unsupported",
            {},
            BytesIO(b'{"error":{"message":"unsupported reasoning_effort"}}'),
        )

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return b'{"choices":[{"message":{"content":"ok"}}]}'

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", side_effect=[first, FakeResponse()]) as opened:
            status, _, raw = router.chat_completion(
                provider,
                {
                    "model": "demo/model",
                    "input": "hello",
                    "reasoning": {"effort": "medium"},
                },
                {"upstream_id": "model", "reasoning_levels": ["medium"]},
                {},
            )
        self.assertEqual(status, 200)
        self.assertIn(b'"output_text": "ok"', raw)
        retry_payload = json.loads(opened.call_args_list[1].args[0].data)
        self.assertNotIn("reasoning_effort", retry_payload)

    def test_model_discovery_ignores_unbounded_context_metadata(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def read(self):
                return json.dumps({"data": [{"id": "demo-a", "context_window": 10**300}]}).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()):
            value = router.discover_models(provider)
        self.assertEqual(value[0]["context_window"], 0)

    def test_gemini_model_discovery_paginates_and_filters_non_generation_models(self):
        provider = {
            "id": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

        class FakeResponse:
            def __init__(self, value):
                self.value = value

            def read(self):
                return json.dumps(self.value).encode()

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        pages = [
            FakeResponse({
                "models": [
                    {
                        "name": "models/gemini-3.7-flash",
                        "displayName": "Gemini 3.7 Flash",
                        "supportedGenerationMethods": ["generateContent"],
                        "inputTokenLimit": 1048576,
                        "thinking": True,
                    },
                    {
                        "name": "models/text-embedding",
                        "supportedGenerationMethods": ["embedContent"],
                    },
                ],
                "nextPageToken": "next-page",
            }),
            FakeResponse({
                "models": [{
                    "name": "models/gemini-2.5-flash",
                    "supportedGenerationMethods": ["generateContent"],
                }],
            }),
        ]
        with patch.object(router, "urlopen", side_effect=pages) as opened:
            value = router.discover_models(provider)
        self.assertEqual(opened.call_args_list[0].args[0].full_url, "https://generativelanguage.googleapis.com/v1beta/models")
        self.assertEqual(opened.call_args_list[1].args[0].full_url, "https://generativelanguage.googleapis.com/v1beta/models?pageToken=next-page")
        self.assertEqual([item["upstream_id"] for item in value], ["gemini-3.7-flash", "gemini-2.5-flash"])
        self.assertTrue(value[0]["supports_reasoning"])
        self.assertEqual(value[0]["reasoning_levels"], [])
        self.assertEqual(
            value[0]["input_modalities"],
            ["text", "image", "video", "audio", "pdf"],
        )
        self.assertEqual(
            value[0]["capability_sources"]["input_modalities"]["source"],
            "official",
        )
        self.assertEqual(value[1]["input_modalities"], ["text"])
        self.assertEqual(
            value[1]["capability_sources"]["input_modalities"]["source"],
            "unknown",
        )

    def test_unlisted_native_model_uses_unique_forward_provider(self):
        provider, model = find_route(self.config, "gpt-native")
        self.assertEqual(provider["id"], "chatgpt")
        self.assertEqual(model["upstream_id"], "gpt-native")

    def test_known_native_model_uses_implicit_current_login_route_without_provider(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "gpt-native",
                                "visibility": "list",
                                "supported_in_api": True,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = {
                "native_catalog_path": str(native_path),
                "codex_base_url": "https://chatgpt.com/backend-api/codex",
                "providers": [],
                "models": [],
            }

            provider, model = find_route(config, "gpt-native")

            self.assertEqual(provider["auth_mode"], "forward")
            self.assertEqual(provider["protocol"], "responses")
            self.assertEqual(
                provider["base_url"], "https://chatgpt.com/backend-api/codex"
            )
            self.assertTrue(provider["implicit_native"])
            self.assertEqual(model["upstream_id"], "gpt-native")
            self.assertEqual(config["providers"], [])

    def test_implicit_native_route_forwards_current_login_and_account_headers(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps({"models": [{"slug": "gpt-native"}]}),
                encoding="utf-8",
            )
            config = {
                "native_catalog_path": str(native_path),
                "codex_base_url": "https://chatgpt.com/backend-api/codex",
                "providers": [],
                "models": [],
            }
            incoming = {
                "Authorization": "Bearer current-login",
                "ChatGPT-Account-ID": "current-account",
            }

            class FakeResponse:
                status = 200
                headers = {"Content-Type": "application/json"}

                def __init__(self):
                    self.sent = False

                def read(self, size=-1):
                    if self.sent:
                        return b""
                    self.sent = True
                    return b"{}"

                def close(self):
                    pass

            with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
                metadata, raw = router.proxy(
                    config,
                    {"model": "gpt-native", "input": "hello"},
                    incoming,
                )

            request = opened.call_args.args[0]
            self.assertEqual(metadata["provider_id"], "codex-native")
            self.assertEqual(request.full_url, "https://chatgpt.com/backend-api/codex/responses")
            self.assertEqual(request.get_header("Authorization"), "Bearer current-login")
            self.assertEqual(request.get_header("Chatgpt-account-id"), "current-account")
            self.assertEqual(raw, b"{}")

    def test_subscription_prefix_selects_account_and_strips_prefix(self):
        config = {
            "codex_base_url": "https://chatgpt.com/backend-api/codex",
            "accounts": [{
                "id": "plus",
                "name": "Plus",
                "prefix": "secondary",
                "auth_file": "/tmp/plus-auth.json",
                "enabled": True,
            }],
            "providers": [],
            "models": [],
        }
        provider, model = find_route(config, "secondary/gpt-native")
        self.assertEqual(provider["auth_mode"], "account")
        self.assertEqual(provider["account"]["id"], "plus")
        self.assertEqual(provider["base_url"], "https://chatgpt.com/backend-api/codex")
        self.assertEqual(model["upstream_id"], "gpt-native")

    def test_subscription_compaction_uses_native_compact_endpoint(self):
        config = {
            "providers": [{
                "id": "chatgpt",
                "base_url": "https://chatgpt.com/backend-api/codex",
                "enabled": True,
                "auth_mode": "forward",
                "protocol": "responses",
            }],
            "models": [],
        }

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return b'{"output":[]}'

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            metadata, raw = proxy_compact(
                config,
                {"model": "gpt-native", "input": []},
                {"Authorization": "Bearer subscription-token"},
            )
        request = opened.call_args.args[0]
        self.assertEqual(request.full_url, "https://chatgpt.com/backend-api/codex/responses/compact")
        self.assertEqual(json.loads(request.data)["model"], "gpt-native")
        self.assertEqual(metadata["status"], 200)
        self.assertEqual(raw, b'{"output":[]}')

    def test_implicit_native_compaction_uses_native_compact_endpoint(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps({"models": [{"slug": "gpt-native"}]}),
                encoding="utf-8",
            )
            config = {
                "native_catalog_path": str(native_path),
                "codex_base_url": "https://chatgpt.com/backend-api/codex",
                "providers": [],
                "models": [],
            }

            class FakeResponse:
                status = 200
                headers = {"Content-Type": "application/json"}

                def __init__(self):
                    self.sent = False

                def read(self, size=-1):
                    if self.sent:
                        return b""
                    self.sent = True
                    return b'{"output":[]}'

                def close(self):
                    pass

            with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
                metadata, raw = proxy_compact(
                    config,
                    {"model": "gpt-native", "input": []},
                    {"Authorization": "Bearer current-login"},
                )

            request = opened.call_args.args[0]
            self.assertEqual(
                request.full_url,
                "https://chatgpt.com/backend-api/codex/responses/compact",
            )
            self.assertEqual(json.loads(request.data)["model"], "gpt-native")
            self.assertEqual(metadata["status"], 200)
            self.assertEqual(raw, b'{"output":[]}')

    def test_hidden_native_model_routes_without_becoming_user_selectable(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {"slug": "gpt-native", "visibility": "list"},
                            {"slug": "gpt-hidden", "visibility": "hide"},
                            {"slug": "gpt-unsupported", "supported_in_api": False},
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = {
                "native_catalog_path": str(native_path),
                "providers": [],
                "models": [],
            }

            provider, model = find_route(config, "gpt-hidden")
            self.assertTrue(provider["implicit_native"])
            self.assertEqual(model["upstream_id"], "gpt-hidden")

            for model_id in ("gpt-unknown", "gpt-unsupported"):
                with self.subTest(model_id=model_id):
                    with self.assertRaises(RouterError) as raised:
                        find_route(config, model_id)
                    self.assertEqual(raised.exception.status, 404)

    def test_explicit_and_prefixed_routes_precede_implicit_native_route(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps({"models": [{"slug": "gpt-native"}]}),
                encoding="utf-8",
            )
            config = {
                "native_catalog_path": str(native_path),
                "codex_base_url": "https://chatgpt.com/backend-api/codex",
                "providers": [
                    {
                        "id": "external",
                        "base_url": "https://example.com/v1",
                        "protocol": "responses",
                        "auth_mode": "api_key",
                    }
                ],
                "models": [
                    {
                        "id": "gpt-native",
                        "provider": "external",
                        "upstream_id": "external-native",
                    }
                ],
                "accounts": [
                    {
                        "id": "primary",
                        "prefix": "primary",
                        "auth_file": "/tmp/primary-auth.json",
                    }
                ],
            }

            explicit_provider, explicit_model = find_route(config, "gpt-native")
            prefixed_provider, prefixed_model = find_route(config, "primary/gpt-native")

            self.assertEqual(explicit_provider["id"], "external")
            self.assertEqual(explicit_model["upstream_id"], "external-native")
            self.assertEqual(prefixed_provider["auth_mode"], "account")
            self.assertEqual(prefixed_model["upstream_id"], "gpt-native")

    def test_chat_provider_supports_v1_and_v2_remote_compaction(self):
        summary_response = json.dumps({
            "id": "resp_summary",
            "object": "response",
            "status": "completed",
            "output": [],
            "output_text": "continue from the saved state",
        }).encode()
        history = [{
            "type": "message",
            "role": "user",
            "content": [{"type": "input_text", "text": "keep this request"}],
        }]
        with patch.object(
            router,
            "chat_completion",
            return_value=(200, "application/json", summary_response),
        ) as completion:
            metadata, raw = proxy_compact(
                self.config,
                {"model": "demo/model", "input": history},
                {},
            )
            compacted = json.loads(raw)
            self.assertEqual(metadata["status"], 200)
            self.assertIn("continue from the saved state", compacted["output"][-1]["content"][0]["text"])

            metadata, stream = router.proxy(
                self.config,
                {
                    "model": "demo/model",
                    "stream": True,
                    "input": history + [{"type": "compaction_trigger"}],
                },
                {},
            )
            events = list(sse_json_events(stream))

        done = [event for event in events if event.get("type") == "response.output_item.done"]
        self.assertEqual(len(done), 1)
        self.assertEqual(done[0]["item"]["type"], "compaction")
        self.assertNotIn("status", done[0]["item"])
        replay = responses_to_chat(
            {"model": "demo/model", "input": [done[0]["item"]]},
            "model",
        )
        self.assertIn("continue from the saved state", replay["messages"][0]["content"])
        opaque = responses_to_chat(
            {
                "model": "demo/model",
                "input": [{"type": "compaction", "encrypted_content": "provider-secret"}],
            },
            "model",
        )
        self.assertIn("another provider", opaque["messages"][0]["content"])
        summary_request = completion.call_args.args[1]
        self.assertNotIn("compaction_trigger", json.dumps(summary_request))

    def test_responses_request_becomes_chat_request(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "instructions": "Be concise",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}],
            "max_output_tokens": 32,
        }, "model")
        self.assertEqual(payload["messages"][0], {"role": "system", "content": "Be concise"})
        self.assertEqual(payload["messages"][1], {"role": "user", "content": "Hello"})
        self.assertEqual(payload["max_tokens"], 32)

    def test_function_call_history_becomes_chat_tool_messages(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "input": [
                {
                    "type": "function_call",
                    "id": "call_1",
                    "call_id": "call_1",
                    "name": "exec",
                    "arguments": '{"cmd":"pwd"}',
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_1",
                    "output": " /tmp",
                },
            ],
        }, "model")
        self.assertEqual(payload["messages"][0]["role"], "assistant")
        self.assertEqual(payload["messages"][0]["tool_calls"][0]["id"], "call_1")
        self.assertEqual(payload["messages"][0]["tool_calls"][0]["function"]["name"], "exec")
        self.assertEqual(payload["messages"][1], {
            "role": "tool",
            "tool_call_id": "call_1",
            "content": " /tmp",
        })

    def test_chat_translation_preserves_structured_tools(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "input": "Hello",
            "tools": [{
                "type": "function",
                "name": "exec",
                "description": "run a command",
                "parameters": {"type": "object"},
            }],
        }, "model")
        self.assertEqual(payload["tools"][0]["function"]["name"], "exec")

    def test_chat_translation_preserves_codex_custom_tools_and_history(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "input": [
                {
                    "type": "additional_tools",
                    "tools": [{
                        "type": "namespace",
                        "name": "functions",
                        "tools": [{"type": "custom", "name": "exec", "description": "Run code"}],
                    }],
                },
                {
                    "type": "custom_tool_call",
                    "id": "ctc_fixture",
                    "call_id": "call_fixture",
                    "name": "exec",
                    "input": "text('ok')",
                },
                {
                    "type": "custom_tool_call_output",
                    "call_id": "call_fixture",
                    "output": "ok",
                },
            ],
        }, "model")
        self.assertEqual(payload["tools"][0]["function"]["name"], "exec")
        self.assertEqual(
            json.loads(payload["messages"][0]["tool_calls"][0]["function"]["arguments"]),
            {"input": "text('ok')"},
        )
        self.assertEqual(payload["messages"][1]["tool_call_id"], "call_fixture")

    def test_chat_custom_tool_response_uses_codex_item_shape(self):
        response = _response_from_chat(
            {
                "choices": [{
                    "message": {
                        "tool_calls": [{
                            "id": "call_fixture",
                            "type": "function",
                            "function": {"name": "exec", "arguments": '{"input":"text(1)"}'},
                        }]
                    },
                    "finish_reason": "tool_calls",
                }]
            },
            "demo/model",
            custom_names={"exec"},
        )
        item = response["output"][0]
        self.assertEqual(item["type"], "custom_tool_call")
        self.assertTrue(item["id"].startswith("ctc_"))
        self.assertEqual(item["call_id"], "call_fixture")
        self.assertEqual(item["input"], "text(1)")

    def test_chat_length_finish_is_incomplete_not_completed(self):
        response = _response_from_chat(
            {
                "choices": [{
                    "message": {"content": "partial"},
                    "finish_reason": "length",
                }]
            },
            "demo/model",
        )
        self.assertEqual(response["status"], "incomplete")
        self.assertEqual(
            response["incomplete_details"], {"reason": "max_output_tokens"}
        )

    def test_responses_reasoning_effort_becomes_chat_reasoning_effort(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "input": "Hello",
            "reasoning": {"effort": "high"},
        }, "model")
        self.assertEqual(payload["reasoning_effort"], "high")

    def test_request_retries_transient_upstream_failure(self):
        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "api_key": "key", "base_url": "https://example.com/v1"}

        class FakeResponse:
            status = 200

        failure = HTTPError("https://example.com/v1/chat/completions", 503, "busy", {}, BytesIO(b"busy"))
        with patch.object(router, "urlopen", side_effect=[failure, FakeResponse()]) as opened:
            with patch.object(router.time, "sleep") as sleep:
                result = router._request(provider, {}, {}, False)
        self.assertIsInstance(result, FakeResponse)
        self.assertEqual(opened.call_count, 2)
        sleep.assert_called_once_with(0.5)

    def test_request_uses_compact_json_for_gateway_body_limits(self):
        provider = {
            "id": "demo",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "key",
            "base_url": "https://example.com/v1",
        }

        class FakeResponse:
            status = 200

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            router._request(provider, {"model": "demo", "input": [1, 2]}, {}, False)

        request = opened.call_args.args[0]
        self.assertEqual(request.data, b'{"model":"demo","input":[1,2]}')

    def test_request_retries_transient_connection_failure(self):
        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "api_key": "key", "base_url": "https://example.com/v1"}

        class FakeResponse:
            status = 200

        with patch.object(router, "urlopen", side_effect=[URLError("temporary TLS EOF"), FakeResponse()]) as opened:
            with patch.object(router.time, "sleep") as sleep:
                result = router._request(provider, {}, {}, False)
        self.assertIsInstance(result, FakeResponse)
        self.assertEqual(opened.call_count, 2)
        sleep.assert_called_once_with(0.5)

    def test_request_summarizes_html_error_without_echoing_the_page(self):
        provider = {
            "id": "demo",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "key",
            "base_url": "https://example.com/v1",
        }
        page = (
            b'<!DOCTYPE html><html><head><link href="data:image/png;base64,'
            + (b"A" * 10000)
            + b'"></head><body>Forbidden</body></html>'
        )
        failure = HTTPError(
            "https://example.com/v1/responses",
            403,
            "Forbidden",
            {"Content-Type": "text/html; charset=utf-8"},
            BytesIO(page),
        )

        with patch.object(router, "urlopen", side_effect=failure):
            with self.assertRaises(RouterError) as raised:
                router._request(provider, {"model": "demo"}, {}, False)

        message = str(raised.exception)
        self.assertEqual(raised.exception.status, 403)
        self.assertIn("upstream returned 403 (text/html)", message)
        self.assertIn("gateway or WAF", message)
        self.assertNotIn("base64", message)
        self.assertLess(len(message), 240)

    def test_subscription_request_uses_only_selected_account_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_path, {
                    "tokens": {"access_token": "selected-secret", "account_id": "selected-account"}
                })
            provider = {
                "id": "primary",
                "protocol": "responses",
                "auth_mode": "account",
                "base_url": "https://example.com/v1",
                "account": {"id": "primary", "auth_file": str(auth_path)},
            }
            model = {"id": "primary/gpt-native", "upstream_id": "gpt-native"}

            class FakeResponse:
                status = 200
                headers = {}

                def getheader(self, name, default=None):
                    return default

                def read(self):
                    return b'{}'

                def __enter__(self):
                    return self

                def __exit__(self, *args):
                    pass

            with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
                router.forward_responses(
                    provider,
                    {"model": "primary/gpt-native", "input": "hello"},
                    model,
                    {"Authorization": "Bearer wrong-account"},
                )
            request = opened.call_args.args[0]
            self.assertEqual(request.get_header("Authorization"), "Bearer selected-secret")
            self.assertEqual(request.get_header("Chatgpt-account-id"), "selected-account")

    def test_external_responses_provider_does_not_receive_codex_client_metadata(self):
        provider = {
            "id": "demo",
            "protocol": "responses",
            "auth_mode": "api_key",
            "base_url": "https://example.com/v1",
        }
        model = {"id": "demo/model", "upstream_id": "model"}

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return b"{}"

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "_request", return_value=FakeResponse()) as requested:
            router.forward_responses(
                provider,
                {
                    "model": "demo/model",
                    "input": "hello",
                    "client_metadata": {
                        "thread_id": "private-thread",
                        "x-codex-turn-metadata": "private-metadata",
                    },
                },
                model,
                {},
            )

        payload = requested.call_args.args[1]
        self.assertNotIn("client_metadata", payload)
        self.assertEqual(payload["model"], "model")
        self.assertEqual(payload["input"], "hello")

    def test_subscription_responses_preserve_native_codex_client_metadata(self):
        metadata = {"thread_id": "native-thread"}
        payload = router._responses_payload(
            {"id": "primary", "auth_mode": "account"},
            {
                "model": "primary/gpt-native",
                "input": "hello",
                "client_metadata": metadata,
            },
            {"id": "primary/gpt-native", "upstream_id": "gpt-native"},
        )
        self.assertEqual(payload["client_metadata"], metadata)

    def test_subscription_request_refreshes_account_once_after_401(self):
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_path, {"tokens": {"access_token": "stale-secret"}})
            provider = {
                "id": "primary",
                "protocol": "responses",
                "auth_mode": "account",
                "base_url": "https://example.com/v1",
                "account": {"id": "primary", "auth_file": str(auth_path)},
            }
            model = {"id": "primary/gpt-native", "upstream_id": "gpt-native"}

            class FakeResponse:
                status = 200
                headers = {}

                def read(self):
                    return b'{}'

                def __enter__(self):
                    return self

                def __exit__(self, *args):
                    pass

            failure = HTTPError("https://example.com/v1/responses", 401, "expired", {}, BytesIO(b""))
            with patch.object(router, "urlopen", side_effect=[failure, FakeResponse()]) as opened:
                with patch.object(router, "refresh_account_quota", return_value={} ) as refreshed:
                    router.forward_responses(
                        provider,
                        {"model": "primary/gpt-native", "input": "hello"},
                        model,
                        {},
                    )
            self.assertEqual(opened.call_count, 2)
            refreshed.assert_called_once_with(provider["account"])

    def test_chat_response_becomes_responses_response(self):
        result = _response_from_chat({"choices": [{"message": {"content": "Hello"}}]}, "demo/model")
        self.assertEqual(result["object"], "response")
        self.assertEqual(result["output_text"], "Hello")
        self.assertEqual(result["output"][0]["content"][0]["text"], "Hello")

    def test_chat_response_rejects_textual_tool_markup(self):
        with self.assertRaises(RouterError) as raised:
            _response_from_chat({
                "choices": [{"message": {"content": "<tool_call>exec</tool_call>"}}]
            }, "demo/model")
        self.assertEqual(raised.exception.status, 502)

    def test_chat_response_surfaces_json_error(self):
        with self.assertRaises(RouterError) as raised:
            _response_from_chat({"error": {"message": "invalid model"}}, "demo/model")
        self.assertEqual(str(raised.exception), "Chat Completions upstream error: invalid model")

    def test_responses_request_becomes_anthropic_messages(self):
        payload = responses_to_anthropic({
            "model": "ant/claude",
            "instructions": "Be concise",
            "input": [{"type": "message", "role": "user", "content": [{"type": "input_text", "text": "Hello"}]}],
            "tools": [{
                "type": "function",
                "name": "exec",
                "description": "run a command",
                "parameters": {"type": "object"},
            }],
            "max_output_tokens": 32,
        }, "claude")
        self.assertEqual(payload["system"], "Be concise")
        self.assertEqual(payload["messages"][0]["content"][0], {"type": "text", "text": "Hello"})
        self.assertEqual(payload["tools"][0]["name"], "exec")
        self.assertEqual(payload["max_tokens"], 32)

    def test_anthropic_response_becomes_responses_response(self):
        result = _response_from_anthropic({
            "id": "msg_1",
            "model": "claude",
            "content": [{"type": "text", "text": "Hello"}],
            "usage": {"input_tokens": 2, "output_tokens": 1},
        }, "ant/claude")
        self.assertEqual(result["output_text"], "Hello")
        self.assertEqual(result["output"][0]["content"][0]["text"], "Hello")

    def test_anthropic_max_tokens_is_incomplete_not_completed(self):
        result = _response_from_anthropic({
            "id": "msg_1",
            "model": "claude",
            "content": [{"type": "text", "text": "Partial"}],
            "stop_reason": "max_tokens",
        }, "ant/claude")
        self.assertEqual(result["status"], "incomplete")
        self.assertEqual(
            result["incomplete_details"], {"reason": "max_output_tokens"}
        )

    def test_anthropic_provider_uses_anthropic_auth_header(self):
        headers = router._headers(
            {
                "id": "anthropic",
                "protocol": "anthropic_messages",
                "auth_mode": "anthropic_api_key",
                "api_key": "anthropic-secret",
                "anthropic_version": "2023-06-01",
            },
            {},
            False,
        )
        self.assertEqual(headers["x-api-key"], "anthropic-secret")
        self.assertNotIn("Authorization", headers)

    def test_stream_has_codex_response_events(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"choices":[{"delta":{"content":"Hi"}}]}\n',
                    b'\n',
                    b'data: [DONE]\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_chat_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn("event: response.output_text.delta", output)
        self.assertIn('"item_id":', output)
        self.assertIn("event: response.completed", output)

    def test_stream_preserves_structured_chat_tool_calls(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","type":"function","function":{"name":"exec","arguments":"{\\"cmd\\":"}}]}}]}\n',
                    b'\n',
                    b'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\\"pwd\\"}"}}]}}]}\n',
                    b'\n',
                    b'data: [DONE]\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Run pwd", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_chat_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn("event: response.function_call_arguments.delta", output)
        self.assertIn("event: response.function_call_arguments.done", output)
        self.assertIn('"type": "function_call"', output)
        self.assertIn('"arguments": "{\\\"cmd\\\":\\\"pwd\\\"}"', output)
        self.assertIn("event: response.completed", output)

    def test_stream_maps_custom_chat_tool_call_back_to_codex(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_fixture","type":"function","function":{"name":"exec","arguments":"{\\"input\\":\\"text(1)\\"}"}}]},"finish_reason":"tool_calls"}]}\n',
                    b'\n',
                    b'data: [DONE]\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {
            "model": "demo/model",
            "input": "Run it",
            "stream": True,
            "tools": [{"type": "custom", "name": "exec", "description": "Run code"}],
        }
        with patch.object(router, "_request", return_value=FakeResponse()):
            events = list(sse_json_events(router.stream_chat_completion(provider, body, model, {})))
        added = next(
            event for event in events
            if event.get("type") == "response.output_item.added"
            and event.get("item", {}).get("type") == "custom_tool_call"
        )
        done = next(
            event for event in events
            if event.get("type") == "response.output_item.done"
            and event.get("item", {}).get("type") == "custom_tool_call"
        )
        self.assertTrue(added["item"]["id"].startswith("ctc_"))
        self.assertEqual(done["item"]["call_id"], "call_fixture")
        self.assertEqual(done["item"]["input"], "text(1)")
        self.assertTrue(any(event.get("type") == "response.completed" for event in events))

    def test_stream_length_finish_emits_incomplete_terminal(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"choices":[{"delta":{"content":"partial"},"finish_reason":"length"}]}\n',
                    b'\n',
                    b'data: [DONE]\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            events = list(sse_json_events(router.stream_chat_completion(provider, body, model, {})))
        self.assertEqual(events[-1]["type"], "response.incomplete")
        self.assertEqual(
            events[-1]["response"]["incomplete_details"]["reason"],
            "max_output_tokens",
        )
        self.assertFalse(any(event.get("type") == "response.completed" for event in events))

    def test_anthropic_stream_max_tokens_emits_incomplete_terminal(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}\n',
                    b'\n',
                    b'data: {"type":"message_delta","delta":{"stop_reason":"max_tokens"}}\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {
            "id": "demo",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "base_url": "https://example.com/v1",
        }
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            events = list(
                sse_json_events(
                    router.stream_anthropic_completion(provider, body, model, {})
                )
            )
        self.assertEqual(events[-1]["type"], "response.incomplete")
        self.assertEqual(
            events[-1]["response"]["incomplete_details"]["reason"],
            "max_output_tokens",
        )
        self.assertFalse(
            any(event.get("type") == "response.completed" for event in events)
        )

    def test_stream_rejects_textual_tool_markup(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'data: {"choices":[{"delta":{"content":"<tool_call>"}}]}\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Run pwd", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_chat_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn("textual reasoning/tool-call markup", output)

    def test_stream_accepts_non_sse_chat_response(self):
        class FakeResponse:
            def __iter__(self):
                return iter([b'{"choices":[{"message":{"content":"Hello"}}]}'])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_chat_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn('"delta": "Hello"', output)
        self.assertIn("event: response.completed", output)

        class ResponsesResponse:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self, value):
                self.value = value
                self.closed = False

            def read(self, size=-1):
                value, self.value = self.value, b""
                return value

            def close(self):
                self.closed = True

        response_body = json.dumps({
            "id": "resp-json",
            "object": "response",
            "status": "completed",
            "output": [{
                "id": "msg-json",
                "type": "message",
                "status": "completed",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "Hello", "annotations": []}],
            }],
            "output_text": "Hello",
        }).encode("utf-8")
        response = ResponsesResponse(response_body)
        converted = b"".join(router._validated_responses_stream(response)).decode("utf-8")
        self.assertIn("event: response.completed", converted)
        self.assertIn('"delta": "Hello"', converted)
        self.assertTrue(response.closed)

    def test_stream_surfaces_empty_upstream_response(self):
        class FakeResponse:
            def __iter__(self):
                return iter([b"data: [DONE]\n", b"\n"])

            def close(self):
                pass

        provider = {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"}
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_chat_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn("empty Chat Completions response", output)

        class ResponsesResponse:
            status = 200
            headers = {"Content-Type": "text/event-stream"}

            def __iter__(self):
                return iter([b"data: [DONE]\n", b"\n"])

            def close(self):
                pass

        output = b"".join(router._validated_responses_stream(ResponsesResponse())).decode("utf-8")
        self.assertIn("event: response.failed", output)
        self.assertNotIn("event: response.completed", output)

    def test_translated_stream_timeout_is_a_terminal_response_failure(self):
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "Hello", "stream": True}
        providers_and_streams = (
            (
                {"id": "demo", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"},
                router.stream_chat_completion,
            ),
            (
                {"id": "demo", "protocol": "anthropic_messages", "auth_mode": "anthropic_api_key", "base_url": "https://example.com/v1"},
                router.stream_anthropic_completion,
            ),
        )
        for provider, stream in providers_and_streams:
            with self.subTest(protocol=provider["protocol"]), patch.object(
                router,
                "_request",
                side_effect=RouterError("upstream request timed out", 504),
            ):
                output = b"".join(stream(provider, body, model, {})).decode("utf-8")
            self.assertIn("event: response.failed", output)
            self.assertIn('"status": "failed"', output)
            self.assertIn("HTTP 504", output)
            self.assertNotIn('"type": "error"', output)

    def test_responses_stream_accepts_headerless_sse(self):
        class HeaderlessResponse:
            status = 200
            headers = {}

            def __init__(self):
                self.body = (
                    b'event: response.completed\n'
                    b'data: {"type":"response.completed","response":'
                    b'{"id":"resp-headerless","status":"completed","output":[]}}\n\n'
                )
                self.closed = False

            def read(self, size=-1):
                body, self.body = self.body, b""
                return body

            def close(self):
                self.closed = True

        response = HeaderlessResponse()
        output = b"".join(router._validated_responses_stream(response)).decode("utf-8")
        self.assertIn("event: response.completed", output)
        self.assertNotIn("event: response.failed", output)
        self.assertTrue(response.closed)

    def test_anthropic_stream_has_codex_response_events(self):
        class FakeResponse:
            def __iter__(self):
                return iter([
                    b'event: content_block_delta\n',
                    b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"Hi"}}\n',
                    b'\n',
                    b'event: message_stop\n',
                    b'data: {"type":"message_stop"}\n',
                    b'\n',
                ])

            def close(self):
                pass

        provider = {
            "id": "anthropic",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "api_key": "test-key",
            "base_url": "https://example.com/v1",
        }
        model = {"id": "ant/claude"}
        body = {"model": "ant/claude", "input": "Hello", "stream": True}
        with patch.object(router, "_request", return_value=FakeResponse()):
            output = b"".join(router.stream_anthropic_completion(provider, body, model, {})).decode("utf-8")
        self.assertIn("event: response.output_text.delta", output)
        self.assertIn("event: response.completed", output)


if __name__ == "__main__":
    unittest.main()
