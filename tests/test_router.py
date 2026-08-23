import base64
import copy
import unittest
import json
import tempfile
from io import BytesIO
from pathlib import Path
from urllib.error import HTTPError, URLError
from unittest.mock import patch

import zstandard

from tests.support import ensure_test_master_key
import easy_multi_provider.router as router
from easy_multi_provider.capabilities import endpoint_fingerprint
from easy_multi_provider.config import normalize
from easy_multi_provider.continuity import (
    BindingMissing,
    CompactionBindingStore,
    NativeCompactionObserver,
    binding_for_opaque,
    derive_native_route_identity,
    register_native_json,
)
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


class _ContinuityResponse:
    def __init__(self, value, status=200, content_type="application/json"):
        self.status = status
        self.headers = {"Content-Type": content_type}
        self._raw = value if isinstance(value, bytes) else json.dumps(value).encode("utf-8")
        self._sent = False
        self.closed = False

    def read(self, size=-1):
        if self._sent:
            return b""
        self._sent = True
        return self._raw

    def close(self):
        self.closed = True


def _continuity_request_json(request):
    raw = request.data
    headers = {key.lower(): value for key, value in request.header_items()}
    if headers.get("content-encoding") == "zstd":
        raw = zstandard.ZstdDecompressor().decompress(raw)
    return json.loads(raw.decode("utf-8"))


def _register_router_binding(
    directory,
    opaque,
    source_provider,
    source_model,
    source_slug,
    account_id,
):
    store = CompactionBindingStore(Path(directory) / "bindings.enc")
    identity = derive_native_route_identity(
        {"chatgpt-account-id": account_id},
        source_provider["base_url"],
        router.deployment_identity(source_provider, source_model),
        router._upstream_model(source_provider, source_model, source_slug),
        source_slug,
    )
    store.register(binding_for_opaque(identity, opaque))
    return store


