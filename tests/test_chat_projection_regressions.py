"""Preserve content and measurements across the Chat/Responses boundary."""

import json
import unittest
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.protocol_adapters import body_with_supported_effort
from easy_multi_provider.protocol_projection import _response_from_chat, _response_from_anthropic, responses_to_chat, responses_to_anthropic
from easy_multi_provider.dialects import project_request
from easy_multi_provider.router_errors import RouterError
from easy_multi_provider.stream_adapters import _response_json_stream
from easy_multi_provider.transport import sse_json_events


class ChatProjectionRegressions(unittest.TestCase):
    usage = {
        "prompt_tokens": 120, "completion_tokens": 30, "total_tokens": 150,
        "prompt_tokens_details": {"cached_tokens": 80},
        "completion_tokens_details": {"reasoning_tokens": 20},
    }
    expected_usage = {
        "input_tokens": 120, "output_tokens": 30, "total_tokens": 150,
        "input_tokens_details": {"cached_tokens": 80},
        "output_tokens_details": {"reasoning_tokens": 20},
    }

    def stream(self, chunks):
        class Upstream:
            def __iter__(self):
                wire = "".join("data: " + json.dumps(chunk) + "\n\n" for chunk in chunks) + "data: [DONE]\n\n"
                return iter(wire.encode().splitlines(keepends=True))

            def close(self):
                pass

        with patch.object(router, "_request", return_value=Upstream()) as request:
            raw = b"".join(router.stream_chat_completion(
                {"id": "test", "protocol": "chat_completions", "auth_mode": "api_key", "base_url": "https://example.com/v1"},
                {"model": "test/model", "input": "hello", "stream": True},
                {"id": "test/model"}, {},
            )).decode()
        self.assertEqual(request.call_args.args[1]["stream_options"], {"include_usage": True})
        return [json.loads(line[6:]) for line in raw.splitlines() if line.startswith("data: ")]

    def test_refusal_and_usage_survive_json_projection(self):
        for text in (None, "Explanation"):
            result = _response_from_chat({"choices": [{"message": {
                "content": text, "refusal": "I cannot assist with that."
            }, "finish_reason": "stop"}], "usage": self.usage}, "test/model")
            self.assertEqual(result["status"], "completed")
            self.assertEqual(result["output"][0]["content"][-1], {
                "type": "refusal", "refusal": "I cannot assist with that."
            })
            self.assertEqual(result["usage"], self.expected_usage)
            replay = responses_to_chat({"input": result["output"]}, "test/model")
            self.assertEqual(replay["messages"][0]["content"][-1], result["output"][0]["content"][-1])
            events = list(sse_json_events(_response_json_stream(result)))
            refusal_delta = next(e for e in events if e["type"] == "response.refusal.delta")
            self.assertEqual(refusal_delta["delta"], "I cannot assist with that.")
            self.assertEqual(refusal_delta["content_index"], 1 if text else 0)
            portable = project_request({"protocol": "responses", "auth_mode": "api_key"}, {"input": result["output"]})
            self.assertEqual(portable["input"][0]["content"][-1]["refusal"], "I cannot assist with that.")
            anthropic = responses_to_anthropic({"input": result["output"]}, "test/model")
            self.assertEqual(anthropic["messages"][0]["content"][-1]["text"], "I cannot assist with that.")

    def test_refusal_stream_and_usage_only_tail(self):
        events = self.stream([
            {"choices": [{"delta": {"refusal": "I cannot "}}]},
            {"choices": [{"delta": {"refusal": "assist."}, "finish_reason": "stop"}]},
            {"choices": [], "usage": self.usage},
        ])
        self.assertEqual(events[-1]["type"], "response.completed")
        self.assertEqual(events[-1]["response"]["usage"], self.expected_usage)
        self.assertEqual(events[-1]["response"]["output"][0]["content"], [
            {"type": "refusal", "refusal": "I cannot assist."}
        ])
        self.assertEqual("".join(e["delta"] for e in events if e["type"] == "response.refusal.delta"), "I cannot assist.")
        self.assertEqual(next(e for e in events if e["type"] == "response.refusal.done")["refusal"], "I cannot assist.")

    def test_chat_reasoning_stays_separate_from_answer(self):
        result = _response_from_chat({"choices": [{"message": {
            "reasoning_content": "Check the sum.",
            "content": "Four.",
        }, "finish_reason": "stop"}]}, "test/model")
        self.assertEqual([item["type"] for item in result["output"]], ["reasoning", "message"])
        self.assertEqual(result["output"][0]["content"], [
            {"type": "reasoning_text", "text": "Check the sum."}
        ])
        self.assertEqual(result["output"][0]["summary"], [])
        self.assertEqual(result["output_text"], "Four.")
        events = list(sse_json_events(_response_json_stream(result)))
        self.assertEqual(
            [event["item"]["type"] for event in events if event["type"] == "response.output_item.done"],
            ["reasoning", "message"],
        )

    def test_plaintext_agent_message_becomes_portable_user_turn(self):
        item = {
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/worker",
            "content": [
                {"type": "input_text", "text": "Message Type: NEW_TASK"},
                {"type": "input_text", "text": "Implement the requested UI."},
            ],
        }

        chat = responses_to_chat({"input": [item]}, "chat-model")
        self.assertEqual(chat["messages"], [{
            "role": "user",
            "content": "Message Type: NEW_TASK\nImplement the requested UI.",
        }])
        anthropic = responses_to_anthropic({"input": [item]}, "claude-model")
        self.assertEqual(anthropic["messages"], [{
            "role": "user",
            "content": [{
                "type": "text",
                "text": "Message Type: NEW_TASK\nImplement the requested UI.",
            }],
        }])

    def test_agent_message_rejects_opaque_or_malformed_content(self):
        valid = {
            "type": "agent_message",
            "author": "/root",
            "recipient": "/root/worker",
        }
        for content in (
            [],
            [{"type": "encrypted_content", "encrypted_content": "opaque"}],
            [{"type": "input_text", "text": ""}],
        ):
            with self.subTest(content=content):
                with self.assertRaises(RouterError):
                    responses_to_chat({"input": [{**valid, "content": content}]}, "chat-model")
                with self.assertRaises(RouterError):
                    responses_to_anthropic(
                        {"input": [{**valid, "content": content}]}, "claude-model"
                    )

    def test_chat_stream_reasoning_and_answer_have_distinct_items(self):
        events = self.stream([
            {"choices": [{"delta": {"content": None, "reasoning_content": "Check "}}]},
            {"choices": [{"delta": {"reasoning_content": "the sum.", "content": "Four."}}]},
            {"choices": [{"delta": {}, "finish_reason": "stop"}]},
        ])
        self.assertEqual(
            "".join(event["delta"] for event in events if event["type"] == "response.reasoning_text.delta"),
            "Check the sum.",
        )
        self.assertEqual(
            "".join(event["delta"] for event in events if event["type"] == "response.output_text.delta"),
            "Four.",
        )
        completed = events[-1]["response"]
        self.assertEqual([item["type"] for item in completed["output"]], ["reasoning", "message"])
        self.assertEqual(completed["output"][0]["content"][0]["text"], "Check the sum.")
        self.assertEqual(completed["output_text"], "Four.")
        item_events = [event for event in events if event["type"] in {
            "response.output_item.added", "response.output_item.done"
        }]
        self.assertEqual([event["type"] for event in item_events], [
            "response.output_item.added", "response.output_item.done",
            "response.output_item.added", "response.output_item.done",
        ])

    def test_chat_stream_reasoning_after_answer_keeps_output_indices(self):
        events = self.stream([
            {"choices": [{"delta": {"content": "First."}}]},
            {"choices": [{"delta": {"reasoning_content": "Late reasoning."}, "finish_reason": "stop"}]},
        ])
        completed = events[-1]["response"]
        self.assertEqual([item["type"] for item in completed["output"]], ["message", "reasoning"])
        added = [event for event in events if event["type"] == "response.output_item.added"]
        self.assertEqual([event["output_index"] for event in added], [0, 1])
        reasoning_delta = next(event for event in events if event["type"] == "response.reasoning_text.delta")
        self.assertEqual(reasoning_delta["output_index"], 1)
        self.assertEqual(reasoning_delta["content_index"], 0)

    def test_chat_stream_message_closes_before_tool_call(self):
        events = self.stream([
            {"choices": [{"delta": {"content": "Calculating."}}]},
            {"choices": [{"delta": {"tool_calls": [{
                "index": 0, "id": "call_fixture", "type": "function",
                "function": {"name": "calculator", "arguments": '{"x":4}'},
            }]}, "finish_reason": "tool_calls"}]},
        ])
        item_events = [event for event in events if event["type"] in {
            "response.output_item.added", "response.output_item.done"
        }]
        self.assertEqual([(event["type"], event["item"]["type"]) for event in item_events], [
            ("response.output_item.added", "message"),
            ("response.output_item.done", "message"),
            ("response.output_item.added", "function_call"),
            ("response.output_item.done", "function_call"),
        ])

    def test_literal_markup_split_across_chunks_is_text(self):
        pieces = ["Example: <thi", "nk>text</think> and <tool_", "call> is documentation."]
        events = self.stream([{"choices": [{"delta": {"content": p}}]} for p in pieces]
                             + [{"choices": [{"delta": {}, "finish_reason": "stop"}]}])
        self.assertEqual(events[-1]["type"], "response.completed")
        self.assertEqual(events[-1]["response"]["output_text"], "".join(pieces))
        self.assertNotIn("usage", events[-1]["response"])
        result = _response_from_anthropic({"content": [{"type": "text", "text": "".join(pieces)}], "stop_reason": "end_turn"}, "test/model")
        self.assertEqual(result["output_text"], "".join(pieces))

    def test_unknown_effort_capability_preserves_explicit_selection(self):
        body = {"reasoning": {"effort": "high", "summary": "auto"}}
        for model in ({}, {"reasoning_levels": None}, {"reasoning_levels": []}, {"reasoning_levels": [], "supports_reasoning": None}, {"reasoning_levels": ["high"]}):
            self.assertEqual(body_with_supported_effort({}, body, model), body)
        for model in ({"reasoning_levels": [], "supports_reasoning": False}, {"reasoning_levels": ["low"]}):
            self.assertEqual(body_with_supported_effort({}, body, model), {"reasoning": {"summary": "auto"}})
        self.assertEqual(body["reasoning"]["effort"], "high")

    def test_external_responses_preserves_codex_persistent_wire_alias(self):
        body = {"reasoning": {"effort": "disabled", "summary": "auto"}}
        model = {"reasoning_levels": ["persistent"], "supports_reasoning": True}

        projected = body_with_supported_effort(
            {"protocol": "responses", "auth_mode": "api_key"}, body, model
        )

        self.assertEqual(projected, body)

    def test_persistent_wire_alias_does_not_cross_protocol_boundaries(self):
        body = {"reasoning": {"effort": "disabled", "summary": "auto"}}
        model = {"reasoning_levels": ["persistent"], "supports_reasoning": True}

        for provider in (
            {"protocol": "chat_completions", "auth_mode": "api_key"},
            {"protocol": "anthropic_messages", "auth_mode": "anthropic_api_key"},
        ):
            with self.subTest(protocol=provider["protocol"]):
                self.assertEqual(
                    body_with_supported_effort(provider, body, model),
                    {"reasoning": {"summary": "auto"}},
                )

    def test_explicit_reasoning_disable_still_wins_over_alias(self):
        body = {"reasoning": {"effort": "disabled"}}
        model = {
            "reasoning_levels": ["persistent"],
            "supports_reasoning": False,
        }

        self.assertNotIn(
            "reasoning",
            body_with_supported_effort(
                {"protocol": "responses", "auth_mode": "api_key"}, body, model
            ),
        )

    def test_structured_output_and_reasoning_survive_external_projection(self):
        schema = {
            "type": "object",
            "properties": {"answer": {"type": "string"}},
            "required": ["answer"],
            "additionalProperties": False,
        }
        body = {
            "input": "return an answer",
            "reasoning": {"effort": "high"},
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "answer_schema",
                    "strict": True,
                    "schema": schema,
                }
            },
        }

        chat = responses_to_chat(body, "chat-model")
        self.assertEqual(chat["reasoning_effort"], "high")
        self.assertEqual(
            chat["response_format"],
            {
                "type": "json_schema",
                "json_schema": {
                    "name": "answer_schema",
                    "strict": True,
                    "schema": schema,
                },
            },
        )

        anthropic = responses_to_anthropic(body, "claude-model")
        self.assertEqual(anthropic["output_config"]["effort"], "high")
        self.assertEqual(
            anthropic["output_config"]["format"],
            {"type": "json_schema", "schema": schema},
        )
        self.assertNotIn("name", anthropic["output_config"]["format"])
        self.assertNotIn("strict", anthropic["output_config"]["format"])
        for unsupported in ("none", "minimal", "ultra"):
            with self.subTest(unsupported=unsupported), self.assertRaises(RouterError):
                responses_to_anthropic(
                    {"input": "hello", "reasoning": {"effort": unsupported}},
                    "claude-model",
                )
