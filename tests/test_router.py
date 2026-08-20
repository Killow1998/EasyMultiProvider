import unittest
import json
import tempfile
from io import BytesIO
from pathlib import Path
from urllib.error import HTTPError
from unittest.mock import patch

from tests.support import ensure_test_master_key
import easy_multi_provider.router as router
from easy_multi_provider.router import (
    RouterError,
    _response_from_anthropic,
    _response_from_chat,
    find_route,
    responses_to_anthropic,
    responses_to_chat,
)
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
        self.assertEqual(headers["User-Agent"], "EasyMultiProvider/0.3.0")

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
        self.assertEqual(value["reasoning_levels"], ["low", "medium", "high"])

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

    def test_auto_model_discovery_resolves_to_chat_completions(self):
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
        self.assertEqual(provider["protocol"], "chat_completions")
        self.assertEqual(value[0]["upstream_id"], "demo-a")

    def test_auto_provider_can_proxy_without_model_discovery(self):
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
        self.assertEqual(result, b"{}")
        self.assertEqual(completion.call_args.args[0]["protocol"], "chat_completions")

    def test_auto_endpoint_does_not_require_model_discovery(self):
        provider = {
            "base_url": "https://example.com/v1",
            "protocol": "auto",
        }
        self.assertEqual(router._endpoint(provider), "https://example.com/v1/chat/completions")

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
                {"upstream_id": "model"},
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
        self.assertEqual(value[0]["reasoning_levels"], ["low", "medium", "high"])

    def test_unlisted_native_model_uses_unique_forward_provider(self):
        provider, model = find_route(self.config, "gpt-native")
        self.assertEqual(provider["id"], "chatgpt")
        self.assertEqual(model["upstream_id"], "gpt-native")

    def test_subscription_prefix_selects_account_and_strips_prefix(self):
        config = {
            "codex_base_url": "https://chatgpt.com/backend-api/codex",
            "accounts": [{
                "id": "plus",
                "name": "Plus",
                "prefix": "plus258",
                "auth_file": "/tmp/plus-auth.json",
                "enabled": True,
            }],
            "providers": [],
            "models": [],
        }
        provider, model = find_route(config, "plus258/gpt-native")
        self.assertEqual(provider["auth_mode"], "account")
        self.assertEqual(provider["account"]["id"], "plus")
        self.assertEqual(provider["base_url"], "https://chatgpt.com/backend-api/codex")
        self.assertEqual(model["upstream_id"], "gpt-native")

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

    def test_disabled_tool_mode_omits_chat_tools(self):
        payload = responses_to_chat({
            "model": "demo/model",
            "input": "Hello",
            "tools": [{
                "type": "function",
                "name": "exec",
                "description": "run a command",
                "parameters": {"type": "object"},
            }],
        }, "model", {"tool_call_mode": "disabled"})
        self.assertNotIn("tools", payload)

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

    def test_subscription_request_uses_only_selected_account_credentials(self):
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_path, {
                    "tokens": {"access_token": "selected-secret", "account_id": "selected-account"}
                })
            provider = {
                "id": "ship",
                "protocol": "responses",
                "auth_mode": "account",
                "base_url": "https://example.com/v1",
                "account": {"id": "ship", "auth_file": str(auth_path)},
            }
            model = {"id": "ship/gpt-native", "upstream_id": "gpt-native"}

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
                    {"model": "ship/gpt-native", "input": "hello"},
                    model,
                    {"Authorization": "Bearer wrong-account"},
                )
            request = opened.call_args.args[0]
            self.assertEqual(request.get_header("Authorization"), "Bearer selected-secret")
            self.assertEqual(request.get_header("Chatgpt-account-id"), "selected-account")

    def test_subscription_request_refreshes_account_once_after_401(self):
        with tempfile.TemporaryDirectory() as directory:
            auth_path = Path(directory) / "auth.json.enc"
            write_encrypted_json(auth_path, {"tokens": {"access_token": "stale-secret"}})
            provider = {
                "id": "ship",
                "protocol": "responses",
                "auth_mode": "account",
                "base_url": "https://example.com/v1",
                "account": {"id": "ship", "auth_file": str(auth_path)},
            }
            model = {"id": "ship/gpt-native", "upstream_id": "gpt-native"}

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
                        {"model": "ship/gpt-native", "input": "hello"},
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
            "max_output_tokens": 32,
        }, "claude")
        self.assertEqual(payload["system"], "Be concise")
        self.assertEqual(payload["messages"][0]["content"][0], {"type": "text", "text": "Hello"})
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