def _continuity_config():
    return {
        "providers": [
            {
                "id": "native-current",
                "enabled": True,
                "auth_mode": "forward",
                "protocol": "responses",
                "base_url": "https://native-current.example/backend-api/codex",
                "deployment_identity": "current-deployment",
            },
            {
                "id": "native-selected",
                "enabled": True,
                "auth_mode": "account",
                "account": {"id": "selected-account", "name": "Private Selected"},
                "protocol": "responses",
                "base_url": "https://native-selected.example/backend-api/codex",
                "deployment_identity": "selected-deployment",
            },
            {
                "id": "external",
                "enabled": True,
                "auth_mode": "api_key",
                "api_key": "external-key",
                "protocol": "chat_completions",
                "base_url": "https://external.example/v1/responses",
            },
        ],
        "models": [
            {
                "id": "native/current-source",
                "provider": "native-current",
                "upstream_id": "current-source-upstream",
                "enabled": True,
            },
            {
                "id": "native/current-destination",
                "provider": "native-current",
                "upstream_id": "current-destination-upstream",
                "enabled": True,
            },
            {
                "id": "native/selected-source",
                "provider": "native-selected",
                "upstream_id": "selected-source-upstream",
                "enabled": True,
            },
            {
                "id": "native/selected-destination",
                "provider": "native-selected",
                "upstream_id": "selected-destination-upstream",
                "enabled": True,
            },
            {
                "id": "external/destination",
                "provider": "external",
                "upstream_id": "external-upstream",
                "enabled": True,
            },
        ],
    }


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
                "OpenAI-Beta": "responses=experimental",
                "Originator": "codex_cli_rs",
            },
            False,
        )
        self.assertEqual(headers["Authorization"], "Bearer session-token")
        self.assertEqual(headers["chatgpt-account-id"], "account-1")
        self.assertEqual(headers["OpenAI-Beta"], "responses=experimental")
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

    def test_external_auto_stream_does_not_create_native_observer(self):
        config = {
            "providers": [{
                "id": "external",
                "base_url": "https://external.example/v1/responses",
                "protocol": "auto",
                "auth_mode": "api_key",
                "api_key": "fixture-key",
                "resolved_protocol": "responses",
                "protocol_observation": {
                    "endpoint_fingerprint": endpoint_fingerprint("https://external.example/v1/responses"),
                    "deployment_identity": "default",
                    "upstream_model": "model-a",
                },
            }],
            "models": [{
                "id": "external/model-a",
                "provider": "external",
                "upstream_id": "model-a",
                "supported_protocols": ["responses"],
            }],
        }
        raw = b'event: response.completed\ndata: {"type":"response.completed","response":{"status":"completed"}}\n\n'
        events = []
        native_starts = []

        class FakeResponse(BytesIO):
            status = 200
            headers = {"Content-Type": "text/event-stream"}

        with patch.object(router, "urlopen", return_value=FakeResponse(raw)):
            _, result = router.proxy(
                config,
                {"model": "external/model-a", "input": [], "stream": True},
                {},
                on_stream_event=events.append,
                on_native_stream_start=lambda identity: native_starts.append(identity),
            )
            list(result)
        self.assertEqual([event["type"] for event in events], ["response.completed"])
        self.assertEqual(native_starts, [])

    def test_proxy_resolved_passes_native_stream_factory_to_responses_helper(self):
        provider = {
            "id": "native",
            "base_url": "https://native.example/backend-api/codex",
            "protocol": "responses",
            "auth_mode": "forward",
        }
        model = {"id": "native/model-a", "upstream_id": "model-a"}
        factory = object()
        event_callback = object()
        with patch.object(router, "forward_responses_stream", return_value=iter([])) as forward:
            _, result = router._proxy_resolved(
                provider,
                model,
                {"model": "native/model-a", "input": [], "stream": True},
                {},
                on_stream_event=event_callback,
                on_native_stream_start=factory,
            )
            list(result)
        self.assertIs(forward.call_args.kwargs["on_stream_event"], event_callback)
        self.assertIs(forward.call_args.kwargs["on_native_stream_start"], factory)

    def test_normalized_forward_native_provider_is_fixed_to_responses(self):
        config = normalize({
            "providers": [{
                "id": "native",
                "base_url": "https://native.example/backend-api/codex",
                "protocol": "responses",
                "auth_mode": "forward",
            }],
            "models": [{"id": "native/model-a", "provider": "native", "upstream_id": "model-a"}],
        })
        provider, _ = find_route(config, "native/model-a")
        self.assertEqual(provider["protocol"], "responses")

    def test_handoff_failure_observation_is_minimal_and_private(self):
        configured = {
            "providers": [{
                "id": "private-provider",
                "base_url": "https://private.example/backend-api/codex",
                "protocol": "responses",
                "auth_mode": "forward",
                "deployment_identity": "private-deployment",
            }],
            "models": [{
                "id": "private-provider/private-model",
                "provider": "private-provider",
                "upstream_id": "private-upstream",
            }],
        }
        observations = []
        with self.assertRaises(router.ContextHandoffRequiredError):
            router.proxy(
                configured,
                {
                    "model": "private-provider/private-model",
                    "input": [{"type": "compaction", "encrypted_content": "private-content"}],
                },
                {},
                on_observation=observations.append,
            )
        self.assertEqual(len(observations), 1)
        self.assertEqual(
            set(observations[0]),
            {"route", "transport", "status", "success", "error_class", "terminal_event_observed"},
        )
        serialized = json.dumps(observations[0])
        for secret in (
            "private-provider", "private-model", "private-deployment",
            "private-upstream", "private.example", "private-content",
        ):
            self.assertNotIn(secret, serialized)

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
        # Registry enriches gemini-3.7-flash with reasoning levels
        self.assertEqual(
            value[0]["reasoning_levels"],
            ["minimal", "low", "medium", "high"],
        )
        # Registry enriches with official input modalities (no hardcoded fallback)
        self.assertEqual(
            value[0]["input_modalities"],
            ["text", "image"],
        )
        self.assertEqual(
            value[0]["capability_sources"]["input_modalities"]["source"],
            "official",
        )
        # gemini-2.5-flash is in the registry with confirmed audio/video input
        self.assertEqual(value[1]["input_modalities"], ["text", "image", "audio", "video"])
        self.assertEqual(
            value[1]["capability_sources"]["input_modalities"]["source"],
            "official",
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
        request_body = json.loads(
            zstandard.ZstdDecompressor().decompress(request.data)
        )
        self.assertEqual(request_body["model"], "gpt-native")
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
            request_body = json.loads(
                zstandard.ZstdDecompressor().decompress(request.data)
            )
            self.assertEqual(request_body["model"], "gpt-native")
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

    def test_unknown_opaque_compaction_to_translated_external_requires_handoff_before_upstream(self):
        opaque = "private-opaque-state"
        for protocol in ("chat_completions", "anthropic_messages"):
            with self.subTest(protocol=protocol):
                config = {
                    "providers": [
                        {
                            "id": "external",
                            "enabled": True,
                            "auth_mode": "api_key",
                            "api_key": "key",
                            "protocol": protocol,
                            "base_url": "https://example.com/v1",
                        }
                    ],
                    "models": [
                        {
                            "id": "external/model",
                            "provider": "external",
                            "enabled": True,
                        }
                    ],
                }
                with patch.object(
                    router,
                    "urlopen",
                    side_effect=AssertionError("destination contacted"),
                ) as opened:
                    with self.assertRaises(RouterError) as raised:
                        router.proxy(
                            config,
                            {
                                "model": "external/model",
                                "input": [
                                    {
                                        "type": "compaction",
                                        "encrypted_content": opaque,
                                    }
                                ],
                            },
                            {},
                        )

                opened.assert_not_called()
                self.assertEqual(raised.exception.status, 409)
                self.assertEqual(
                    getattr(raised.exception, "error_class", None),
                    "context_handoff_required",
                )
                self.assertEqual(
                    getattr(raised.exception, "reason", None), "binding_missing"
                )
                message = str(raised.exception)
                self.assertIn("context_handoff_required", message)
                self.assertIn("index=0", message)
                self.assertNotIn(opaque, message)

    def test_unknown_opaque_compaction_to_portable_responses_requires_handoff(self):
        config = {
            "providers": [
                {
                    "id": "external",
                    "enabled": True,
                    "auth_mode": "api_key",
                    "api_key": "key",
                    "protocol": "responses",
                    "base_url": "https://example.com/v1",
                }
            ],
            "models": [
                {"id": "external/model", "provider": "external", "enabled": True}
            ],
        }
        opaque = "private-opaque-state"

        with patch.object(router, "urlopen") as opened:
            with self.assertRaises(RouterError) as raised:
                router.proxy(
                    config,
                    {
                        "model": "external/model",
                        "input": [
                            {"type": "compaction", "encrypted_content": opaque}
                        ],
                    },
                    {},
                )

        opened.assert_not_called()
        self.assertEqual(raised.exception.status, 409)
        self.assertEqual(
            getattr(raised.exception, "error_class", None),
            "context_handoff_required",
        )
        self.assertNotIn(opaque, str(raised.exception))

    def test_continuity_same_domain_preserves_opaque_without_bridge_or_mutation(self):
        opaque = "opaque-same-domain-private"
        config = _continuity_config()
        source_provider, source_model = find_route(
            config, "native/current-source"
        )
        body = {
            "model": "native/current-destination",
            "input": [
                {"type": "message", "role": "user", "content": "before"},
                {
                    "id": "cmp_same",
                    "type": "compaction",
                    "encrypted_content": opaque,
                },
                {"type": "message", "role": "user", "content": "after"},
            ],
        }
        body_before = copy.deepcopy(body)
        config_before = copy.deepcopy(config)
        incoming = {
            "Authorization": "Bearer current-token-private",
            "ChatGPT-Account-ID": "current-account-private",
        }
        observations = []

        with tempfile.TemporaryDirectory() as directory:
            store = _register_router_binding(
                directory,
                opaque,
                source_provider,
                source_model,
                "native/current-source",
                "current-account-private",
            )
            with patch.object(
                router,
                "urlopen",
                return_value=_ContinuityResponse(
                    {"status": "completed", "output": []}
                ),
            ) as opened:
                metadata, _ = router.proxy(
                    config,
                    body,
                    incoming,
                    on_observation=observations.append,
                    on_context=lambda *args: {"input_estimate": 3},
                    binding_store=store,
                )

        self.assertEqual(opened.call_count, 1)
        payload = _continuity_request_json(opened.call_args.args[0])
        self.assertEqual(payload["input"][1], body["input"][1])
        self.assertEqual(
            payload["input"][1]["encrypted_content"].encode("utf-8"),
            opaque.encode("utf-8"),
        )
        self.assertEqual(body, body_before)
        self.assertEqual(config, config_before)
        self.assertFalse(
            any("_context_observation" in provider for provider in config["providers"])
        )
        diagnostic = json.dumps(
            {"metadata": metadata, "observations": observations},
            ensure_ascii=False,
        )
        for private in (
            opaque,
            "current-account-private",
            "current-token-private",
            "https://native-current.example",
        ):
            self.assertNotIn(private, diagnostic)

    def test_continuity_cross_domain_matrix_uses_exact_source_route_and_auth(self):
        transitions = (
            (
                "N-to-S",
                "native/current-source",
                "native/selected-destination",
                "current-account-private",
                "https://native-current.example/backend-api/codex/responses",
                "Bearer current-token-private",
            ),
            (
                "S-to-N",
                "native/selected-source",
                "native/current-destination",
                "selected-account-private",
                "https://native-selected.example/backend-api/codex/responses",
                "Bearer selected-token-private",
            ),
            (
                "N-to-E",
                "native/current-source",
                "external/destination",
                "current-account-private",
                "https://native-current.example/backend-api/codex/responses",
                "Bearer current-token-private",
            ),
            (
                "S-to-E",
                "native/selected-source",
                "external/destination",
                "selected-account-private",
                "https://native-selected.example/backend-api/codex/responses",
                "Bearer selected-token-private",
            ),
        )
        incoming = {
            "Authorization": "Bearer current-token-private",
            "ChatGPT-Account-ID": "current-account-private",
        }

        for (
            name,
            source_slug,
            destination_slug,
            source_account_id,
            source_url,
            source_authorization,
        ) in transitions:
            with self.subTest(transition=name), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                source_provider, source_model = find_route(config, source_slug)
                opaque = "opaque-%s-private" % name
                store = _register_router_binding(
                    directory,
                    opaque,
                    source_provider,
                    source_model,
                    source_slug,
                    source_account_id,
                )
                requests = []
                observations = []

                def selected_headers(account):
                    self.assertEqual(account["id"], "selected-account")
                    return {
                        "Authorization": "Bearer selected-token-private",
                        "chatgpt-account-id": "selected-account-private",
                    }

                def open_request(request, timeout):
                    requests.append(request)
                    if request.full_url == source_url:
                        return _ContinuityResponse(
                            {
                                "status": "completed",
                                "output_text": "checkpoint-%s" % name,
                                "output": [],
                            }
                        )
                    if request.full_url.endswith("/chat/completions"):
                        return _ContinuityResponse(
                            {"choices": [{"message": {"content": "destination"}}]}
                        )
                    return _ContinuityResponse(
                        {"status": "completed", "output": []}
                    )

                with patch.object(
                    router, "auth_headers", side_effect=selected_headers
                ), patch.object(
                    router, "refresh_account_quota"
                ) as refreshed, patch.object(
                    router, "urlopen", side_effect=open_request
                ):
                    metadata, _ = router.proxy(
                        config,
                        {
                            "model": destination_slug,
                            "input": [
                                {
                                    "type": "compaction",
                                    "encrypted_content": opaque,
                                }
                            ],
                        },
                        incoming,
                        on_observation=observations.append,
                        binding_store=store,
                    )

                self.assertEqual(len(requests), 2)
                self.assertEqual(requests[0].full_url, source_url)
                self.assertEqual(
                    requests[0].get_header("Authorization"), source_authorization
                )
                refreshed.assert_not_called()
                bridge = _continuity_request_json(requests[0])
                self.assertEqual(
                    bridge["model"], source_model["upstream_id"]
                )
                self.assertEqual(len(bridge["input"]), 2)
                self.assertEqual(
                    bridge["input"][0]["encrypted_content"], opaque
                )
                self.assertEqual(bridge["input"][1]["role"], "user")
                self.assertFalse(bridge["stream"])
                self.assertEqual(bridge["tools"], [])
                self.assertGreater(bridge["max_output_tokens"], 0)
                self.assertLessEqual(bridge["max_output_tokens"], 4096)
                for forbidden in (
                    "previous_response_id",
                    "mcp_servers",
                    "web_search",
                    "tool_choice",
                ):
                    self.assertNotIn(forbidden, bridge)
                destination = _continuity_request_json(requests[1])
                destination_json = json.dumps(destination, ensure_ascii=False)
                self.assertIn("checkpoint-%s" % name, destination_json)
                self.assertNotIn(opaque, destination_json)
                self.assertEqual(metadata["status"], 200)
                diagnostics = json.dumps(
                    {"metadata": metadata, "observations": observations},
                    ensure_ascii=False,
                )
                for private in (
                    opaque,
                    "checkpoint-%s" % name,
                    router._CONTINUITY_BRIDGE_PROMPT,
                    source_account_id,
                    source_url,
                ):
                    self.assertNotIn(private, diagnostics)

    def test_continuity_binding_drift_fails_before_any_upstream(self):
        drifts = ("trust", "endpoint", "deployment")
        for drift in drifts:
            with self.subTest(drift=drift), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                source_slug = "native/selected-source"
                source_provider, source_model = find_route(config, source_slug)
                opaque = "opaque-drift-%s-private" % drift
                store = _register_router_binding(
                    directory,
                    opaque,
                    copy.deepcopy(source_provider),
                    copy.deepcopy(source_model),
                    source_slug,
                    "selected-account-private",
                )
                selected_account = "selected-account-private"
                if drift == "trust":
                    selected_account = "different-selected-account-private"
                elif drift == "endpoint":
                    source_provider["base_url"] = (
                        "https://drifted-source.example/backend-api/codex"
                    )
                else:
                    source_provider["deployment_identity"] = "drifted-deployment"

                with patch.object(
                    router,
                    "auth_headers",
                    return_value={
                        "Authorization": "Bearer selected-token-private",
                        "chatgpt-account-id": selected_account,
                    },
                ), patch.object(router, "urlopen") as opened:
                    with self.assertRaises(RouterError) as raised:
                        router.proxy(
                            config,
                            {
                                "model": "external/destination",
                                "input": [
                                    {
                                        "type": "compaction",
                                        "encrypted_content": opaque,
                                    }
                                ],
                            },
                            {
                                "Authorization": "Bearer current-token-private",
                                "ChatGPT-Account-ID": "current-account-private",
                            },
                            binding_store=store,
                        )

                opened.assert_not_called()
                self.assertEqual(raised.exception.status, 409)
                self.assertEqual(raised.exception.reason, "binding_stale")
                error = str(raised.exception)
                for private in (
                    opaque,
                    selected_account,
                    "selected-token-private",
                    "drifted-source.example",
                    "drifted-deployment",
                ):
                    self.assertNotIn(private, error)

    def test_continuity_missing_or_unavailable_source_fails_before_destination(self):
        incoming = {
            "Authorization": "Bearer current-token-private",
            "ChatGPT-Account-ID": "current-account-private",
        }
        cases = (
            "lookup_missing",
            "source_non_native",
            "source_auth_missing",
            "source_disabled",
        )
        for case in cases:
            with self.subTest(case=case), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                opaque = "opaque-unavailable-%s-private" % case
                store = CompactionBindingStore(Path(directory) / "bindings.enc")
                if case == "source_non_native":
                    external_provider, external_model = find_route(
                        config, "external/destination"
                    )
                    identity = derive_native_route_identity(
                        {"chatgpt-account-id": "selected-account-private"},
                        "https://former-native.example/backend-api/codex",
                        "former-deployment",
                        "former-upstream",
                        "external/destination",
                    )
                    store.register(binding_for_opaque(identity, opaque))
                elif case in {"source_auth_missing", "source_disabled"}:
                    source_provider, source_model = find_route(
                        config, "native/selected-source"
                    )
                    store = _register_router_binding(
                        directory,
                        opaque,
                        source_provider,
                        source_model,
                        "native/selected-source",
                        "selected-account-private",
                    )
                    if case == "source_disabled":
                        source_model["enabled"] = False

                auth = (
                    {"Authorization": "Bearer selected-token-private"}
                    if case == "source_auth_missing"
                    else {
                        "Authorization": "Bearer selected-token-private",
                        "chatgpt-account-id": "selected-account-private",
                    }
                )
                with patch.object(
                    router, "auth_headers", return_value=auth
                ), patch.object(router, "urlopen") as opened:
                    with self.assertRaises(RouterError) as raised:
                        router.proxy(
                            config,
                            {
                                "model": "external/destination",
                                "input": [
                                    {
                                        "type": "compaction",
                                        "encrypted_content": opaque,
                                    }
                                ],
                            },
                            incoming,
                            binding_store=store,
                        )

                opened.assert_not_called()
                expected = (
                    "binding_missing"
                    if case == "lookup_missing"
                    else "source_unavailable"
                )
                self.assertEqual(raised.exception.reason, expected)
                self.assertNotIn(opaque, str(raised.exception))

    def test_continuity_bridge_http_failures_are_single_attempt_without_refresh(self):
        incoming = {
            "Authorization": "Bearer current-token-private",
            "ChatGPT-Account-ID": "current-account-private",
        }
        for status in (401, 404, 429, 500, 502, 503):
            with self.subTest(status=status), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                source_provider, source_model = find_route(
                    config, "native/selected-source"
                )
                opaque = "opaque-http-%d-private" % status
                store = _register_router_binding(
                    directory,
                    opaque,
                    source_provider,
                    source_model,
                    "native/selected-source",
                    "selected-account-private",
                )

                def fail(request, timeout):
                    raise HTTPError(
                        request.full_url,
                        status,
                        "private bridge failure",
                        {"Content-Type": "text/plain"},
                        BytesIO(b"private bridge body"),
                    )

                with patch.object(
                    router,
                    "auth_headers",
                    return_value={
                        "Authorization": "Bearer selected-token-private",
                        "chatgpt-account-id": "selected-account-private",
                    },
                ), patch.object(
                    router, "refresh_account_quota"
                ) as refreshed, patch.object(
                    router, "urlopen", side_effect=fail
                ) as opened, patch.object(router.time, "sleep") as sleep:
                    with self.assertRaises(RouterError) as raised:
                        router.proxy(
                            config,
                            {
                                "model": "external/destination",
                                "input": [
                                    {
                                        "type": "compaction",
                                        "encrypted_content": opaque,
                                    }
                                ],
                            },
                            incoming,
                            binding_store=store,
                        )

                self.assertEqual(opened.call_count, 1)
                self.assertEqual(
                    opened.call_args.args[0].full_url,
                    "https://native-selected.example/backend-api/codex/responses",
                )
                refreshed.assert_not_called()
                sleep.assert_not_called()
                self.assertEqual(raised.exception.reason, "source_unavailable")
                error = str(raised.exception)
                for private in (
                    opaque,
                    "private bridge failure",
                    "private bridge body",
                    "selected-account-private",
                    "selected-token-private",
                    "native-selected.example",
                ):
                    self.assertNotIn(private, error)

    def test_continuity_invalid_bridge_summary_fails_before_destination(self):
        invalid_responses = (
            {"status": "completed", "output_text": "", "output": []},
            {"status": "failed", "output_text": "private checkpoint", "output": []},
            {
                "status": "completed",
                "output_text": "x" * (128 * 1024 + 1),
                "output": [],
            },
            b"not-json-private-checkpoint",
        )
        for index, response in enumerate(invalid_responses):
            with self.subTest(case=index), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                source_provider, source_model = find_route(
                    config, "native/current-source"
                )
                opaque = "opaque-invalid-summary-%d-private" % index
                store = _register_router_binding(
                    directory,
                    opaque,
                    source_provider,
                    source_model,
                    "native/current-source",
                    "current-account-private",
                )
                with patch.object(
                    router,
                    "urlopen",
                    return_value=_ContinuityResponse(response),
                ) as opened:
                    with self.assertRaises(RouterError) as raised:
                        router.proxy(
                            config,
                            {
                                "model": "external/destination",
                                "input": [
                                    {
                                        "type": "compaction",
                                        "encrypted_content": opaque,
                                    }
                                ],
                            },
                            {
                                "Authorization": "Bearer current-token-private",
                                "ChatGPT-Account-ID": "current-account-private",
                            },
                            binding_store=store,
                        )

                self.assertEqual(opened.call_count, 1)
                self.assertEqual(raised.exception.reason, "summary_invalid")
                self.assertNotIn(opaque, str(raised.exception))
                self.assertNotIn("private checkpoint", str(raised.exception))

    def test_continuity_multiple_opaque_preserves_images_visible_and_tool_order(self):
        config = _continuity_config()
        source_provider, source_model = find_route(
            config, "native/selected-source"
        )
        opaques = ("opaque-first-private", "opaque-second-private")
        input_items = [
            {
                "type": "message",
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "visible-before"},
                    {
                        "type": "input_image",
                        "image_url": "data:image/png;base64,aW1hZ2U=",
                        "detail": "high",
                    },
                ],
            },
            {
                "type": "function_call",
                "call_id": "call_function",
                "name": "read_state",
                "arguments": "{}",
            },
            {
                "type": "function_call_output",
                "call_id": "call_function",
                "output": "function-result",
            },
            {"type": "compaction", "encrypted_content": opaques[0]},
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "visible-middle"}],
            },
            {
                "type": "custom_tool_call",
                "call_id": "call_custom",
                "name": "custom_state",
                "input": "custom-input",
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call_custom",
                "output": "custom-result",
            },
            {"type": "compaction", "encrypted_content": opaques[1]},
            {
                "type": "message",
                "role": "user",
                "content": [{"type": "input_text", "text": "visible-after"}],
            },
        ]
        body = {"model": "native/current-destination", "input": input_items}
        body_before = copy.deepcopy(body)

        with tempfile.TemporaryDirectory() as directory:
            store = CompactionBindingStore(Path(directory) / "bindings.enc")
            source_identity = derive_native_route_identity(
                {"chatgpt-account-id": "selected-account-private"},
                source_provider["base_url"],
                router.deployment_identity(source_provider, source_model),
                source_model["upstream_id"],
                "native/selected-source",
            )
            for opaque in opaques:
                store.register(binding_for_opaque(source_identity, opaque))
            requests = []

            def open_request(request, timeout):
                requests.append(request)
                if len(requests) <= 2:
                    return _ContinuityResponse(
                        {
                            "status": "completed",
                            "output_text": "checkpoint-%d" % len(requests),
                            "output": [],
                        }
                    )
                return _ContinuityResponse({"status": "completed", "output": []})

            with patch.object(
                router,
                "auth_headers",
                return_value={
                    "Authorization": "Bearer selected-token-private",
                    "chatgpt-account-id": "selected-account-private",
                },
            ), patch.object(router, "urlopen", side_effect=open_request):
                router.proxy(
                    config,
                    body,
                    {
                        "Authorization": "Bearer current-token-private",
                        "ChatGPT-Account-ID": "current-account-private",
                    },
                    binding_store=store,
                )

        self.assertEqual(len(requests), 3)
        self.assertEqual(
            [
                _continuity_request_json(request)["input"][0]["encrypted_content"]
                for request in requests[:2]
            ],
            list(opaques),
        )
        destination = _continuity_request_json(requests[2])
        projected = destination["input"]
        self.assertEqual(
            [item.get("type") for item in projected],
            [
                "message",
                "function_call",
                "function_call_output",
                "message",
                "message",
                "custom_tool_call",
                "custom_tool_call_output",
                "message",
                "message",
            ],
        )
        for index in (0, 1, 2, 4, 5, 6, 8):
            self.assertEqual(projected[index], input_items[index])
        self.assertIn("checkpoint-1", json.dumps(projected[3]))
        self.assertIn("checkpoint-2", json.dumps(projected[7]))
        self.assertNotIn(opaques[0], json.dumps(destination))
        self.assertNotIn(opaques[1], json.dumps(destination))
        self.assertEqual(body, body_before)

    def test_continuity_preparation_applies_to_compact_and_compaction_trigger(self):
        operations = ("compact", "trigger_nonstream", "trigger_stream")
        for operation in operations:
            with self.subTest(operation=operation), tempfile.TemporaryDirectory() as directory:
                config = _continuity_config()
                source_provider, source_model = find_route(
                    config, "native/current-source"
                )
                opaque = "opaque-%s-private" % operation
                store = _register_router_binding(
                    directory,
                    opaque,
                    source_provider,
                    source_model,
                    "native/current-source",
                    "current-account-private",
                )
                requests = []

                def open_request(request, timeout):
                    requests.append(request)
                    if request.full_url.startswith(
                        "https://native-current.example/"
                    ):
                        return _ContinuityResponse(
                            {
                                "status": "completed",
                                "output_text": "handoff-checkpoint",
                                "output": [],
                            }
                        )
                    return _ContinuityResponse(
                        {
                            "choices": [
                                {"message": {"content": "portable-summary"}}
                            ]
                        }
                    )

                body = {
                    "model": "external/destination",
                    "input": [
                        {"type": "compaction", "encrypted_content": opaque}
                    ],
                }
                if operation.startswith("trigger"):
                    body["input"].append({"type": "compaction_trigger"})
                    body["stream"] = operation == "trigger_stream"

                with patch.object(router, "urlopen", side_effect=open_request):
                    if operation == "compact":
                        metadata, result = router.proxy_compact(
                            config,
                            body,
                            {
                                "Authorization": "Bearer current-token-private",
                                "ChatGPT-Account-ID": "current-account-private",
                            },
                            binding_store=store,
                        )
                        response = json.loads(result)
                    else:
                        metadata, result = router.proxy(
                            config,
                            body,
                            {
                                "Authorization": "Bearer current-token-private",
                                "ChatGPT-Account-ID": "current-account-private",
                            },
                            binding_store=store,
                        )
                        if body["stream"]:
                            events = list(sse_json_events(result))
                            response = next(
                                event["response"]
                                for event in events
                                if event.get("type") == "response.completed"
                            )
                        else:
                            response = json.loads(result)

                self.assertEqual(len(requests), 2)
                summary_payload = _continuity_request_json(requests[1])
                self.assertIn("handoff-checkpoint", json.dumps(summary_payload))
                self.assertNotIn(opaque, json.dumps(summary_payload))
                self.assertEqual(metadata["status"], 200)
                self.assertEqual(len(response["output"]), 1)
                self.assertEqual(response["output"][0]["type"], "compaction")
                self.assertTrue(
                    response["output"][0]["encrypted_content"].startswith("emp1:")
                )

    def test_successful_native_json_exposes_narrow_observation_seam(self):
        incoming = {
            "Authorization": "Bearer current-token-private",
            "ChatGPT-Account-ID": "current-account-private",
        }
        response = {"status": "completed", "output": []}
        for operation in ("generation", "compact"):
            with self.subTest(operation=operation):
                config = _continuity_config()
                body = {
                    "model": "native/current-destination",
                    "input": "hello",
                }
                observed = []
                with patch.object(
                    router, "urlopen", return_value=_ContinuityResponse(response)
                ):
                    entrypoint = (
                        router.proxy_compact
                        if operation == "compact"
                        else router.proxy
                    )
                    entrypoint(
                        config,
                        body,
                        incoming,
                        on_native_response=lambda *args: observed.append(args),
                    )

                self.assertEqual(len(observed), 1)
                (
                    provider,
                    model,
                    requested_slug,
                    transient,
                    identity,
                    result,
                    success,
                ) = observed[0]
                self.assertEqual(provider["id"], "native-current")
                self.assertEqual(model["id"], "native/current-destination")
                self.assertEqual(requested_slug, "native/current-destination")
                self.assertEqual(dict(transient), dict(incoming))
                self.assertIsNotNone(identity)
                self.assertEqual(json.loads(result), response)
                self.assertTrue(success)

    def test_imported_account_native_json_registers_compaction_for_actual_route(self):
        config = _continuity_config()
        opaque = "opaque-imported-account-json-private"
        raw_token = "Bearer imported-account-token-private"
        raw_account = "imported-account-id-private"
        response = {
            "status": "completed",
            "output": [
                {
                    "type": "compaction",
                    "encrypted_content": opaque,
                }
            ],
        }

        with tempfile.TemporaryDirectory() as directory:
            store = CompactionBindingStore(Path(directory) / "bindings.enc")
            observed_identities = []

            def selected_headers(account):
                self.assertEqual(account["id"], "selected-account")
                return {
                    "Authorization": raw_token,
                    "chatgpt-account-id": raw_account,
                }

            def register_response(
                provider, model, slug, transient, identity, raw, success
            ):
                observed_identities.append(identity)
                register_native_json(store, identity, json.loads(raw), success)

            with patch.object(
                router, "auth_headers", side_effect=selected_headers
            ) as auth, patch.object(
                router,
                "urlopen",
                return_value=_ContinuityResponse(response),
            ):
                metadata, raw = router.proxy(
                    config,
                    {
                        "model": "native/selected-destination",
                        "input": "hello",
                    },
                    {},
                    on_native_response=register_response,
                )

            provider, model = find_route(
                config, "native/selected-destination"
            )
            expected_identity = router._native_route_identity(
                provider,
                model,
                "native/selected-destination",
                {"chatgpt-account-id": raw_account},
            )
            binding = store.lookup(opaque)

            self.assertEqual(metadata["status"], 200)
            self.assertEqual(json.loads(raw), response)
            self.assertEqual(auth.call_count, 2)
            self.assertEqual(observed_identities, [expected_identity])
            self.assertEqual(
                binding.trust_domain.auth_identity,
                expected_identity.trust_domain.auth_identity,
            )
            self.assertEqual(
                binding.source_model_slug, "native/selected-destination"
            )
            persisted = (Path(directory) / "bindings.enc").read_bytes()
            diagnostic = json.dumps(metadata, ensure_ascii=False).encode("utf-8")
            for secret in (opaque, raw_token, raw_account):
                self.assertNotIn(secret.encode("utf-8"), persisted)
                self.assertNotIn(secret.encode("utf-8"), diagnostic)

    def test_imported_account_native_sse_registers_only_after_completed(self):
        config = _continuity_config()
        raw_token = "Bearer imported-stream-token-private"
        raw_account = "imported-stream-account-private"

        def selected_headers(account):
            self.assertEqual(account["id"], "selected-account")
            return {
                "Authorization": raw_token,
                "chatgpt-account-id": raw_account,
            }

        cases = (
            ("completed", "response.completed", True, False),
            ("failed", "response.failed", False, False),
            ("incomplete", "response.incomplete", False, False),
            ("early-close", "response.completed", False, True),
        )
        for name, terminal_type, should_register, close_early in cases:
            with self.subTest(case=name), tempfile.TemporaryDirectory() as directory:
                opaque = "opaque-imported-account-sse-%s-private" % name
                item = {
                    "type": "compaction",
                    "encrypted_content": opaque,
                }
                frames = [
                    {
                        "type": "response.output_item.done",
                        "item": item,
                    },
                    {
                        "type": terminal_type,
                        "response": {
                            "status": terminal_type.split(".", 1)[1],
                            "output": [item],
                        },
                    },
                ]

                class FakeResponse:
                    status = 200
                    headers = {"Content-Type": "text/event-stream"}

                    def __init__(self):
                        self._chunks = iter(
                            (
                                "event: %s\ndata: %s\n\n"
                                % (frame["type"], json.dumps(frame))
                            ).encode("utf-8")
                            for frame in frames
                        )

                    def __iter__(self):
                        return self

                    def __next__(self):
                        return next(self._chunks)

                    def close(self):
                        pass

                store = CompactionBindingStore(
                    Path(directory) / "bindings.enc"
                )
                observed_identities = []
                diagnostics = []

                def start_observer(identity):
                    observed_identities.append(identity)
                    return NativeCompactionObserver(store, identity).observe

                with patch.object(
                    router, "auth_headers", side_effect=selected_headers
                ), patch.object(
                    router, "urlopen", return_value=FakeResponse()
                ):
                    provider, model = find_route(
                        config, "native/selected-destination"
                    )
                    stream = router.forward_responses_stream(
                        provider,
                        {
                            "model": "native/selected-destination",
                            "stream": True,
                            "input": "hello",
                        },
                        model,
                        {},
                        terminal_callback=diagnostics.append,
                        on_native_stream_start=start_observer,
                    )
                    if close_early:
                        next(iter(stream))
                        stream.close()
                    else:
                        list(stream)

                expected_identity = router._native_route_identity(
                    provider,
                    model,
                    "native/selected-destination",
                    {"chatgpt-account-id": raw_account},
                )
                self.assertEqual(observed_identities, [expected_identity])
                if should_register:
                    binding = store.lookup(opaque)
                    self.assertEqual(
                        binding.trust_domain.auth_identity,
                        expected_identity.trust_domain.auth_identity,
                    )
                else:
                    with self.assertRaises(BindingMissing):
                        store.lookup(opaque)

                store_path = Path(directory) / "bindings.enc"
                persisted = store_path.read_bytes() if store_path.exists() else b""
                diagnostic = json.dumps(diagnostics, ensure_ascii=False).encode(
                    "utf-8"
                )
                for secret in (opaque, raw_token, raw_account):
                    self.assertNotIn(secret.encode("utf-8"), persisted)
                    self.assertNotIn(secret.encode("utf-8"), diagnostic)

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
            self.assertEqual(len(compacted["output"]), 1)
            compact_item = compacted["output"][0]
            self.assertEqual(compact_item["type"], "compaction")
            self.assertTrue(compact_item["encrypted_content"].startswith("emp1:"))
            self.assertIn(
                "continue from the saved state",
                base64.urlsafe_b64decode(
                    compact_item["encrypted_content"][len("emp1:") :]
                ).decode("utf-8"),
            )

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
        with self.assertRaises(RouterError) as raised:
            responses_to_chat(
                {
                    "model": "demo/model",
                    "input": [
                        {
                            "type": "compaction",
                            "encrypted_content": "provider-secret",
                        }
                    ],
                },
                "model",
            )
        self.assertEqual(raised.exception.status, 409)
        self.assertIn("context_handoff_required", str(raised.exception))
        self.assertNotIn("provider-secret", str(raised.exception))
        summary_request = completion.call_args.args[1]
        self.assertNotIn("compaction_trigger", json.dumps(summary_request))

    def test_external_compact_endpoint_returns_one_emp_compaction_for_every_protocol(self):
        cases = {
            "responses": json.dumps(
                {
                    "id": "resp_summary",
                    "object": "response",
                    "status": "completed",
                    "output": [
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": "responses summary"}
                            ],
                        }
                    ],
                }
            ).encode(),
            "chat_completions": json.dumps(
                {
                    "choices": [
                        {
                            "message": {"content": "chat summary"},
                            "finish_reason": "stop",
                        }
                    ]
                }
            ).encode(),
            "anthropic_messages": json.dumps(
                {
                    "content": [{"type": "text", "text": "anthropic summary"}],
                    "stop_reason": "end_turn",
                }
            ).encode(),
        }

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self, raw):
                self.raw = raw
                self.sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return self.raw

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

            def close(self):
                pass

        for protocol, upstream_raw in cases.items():
            with self.subTest(protocol=protocol):
                config = {
                    "providers": [
                        {
                            "id": "external",
                            "enabled": True,
                            "auth_mode": "api_key",
                            "api_key": "key",
                            "protocol": protocol,
                            "base_url": "https://example.com/v1",
                        }
                    ],
                    "models": [
                        {
                            "id": "external/model",
                            "provider": "external",
                            "enabled": True,
                        }
                    ],
                }
                with patch.object(
                    router, "urlopen", return_value=FakeResponse(upstream_raw)
                ) as opened:
                    metadata, raw = proxy_compact(
                        config,
                        {
                            "model": "external/model",
                            "input": [
                                {
                                    "type": "message",
                                    "role": "user",
                                    "content": [
                                        {"type": "input_text", "text": "history"}
                                    ],
                                }
                            ],
                        },
                        {},
                    )

                response = json.loads(raw)
                self.assertEqual(metadata["status"], 200)
                self.assertEqual(len(response["output"]), 1)
                item = response["output"][0]
                self.assertEqual(item["type"], "compaction")
                self.assertTrue(item["encrypted_content"].startswith("emp1:"))
                decoded = base64.urlsafe_b64decode(
                    item["encrypted_content"][len("emp1:") :]
                ).decode("utf-8")
                self.assertIn(protocol.split("_")[0], decoded)
                self.assertNotIn("/responses/compact", opened.call_args.args[0].full_url)

    def test_external_compaction_trigger_stream_and_nonstream_are_emp_owned(self):
        cases = {
            "responses": json.dumps(
                {
                    "id": "resp_summary",
                    "object": "response",
                    "status": "completed",
                    "output_text": "responses summary",
                    "output": [],
                }
            ).encode(),
            "chat_completions": json.dumps(
                {
                    "choices": [
                        {
                            "message": {"content": "chat summary"},
                            "finish_reason": "stop",
                        }
                    ]
                }
            ).encode(),
            "anthropic_messages": json.dumps(
                {
                    "content": [{"type": "text", "text": "anthropic summary"}],
                    "stop_reason": "end_turn",
                }
            ).encode(),
        }

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self, raw):
                self.raw = raw
                self.sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return self.raw

            def close(self):
                pass

        for protocol, upstream_raw in cases.items():
            for stream in (False, True):
                with self.subTest(protocol=protocol, stream=stream):
                    config = {
                        "providers": [
                            {
                                "id": "external",
                                "enabled": True,
                                "auth_mode": "api_key",
                                "api_key": "key",
                                "protocol": protocol,
                                "base_url": "https://example.com/v1",
                            }
                        ],
                        "models": [
                            {
                                "id": "external/model",
                                "provider": "external",
                                "enabled": True,
                            }
                        ],
                    }
                    body = {
                        "model": "external/model",
                        "stream": stream,
                        "input": [
                            {
                                "type": "message",
                                "role": "user",
                                "content": [
                                    {"type": "input_text", "text": "history"}
                                ],
                            },
                            {"type": "compaction_trigger"},
                        ],
                    }
                    with patch.object(
                        router,
                        "urlopen",
                        side_effect=lambda *args, **kwargs: FakeResponse(upstream_raw),
                    ) as opened:
                        metadata, result = router.proxy(config, body, {})
                        if stream:
                            events = list(sse_json_events(result))
                            done = [
                                event
                                for event in events
                                if event.get("type") == "response.output_item.done"
                            ]
                            completed = [
                                event
                                for event in events
                                if event.get("type") == "response.completed"
                            ]
                            self.assertEqual(metadata["kind"], "stream")
                            self.assertEqual(len(done), 1)
                            self.assertEqual(len(completed), 1)
                            item = done[0]["item"]
                            self.assertNotIn("status", item)
                            self.assertEqual(
                                completed[0]["response"]["output"], [item]
                            )
                        else:
                            response = json.loads(result)
                            self.assertEqual(metadata["kind"], "body")
                            self.assertEqual(len(response["output"]), 1)
                            item = response["output"][0]

                    self.assertEqual(item["type"], "compaction")
                    self.assertTrue(item["encrypted_content"].startswith("emp1:"))
                    decoded = base64.urlsafe_b64decode(
                        item["encrypted_content"][len("emp1:") :]
                    ).decode("utf-8")
                    self.assertIn(protocol.split("_")[0], decoded)
                    self.assertEqual(opened.call_count, 1)
                    summary_request = json.loads(opened.call_args.args[0].data)
                    self.assertFalse(summary_request["stream"])
                    self.assertNotIn(
                        "compaction_trigger", json.dumps(summary_request)
                    )

    def test_external_ordinary_response_with_compaction_fails_closed(self):
        config = {
            "providers": [
                {
                    "id": "external",
                    "enabled": True,
                    "auth_mode": "api_key",
                    "api_key": "key",
                    "protocol": "responses",
                    "base_url": "https://example.com/v1",
                }
            ],
            "models": [
                {"id": "external/model", "provider": "external", "enabled": True}
            ],
        }
        opaque = "private-upstream-compaction"
        upstream_raw = json.dumps(
            {
                "id": "resp_external",
                "object": "response",
                "status": "completed",
                "output": [
                    {"type": "compaction", "encrypted_content": opaque}
                ],
            }
        ).encode()

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "application/json"}
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return upstream_raw

            def __enter__(self):
                return self

            def __exit__(self, *args):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            with self.assertRaises(RouterError) as raised:
                router.proxy(
                    config,
                    {"model": "external/model", "input": "ordinary request"},
                    {},
                )

        self.assertEqual(opened.call_count, 1)
        self.assertEqual(raised.exception.status, 502)
        self.assertEqual(
            getattr(raised.exception, "error_class", None), "protocol_error"
        )
        self.assertIn("unexpected compaction", str(raised.exception))
        self.assertNotIn(opaque, str(raised.exception))

    def test_external_ordinary_stream_with_compaction_emits_one_safe_failure(self):
        config = {
            "providers": [
                {
                    "id": "external",
                    "enabled": True,
                    "auth_mode": "api_key",
                    "api_key": "key",
                    "protocol": "responses",
                    "base_url": "https://example.com/v1",
                }
            ],
            "models": [
                {"id": "external/model", "provider": "external", "enabled": True}
            ],
        }
        opaque = "private-stream-compaction"
        item = {"id": "cmp_external", "type": "compaction", "encrypted_content": opaque}
        frames = [
            {
                "type": "response.created",
                "response": {
                    "id": "resp_external",
                    "status": "in_progress",
                    "output": [],
                },
            },
            {
                "type": "response.output_item.added",
                "output_index": 0,
                "item": item,
            },
            {
                "type": "response.output_item.done",
                "output_index": 0,
                "item": item,
            },
            {
                "type": "response.completed",
                "response": {
                    "id": "resp_external",
                    "status": "completed",
                    "output": [item],
                },
            },
        ]

        class FakeResponse:
            status = 200
            headers = {"Content-Type": "text/event-stream"}

            def __init__(self):
                self.chunks = iter(
                    (
                        "event: %s\ndata: %s\n\n"
                        % (frame["type"], json.dumps(frame))
                    ).encode()
                    for frame in frames
                )

            def __iter__(self):
                return self

            def __next__(self):
                return next(self.chunks)

            def close(self):
                pass

        with patch.object(router, "urlopen", return_value=FakeResponse()):
            metadata, stream = router.proxy(
                config,
                {
                    "model": "external/model",
                    "stream": True,
                    "input": "ordinary request",
                },
                {},
            )
            events = list(sse_json_events(stream))

        failed = [event for event in events if event.get("type") == "response.failed"]
        self.assertEqual(metadata["kind"], "stream")
        self.assertEqual(len(failed), 1)
        self.assertEqual(
            failed[0]["response"]["error"]["error_class"], "protocol_error"
        )
        self.assertFalse(
            any(
                event.get("type")
                in {"response.output_item.added", "response.output_item.done"}
                for event in events
            )
        )
        self.assertFalse(
            any(event.get("type") == "response.completed" for event in events)
        )
        self.assertNotIn(opaque, json.dumps(events))

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

    def test_native_request_zstd_compresses_large_semantic_payload(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        payload = {"model": "gpt-native", "input": "repeat-me" * (256 * 1024)}

        class FakeResponse:
            status = 200

        with patch.object(router, "urlopen", return_value=FakeResponse()) as opened:
            router._request(
                provider,
                payload,
                {"Authorization": "Bearer session-token"},
            )

        request = opened.call_args.args[0]
        headers = {key.lower(): value for key, value in request.header_items()}
        decoded = zstandard.ZstdDecompressor().decompress(request.data)
        self.assertEqual(headers["content-encoding"], "zstd")
        self.assertEqual(json.loads(decoded), payload)
        metadata = provider["_transport_metadata"]
        self.assertEqual(metadata["decoded_request_bytes"], len(decoded))
        self.assertEqual(metadata["upstream_request_bytes"], len(request.data))
        self.assertEqual(metadata["upstream_content_encoding"], "zstd")
        self.assertAlmostEqual(
            metadata["compression_ratio"], len(request.data) / len(decoded)
        )

    def test_native_request_context_check_receives_semantic_payload(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        payload = {"model": "gpt-native", "input": [{"type": "message"}]}
        checked = []

        class FakeResponse:
            status = 200

        def context_check(value, stream, operation):
            checked.append((value, stream, operation))
            return {}

        with patch.object(router, "urlopen", return_value=FakeResponse()):
            router._request(
                provider,
                payload,
                {"Authorization": "Bearer session-token"},
                context_check=context_check,
            )

        self.assertEqual(checked, [(payload, False, "")])
        self.assertIs(checked[0][0], payload)

    def test_native_request_compression_failure_is_fail_closed(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }

        with patch.object(
            router,
            "zstd_encode",
            side_effect=router.TransportError("private compression detail"),
        ), patch.object(router, "urlopen") as opened:
            with self.assertRaises(RouterError) as raised:
                router._request(
                    provider,
                    {"model": "gpt-native", "input": "private-payload"},
                    {"Authorization": "Bearer private-credential"},
                )

        opened.assert_not_called()
        self.assertEqual(raised.exception.status, 502)
        self.assertEqual(str(raised.exception), "native request compression failed")

    def test_external_routes_send_original_json_without_content_encoding(self):
        payload = {"model": "external-model", "input": [1, 2]}
        serialized = b'{"model":"external-model","input":[1,2]}'
        providers = (
            {
                "id": "responses",
                "protocol": "responses",
                "auth_mode": "api_key",
                "api_key": "key",
                "base_url": "https://example.com/v1",
            },
            {
                "id": "chat",
                "protocol": "chat_completions",
                "auth_mode": "api_key",
                "api_key": "key",
                "base_url": "https://example.com/v1",
            },
            {
                "id": "anthropic",
                "protocol": "anthropic_messages",
                "auth_mode": "anthropic_api_key",
                "api_key": "key",
                "base_url": "https://example.com/v1",
            },
        )

        class FakeResponse:
            status = 200

        for provider in providers:
            with self.subTest(protocol=provider["protocol"]), patch.object(
                router, "urlopen", return_value=FakeResponse()
            ) as opened:
                router._request(provider, payload, {})
            request = opened.call_args.args[0]
            headers = {key.lower(): value for key, value in request.header_items()}
            self.assertNotIn("content-encoding", headers)
            self.assertEqual(request.data, serialized)
            self.assertEqual(
                provider["_transport_metadata"],
                {
                    "decoded_request_bytes": len(serialized),
                    "upstream_request_bytes": len(serialized),
                    "upstream_content_encoding": "identity",
                    "compression_ratio": 1.0,
                },
            )

    def test_native_pre_header_timeout_retries_once_with_identical_body(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        requests = []

        class FakeResponse:
            status = 200

        def open_request(request, timeout):
            requests.append(request)
            if len(requests) == 1:
                raise TimeoutError("pre-header timeout")
            return FakeResponse()

        with patch.object(router, "urlopen", side_effect=open_request) as opened:
            result = router._request(
                provider,
                {"model": "gpt-native", "input": "repeat-me" * 4096},
                {
                    "Authorization": "Bearer session-token",
                    "ChatGPT-Account-ID": "account-1",
                    "OpenAI-Beta": "responses=experimental",
                    "Originator": "codex_cli_rs",
                },
            )

        self.assertIsInstance(result, FakeResponse)
        self.assertEqual(opened.call_count, 2)
        self.assertEqual(requests[0].data, requests[1].data)
        self.assertIs(requests[0].data, requests[1].data)
        metadata = provider["_transport_metadata"]
        self.assertEqual(metadata["upstream_request_bytes"], len(requests[1].data))
        headers = [
            {key.lower(): value for key, value in request.header_items()}
            for request in requests
        ]
        self.assertEqual(headers[0], headers[1])
        self.assertEqual(headers[0]["authorization"], "Bearer session-token")
        self.assertEqual(headers[0]["chatgpt-account-id"], "account-1")
        self.assertEqual(headers[0]["openai-beta"], "responses=experimental")
        self.assertEqual(headers[0]["originator"], "codex_cli_rs")

    def test_identical_retry_compresses_once_and_records_one_attempt(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://native.invalid/backend-api/codex",
        }
        encoded = b"encoded-once"

        class FakeResponse:
            status = 200

        with patch.object(router, "zstd_encode", return_value=encoded) as compress, patch.object(
            router, "urlopen", side_effect=[TimeoutError("pre-header"), FakeResponse()]
        ):
            router._request(
                provider,
                {"model": "model-fixture", "input": "payload-fixture"},
                {"Authorization": "Bearer fixture"},
            )

        compress.assert_called_once()
        self.assertEqual(provider["_transport_metadata"]["upstream_request_bytes"], len(encoded))

    def test_zero_decoded_size_omits_ratio_from_safe_metadata(self):
        safe = router._safe_transport_metadata(
            {
                "decoded_request_bytes": 0,
                "upstream_request_bytes": 0,
                "upstream_content_encoding": "identity",
                "compression_ratio": 1.0,
            }
        )
        self.assertNotIn("compression_ratio", safe)

    def test_route_event_transport_metadata_is_strictly_sanitized(self):
        provider = {
            "id": "fixture",
            "protocol": "responses",
            "base_url": "https://fixture.invalid/v1",
            "_transport_metadata": {
                "decoded_request_bytes": 10,
                "upstream_request_bytes": 5,
                "upstream_content_encoding": "zstd",
                "compression_ratio": 0.5,
                "body": "secret-body",
                "authorization": "secret-header",
                "url": "https://secret.invalid",
                "model": "secret-model",
                "account": "secret-account",
                "token": "secret-token",
                "opaque": "secret-opaque",
            },
        }
        event = router._route_event(
            {"status": 200}, provider, {"id": "fixture/model", "upstream_id": "fixture"}
        )
        self.assertEqual(event["decoded_request_bytes"], 10)
        self.assertEqual(event["upstream_request_bytes"], 5)
        self.assertEqual(event["upstream_content_encoding"], "zstd")
        self.assertEqual(event["compression_ratio"], 0.5)
        for key in ("body", "authorization", "url", "model", "account", "token", "opaque"):
            self.assertNotIn(key, event)

    def test_native_pre_header_timeout_reuses_auth_identity(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "account",
            "account": {"id": "account-1"},
            "base_url": "https://example.com/backend-api/codex",
        }
        requests = []

        class FakeResponse:
            status = 200

        def open_request(request, timeout):
            requests.append(request)
            if len(requests) == 1:
                raise TimeoutError("pre-header timeout")
            return FakeResponse()

        with patch.object(
            router,
            "auth_headers",
            side_effect=[
                {"Authorization": "Bearer first-token"},
                {"Authorization": "Bearer second-token"},
            ],
        ) as authorized, patch.object(
            router, "urlopen", side_effect=open_request
        ) as opened:
            router._request(provider, {"model": "gpt-native"}, {})

        self.assertEqual(opened.call_count, 2)
        self.assertEqual(authorized.call_count, 1)
        self.assertEqual(
            [request.get_header("Authorization") for request in requests],
            ["Bearer first-token", "Bearer first-token"],
        )

    def test_native_transient_http_retry_reuses_encoded_body(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        requests = []

        class FakeResponse:
            status = 200

        def open_request(request, timeout):
            requests.append(request)
            if len(requests) == 1:
                raise HTTPError(
                    request.full_url,
                    503,
                    "busy",
                    {},
                    BytesIO(b"busy"),
                )
            return FakeResponse()

        with patch.object(router, "urlopen", side_effect=open_request) as opened:
            with patch.object(router.time, "sleep") as sleep:
                result = router._request(
                    provider,
                    {"model": "gpt-native", "input": "repeat-me" * 4096},
                    {"Authorization": "Bearer session-token"},
                )

        self.assertIsInstance(result, FakeResponse)
        self.assertEqual(opened.call_count, 2)
        sleep.assert_called_once_with(0.5)
        self.assertEqual(requests[0].data, requests[1].data)
        self.assertIs(requests[0].data, requests[1].data)

    def test_native_pre_header_timeout_retry_can_be_disabled(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        failures = (
            TimeoutError("pre-header timeout"),
            URLError(TimeoutError("wrapped pre-header timeout")),
        )

        for failure in failures:
            with self.subTest(failure=type(failure).__name__), patch.object(
                router, "urlopen", side_effect=failure
            ) as opened:
                with self.assertRaises(RouterError) as raised:
                    router._request(
                        provider,
                        {"model": "gpt-native", "input": "private-payload"},
                        {"Authorization": "Bearer private-credential"},
                        allow_retries=False,
                    )

            self.assertEqual(opened.call_count, 1)
            self.assertEqual(raised.exception.status, 504)
            self.assertEqual(str(raised.exception), "upstream request timed out")

    def test_request_retry_policy_disables_existing_native_retries(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }
        failure = HTTPError(
            "https://example.com/backend-api/codex/responses",
            503,
            "busy",
            {"Content-Type": "text/plain"},
            BytesIO(b"busy"),
        )

        with patch.object(router, "urlopen", side_effect=failure) as opened:
            with patch.object(router.time, "sleep") as sleep:
                with self.assertRaises(RouterError) as raised:
                    router._request(
                        provider,
                        {"model": "gpt-native"},
                        {"Authorization": "Bearer session-token"},
                        allow_retries=False,
                    )

        self.assertEqual(opened.call_count, 1)
        sleep.assert_not_called()
        self.assertEqual(raised.exception.status, 503)

    def test_external_pre_header_timeout_does_not_gain_retry(self):
        provider = {
            "id": "responses",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "key",
            "base_url": "https://example.com/v1",
        }

        with patch.object(
            router, "urlopen", side_effect=TimeoutError("pre-header timeout")
        ) as opened:
            with self.assertRaises(TimeoutError):
                router._request(provider, {"model": "external-model"}, {})

        self.assertEqual(opened.call_count, 1)

    def test_native_second_pre_header_timeout_returns_safe_gateway_timeout(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/private-route",
        }
        failure = TimeoutError(
            "https://example.com/private-route private-payload Bearer private-credential"
        )

        with patch.object(router, "urlopen", side_effect=failure) as opened:
            with self.assertRaises(RouterError) as raised:
                router._request(
                    provider,
                    {"model": "gpt-native", "input": "private-payload"},
                    {"Authorization": "Bearer private-credential"},
                )

        self.assertEqual(opened.call_count, 2)
        self.assertEqual(raised.exception.status, 504)
        self.assertEqual(str(raised.exception), "upstream request timed out")

    def test_native_wrapped_pre_header_timeout_uses_same_bounded_retry(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/private-route",
        }
        failure = URLError(
            TimeoutError("private-route private-payload private-credential")
        )

        with patch.object(router, "urlopen", side_effect=failure) as opened:
            with patch.object(router.time, "sleep") as sleep:
                with self.assertRaises(RouterError) as raised:
                    router._request(
                        provider,
                        {"model": "gpt-native", "input": "private-payload"},
                        {"Authorization": "Bearer private-credential"},
                    )

        self.assertEqual(opened.call_count, 2)
        sleep.assert_not_called()
        self.assertEqual(raised.exception.status, 504)
        self.assertEqual(str(raised.exception), "upstream request timed out")

    def test_native_timeout_after_response_object_is_not_retried(self):
        provider = {
            "id": "native",
            "protocol": "responses",
            "auth_mode": "forward",
            "base_url": "https://example.com/backend-api/codex",
        }

        class FakeResponse:
            def close(self):
                pass

        with patch.object(
            router, "urlopen", return_value=FakeResponse()
        ) as opened, patch.object(
            router,
            "_DeadlineResponse",
            side_effect=TimeoutError("post-header timeout"),
        ):
            with self.assertRaises(TimeoutError):
                router._request(
                    provider,
                    {"model": "gpt-native"},
                    {"Authorization": "Bearer session-token"},
                )

        self.assertEqual(opened.call_count, 1)

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
