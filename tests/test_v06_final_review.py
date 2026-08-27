import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from cryptography.fernet import Fernet

from easy_multi_provider import router, stream_adapters
from easy_multi_provider.model_discovery import created_timestamp
from easy_multi_provider.protocol_projection import (
    _advance_textual_protocol_probe,
    _anthropic_incomplete_reason,
    _chat_incomplete_reason,
    responses_terminal_observation,
    responses_to_anthropic,
    responses_to_chat,
    validate_responses_body,
)
from easy_multi_provider.router_errors import ExternalProtocolError, RouterError
from easy_multi_provider.stream_adapters import _response_json_stream
from easy_multi_provider.transport import sse_json_events
from easy_multi_provider.vault import VaultError, default_master_key_file, ensure_master_key


class _FakeResponse:
    def __init__(self, chunks):
        self.chunks = list(chunks)

    def __iter__(self):
        return iter(self.chunks)

    def close(self):
        pass


class _BodyResponse:
    status = 200
    headers = {"Content-Type": "application/json"}

    def __init__(self, value):
        self.raw = json.dumps(value).encode("utf-8")
        self.closed = False

    def read(self, size=-1):
        raw, self.raw = self.raw, b""
        return raw

    def close(self):
        self.closed = True

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class FinalReviewRegressionTests(unittest.TestCase):
    def test_parallel_tool_history_keeps_one_assistant_turn(self):
        body = {
            "input": [
                {"type": "function_call", "call_id": "call_a", "name": "a", "arguments": "{}"},
                {"type": "function_call", "call_id": "call_b", "name": "b", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_a", "output": "A"},
                {"type": "function_call_output", "call_id": "call_b", "output": "B"},
            ]
        }

        chat = responses_to_chat(body, "upstream")
        self.assertEqual([item["role"] for item in chat["messages"]], ["assistant", "tool", "tool"])
        self.assertEqual(len(chat["messages"][0]["tool_calls"]), 2)

        anthropic = responses_to_anthropic(body, "upstream")
        self.assertEqual([item["role"] for item in anthropic["messages"]], ["assistant", "user"])
        self.assertEqual(len(anthropic["messages"][0]["content"]), 2)
        self.assertEqual(len(anthropic["messages"][1]["content"]), 2)

    def test_tool_history_rejects_orphan_duplicate_calls_and_outputs(self):
        orphan = {
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "call_missing",
                    "output": "result",
                }
            ]
        }
        duplicate_calls = {
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_same",
                    "name": "first",
                    "arguments": "{}",
                },
                {
                    "type": "function_call",
                    "call_id": "call_same",
                    "name": "second",
                    "arguments": "{}",
                },
            ]
        }
        duplicate_outputs = {
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_same",
                    "name": "first",
                    "arguments": "{}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_same",
                    "output": "one",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_same",
                    "output": "two",
                },
            ]
        }

        for body in (orphan, duplicate_calls, duplicate_outputs):
            with self.subTest(body=body):
                with self.assertRaises(RouterError):
                    responses_to_chat(body, "upstream")
                with self.assertRaises(RouterError):
                    responses_to_anthropic(body, "upstream")

    def test_named_standalone_tool_output_becomes_visible_context(self):
        body = {
            "input": [{
                "type": "function_call_output",
                "name": "notifications",
                "namespace": "slack",
                "output": "Alice mentioned you.",
            }]
        }

        chat = responses_to_chat(body, "upstream")
        anthropic = responses_to_anthropic(body, "upstream")

        self.assertEqual(chat["messages"][0]["role"], "user")
        self.assertIn("slack/notifications", chat["messages"][0]["content"])
        self.assertIn("Alice mentioned you.", chat["messages"][0]["content"])
        self.assertEqual(anthropic["messages"][0]["role"], "user")
        self.assertIn(
            "slack/notifications",
            anthropic["messages"][0]["content"][0]["text"],
        )

    def test_unknown_terminal_reasons_are_protocol_errors(self):
        with self.assertRaises(ExternalProtocolError):
            _chat_incomplete_reason(None)
        with self.assertRaises(ExternalProtocolError):
            _anthropic_incomplete_reason(None)
        with self.assertRaises(ExternalProtocolError):
            _chat_incomplete_reason("error")
        with self.assertRaises(ExternalProtocolError):
            _anthropic_incomplete_reason("pause_turn")

    def test_long_text_fragment_cannot_hide_protocol_markup(self):
        with self.assertRaises(RouterError):
            _advance_textual_protocol_probe("", "x" * 1024 + "<think>secret")

    def test_responses_json_rejects_unknown_status_and_invalid_output(self):
        with self.assertRaises(ExternalProtocolError):
            list(_response_json_stream({"output": []}))
        with self.assertRaises(ExternalProtocolError):
            list(_response_json_stream({"status": "cancelled", "output": []}))
        with self.assertRaises(ExternalProtocolError):
            list(_response_json_stream({"status": "completed", "output": ["bad"]}))
        with self.assertRaises(ExternalProtocolError):
            list(_response_json_stream({
                "status": "completed",
                "error": {"message": "contradictory"},
                "output": [],
            }))

    def test_responses_json_rejects_unknown_and_malformed_output_items(self):
        invalid_items = (
            {"type": "unknown_output", "value": "unsafe"},
            {"type": "function_call", "name": "shell"},
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": {"bad": True}}],
            },
        )
        for item in invalid_items:
            with self.subTest(item=item), self.assertRaises(ExternalProtocolError):
                validate_responses_body({"status": "completed", "output": [item]})
        native = {"status": "completed", "output": [{"type": "web_search_call"}]}
        self.assertEqual(
            validate_responses_body(native, validate_output_items=False)["output"],
            native["output"],
        )

    def test_portable_nonstream_responses_validate_terminal_before_returning(self):
        provider = {
            "id": "portable",
            "protocol": "responses",
            "auth_mode": "api_key",
            "base_url": "https://example.invalid/v1",
            "api_key": "test-only",
        }
        model = {
            "id": "portable/model",
            "provider": "portable",
            "upstream_id": "model",
        }
        body = {"model": "portable/model", "input": "hello", "stream": False}
        invalid = (
            {"output": []},
            {"status": "cancelled", "output": []},
            {
                "status": "completed",
                "error": {"message": "contradictory"},
                "output": [],
            },
        )
        for value in invalid:
            with self.subTest(value=value), patch.object(
                router, "_request", return_value=_BodyResponse(value)
            ):
                with self.assertRaises(ExternalProtocolError):
                    router.forward_responses(provider, body, model, {})

    def test_nonstream_diagnostics_follow_responses_terminal(self):
        events = []
        result = json.dumps({
            "status": "incomplete",
            "output": [],
            "incomplete_details": {"reason": "max_output_tokens"},
        }).encode("utf-8")

        router._finish_nonstream(
            events.append,
            {"status": 200, "kind": "body"},
            {"protocol": "responses", "auth_mode": "api_key"},
            {"upstream_id": "model"},
            result,
        )

        self.assertFalse(events[0]["success"])
        self.assertEqual(events[0]["error_class"], "output_limit")
        self.assertFalse(
            responses_terminal_observation({
                "status": "failed",
                "output": [],
                "error": {"message": "safe fixture"},
            })["success"]
        )

    def test_sse_parser_releases_raw_json_probe_after_first_data_frame(self):
        class TrackingBuffer(bytearray):
            maximum = 0

            def extend(self, value):
                super().extend(value)
                type(self).maximum = max(type(self).maximum, len(self))

        first = b'data: {"type":"ping"}\n\n'
        keepalive = b': keepalive ' + (b'x' * 4096) + b'\n'
        response = _FakeResponse([first] + [keepalive] * 32)
        with patch.object(
            stream_adapters, "bytearray", TrackingBuffer, create=True
        ):
            self.assertEqual(
                list(stream_adapters._sse_data(response)),
                [('{"type":"ping"}', True)],
            )
        self.assertLessEqual(TrackingBuffer.maximum, len(first))

    def test_model_timestamp_is_not_limited_by_context_window(self):
        self.assertEqual(created_timestamp(1_700_000_000), 1_700_000_000)
        self.assertEqual(created_timestamp(1_700_000_000_000), 1_700_000_000)
        self.assertEqual(created_timestamp("2026-08-23T00:00:00Z"), 1_787_443_200)
        for invalid in (float("nan"), float("inf"), float("-inf"), "NaN", "1e400"):
            with self.subTest(invalid=invalid):
                self.assertEqual(created_timestamp(invalid), 0)

    def test_anthropic_images_use_only_supported_base64_media_types(self):
        body = {
            "input": [{
                "type": "message",
                "role": "user",
                "content": [{
                    "type": "input_image",
                    "image_url": "data:image/png;base64,AA==",
                }],
            }]
        }
        projected = responses_to_anthropic(body, "upstream")
        self.assertEqual(
            projected["messages"][0]["content"][0]["source"]["media_type"],
            "image/png",
        )
        body["input"][0]["content"][0]["image_url"] = (
            "data:image/x-emp-unsupported;base64,AA=="
        )
        with self.assertRaises(RouterError):
            responses_to_anthropic(body, "upstream")

    def test_stream_signal_ignores_terminal_words_inside_model_text(self):
        text_frame = (
            b'data: {"type":"response.output_text.delta","delta":"response.failed HTTP 404"}\n\n'
        )
        self.assertIsNone(router._stream_signal(text_frame))
        failed = router._stream_signal(
            b'data: {"type":"response.failed","response":{"status":"failed"}}\n\n'
        )
        self.assertFalse(failed["success"])

    def test_external_responses_drops_unbound_opaque_reasoning_state(self):
        provider = {
            "id": "destination",
            "protocol": "responses",
            "auth_mode": "api_key",
            "base_url": "https://example.invalid/v1",
        }
        model = {
            "id": "destination/model",
            "upstream_id": "model",
            "supports_reasoning": True,
        }
        body = {
            "model": "destination/model",
            "input": [
                {
                    "type": "reasoning",
                    "encrypted_content": "provider-a-opaque",
                    "summary": [],
                },
                {"type": "message", "role": "user", "content": "continue"},
            ],
        }
        prepared = router._prepare_reasoning_summary_route(
            {}, provider, model, body["model"], body
        )
        payload = router._responses_payload(provider, prepared, model)
        self.assertNotIn("provider-a-opaque", json.dumps(payload))

    def test_chat_stream_requires_done_and_tool_indexes_are_contiguous(self):
        provider = {
            "id": "demo",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "base_url": "https://example.invalid/v1",
        }
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "hello", "stream": True}
        truncated = _FakeResponse(
            [b'data: {"choices":[{"delta":{"content":"partial"},"finish_reason":"stop"}]}\n\n']
        )
        with patch.object(router, "_request", return_value=truncated):
            events = list(
                sse_json_events(router.stream_chat_completion(provider, body, model, {}))
            )
        self.assertEqual(events[-1]["type"], "response.failed")

        full_message_without_done = _FakeResponse([
            b'data: {"choices":[{"message":{"role":"assistant",'
            b'"content":"partial"},"finish_reason":"stop"}]}\n\n'
        ])
        with patch.object(router, "_request", return_value=full_message_without_done):
            events = list(
                sse_json_events(router.stream_chat_completion(provider, body, model, {}))
            )
        self.assertEqual(events[-1]["type"], "response.failed")

        body["tools"] = [{"type": "custom", "name": "shell", "description": "run"}]
        first = {
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 3,
                    "id": "call_1",
                    "function": {"name": "sh", "arguments": ""},
                }]}
            }]
        }
        second = {
            "choices": [{
                "delta": {"tool_calls": [{
                    "index": 3,
                    "function": {
                        "name": "ell",
                        "arguments": json.dumps({"input": "pwd"}),
                    },
                }]},
                "finish_reason": "tool_calls",
            }]
        }
        fragmented = _FakeResponse(
            [
                ("data: " + json.dumps(first) + "\n").encode(),
                b"\n",
                ("data: " + json.dumps(second) + "\n").encode(),
                b"\n",
                b"data: [DONE]\n",
                b"\n",
            ]
        )
        with patch.object(router, "_request", return_value=fragmented):
            events = list(
                sse_json_events(router.stream_chat_completion(provider, body, model, {}))
            )
        self.assertTrue(
            any(event.get("type") == "response.output_item.added" for event in events),
            events,
        )
        added = next(event for event in events if event.get("type") == "response.output_item.added")
        done = next(event for event in events if event.get("type") == "response.output_item.done")
        self.assertEqual(added["item"]["type"], "custom_tool_call")
        self.assertEqual(done["item"]["type"], "custom_tool_call")
        self.assertEqual(added["output_index"], 0)
        self.assertEqual(done["output_index"], 0)
        self.assertFalse(
            any(
                event.get("item", {}).get("type") == "message"
                for event in events
                if isinstance(event.get("item"), dict)
            )
        )

    def test_chat_stream_rejects_non_text_content_and_invalid_utf8(self):
        provider = {
            "id": "demo",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "base_url": "https://example.invalid/v1",
        }
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "hello", "stream": True}
        chunks = (
            [
                b'data: {"choices":[{"delta":{"content":{"bad":true}},"finish_reason":"stop"}]}\n',
                b"\n",
                b"data: [DONE]\n",
                b"\n",
            ],
            [
                b'data: {"choices":[{"delta":{"content":"bad\xfftext"},"finish_reason":"stop"}]}\n',
                b"\n",
                b"data: [DONE]\n",
                b"\n",
            ],
        )
        for raw_chunks in chunks:
            with self.subTest(raw_chunks=raw_chunks), patch.object(
                router, "_request", return_value=_FakeResponse(raw_chunks)
            ):
                events = list(
                    sse_json_events(
                        router.stream_chat_completion(provider, body, model, {})
                    )
                )
            self.assertEqual(
                [event["type"] for event in events].count("response.failed"), 1
            )
            self.assertEqual(events[-1]["type"], "response.failed")

    def test_anthropic_stream_requires_message_stop(self):
        provider = {
            "id": "demo",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "base_url": "https://example.invalid/v1",
        }
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "hello", "stream": True}
        chunks = [
            b'data: {"type":"content_block_delta","delta":{"type":"text_delta","text":"partial"}}\n',
            b"\n",
            b'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}\n',
            b"\n",
        ]
        with patch.object(router, "_request", return_value=_FakeResponse(chunks)):
            events = list(
                sse_json_events(
                    router.stream_anthropic_completion(provider, body, model, {})
                )
            )
        self.assertEqual(events[-1]["type"], "response.failed")

    def test_anthropic_stream_rejects_non_text_content_and_invalid_utf8(self):
        provider = {
            "id": "demo",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "base_url": "https://example.invalid/v1",
        }
        model = {"id": "demo/model"}
        body = {"model": "demo/model", "input": "hello", "stream": True}
        streams = (
            [
                b'data: {"type":"content_block_start","index":0,"content_block":{"type":"text","text":{"bad":true}}}\n',
                b"\n",
                b'data: {"type":"content_block_stop","index":0}\n',
                b"\n",
                b'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}\n',
                b"\n",
                b'data: {"type":"message_stop"}\n',
                b"\n",
            ],
            [
                b'data: {"type":"content_block_delta","index":0,"delta":{"type":"text_delta","text":"bad\xfftext"}}\n',
                b"\n",
                b'data: {"type":"message_delta","delta":{"stop_reason":"end_turn"}}\n',
                b"\n",
                b'data: {"type":"message_stop"}\n',
                b"\n",
            ],
        )
        for raw_chunks in streams:
            with self.subTest(raw_chunks=raw_chunks), patch.object(
                router, "_request", return_value=_FakeResponse(raw_chunks)
            ):
                events = list(
                    sse_json_events(
                        router.stream_anthropic_completion(provider, body, model, {})
                    )
                )
            self.assertEqual(
                [event["type"] for event in events].count("response.failed"), 1
            )
            self.assertEqual(events[-1]["type"], "response.failed")

    def test_master_key_path_rejects_parent_and_leaf_symlinks(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw)
            real_state = root / "real-state"
            real_state.mkdir()
            (real_state / "master.key").write_bytes(Fernet.generate_key() + b"\n")
            linked_state = root / "state"
            linked_state.symlink_to(real_state, target_is_directory=True)
            with self.assertRaises(VaultError):
                with default_master_key_file(linked_state / "master.key"):
                    ensure_master_key()

            safe_state = root / "safe-state"
            safe_state.mkdir()
            leaf = safe_state / "master.key"
            leaf.symlink_to(real_state / "master.key")
            with self.assertRaises(VaultError):
                with default_master_key_file(leaf):
                    ensure_master_key()


if __name__ == "__main__":
    unittest.main()
