import base64
import copy
import json
import tempfile
import threading
import unittest
from concurrent.futures import ThreadPoolExecutor
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.catalog import build_catalog
from easy_multi_provider.config import normalize
from easy_multi_provider.dialects import (
    ProjectionError,
    classify_dialect,
    project_request,
    project_response,
    project_stream_event,
    request_shape,
)
from easy_multi_provider.server import ObservationRing
from easy_multi_provider.transport import sse_json_events


class ResponsesDialectTests(unittest.TestCase):
    def test_portable_projection_always_sends_a_boolean_stream_flag(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}

        missing = project_request(provider, {"model": "provider/model", "input": "hello"})
        explicit_null = project_request(
            provider,
            {"model": "provider/model", "input": "hello", "stream": None},
        )
        streaming = project_request(
            provider,
            {"model": "provider/model", "input": "hello", "stream": True},
        )

        self.assertIs(missing["stream"], False)
        self.assertIs(explicit_null["stream"], False)
        self.assertIs(streaming["stream"], True)

    def test_portable_projection_rejects_a_non_boolean_stream_flag(self):
        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"},
                {"model": "provider/model", "input": "hello", "stream": "false"},
            )

        self.assertIn("class=invalid_stream", str(raised.exception))

    def test_portable_projection_rejects_unbound_previous_response_id(self):
        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"},
                {
                    "model": "provider/model",
                    "input": "continue",
                    "previous_response_id": "resp_previous",
                },
            )

        self.assertIn("class=stateful_response_unsupported", str(raised.exception))

    def test_portable_projection_preserves_visible_content_and_paired_tools(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        body = {
            "model": "provider/model",
            "instructions": "existing instruction",
            "client_metadata": {"thread_id": "redacted"},
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "visible"},
                        {"type": "input_image", "image_url": "https://example.com/image.png"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AA=="},
                    ],
                },
                {"type": "additional_tools"},
                {
                    "type": "message",
                    "role": "system",
                    "content": "system instruction",
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": [
                        {"type": "input_text", "text": "developer instruction"}
                    ],
                },
                {
                    "type": "reasoning",
                    "content": [{"type": "reasoning_text", "text": "redacted"}],
                },
                {
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "fixture_tool",
                    "arguments": "{}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_fixture",
                    "output": "fixture-result",
                },
            ],
        }

        self.assertEqual(classify_dialect(provider), "portable_responses")
        projected = project_request(provider, body)

        self.assertNotIn("client_metadata", projected)
        self.assertEqual(
            projected["instructions"],
            "existing instruction\n\nsystem instruction\n\ndeveloper instruction",
        )
        self.assertEqual(
            [item["type"] for item in projected["input"]],
            ["message", "function_call", "function_call_output"],
        )
        self.assertEqual(
            [part["type"] for part in projected["input"][0]["content"]],
            ["input_text", "input_image", "input_image"],
        )
        self.assertEqual(projected["input"][1]["call_id"], "call_fixture")
        self.assertEqual(projected["input"][2]["call_id"], "call_fixture")

    def test_portable_projection_rejects_non_text_instruction_content_without_content(self):
        body = {
            "model": "provider/model",
            "input": [
                {
                    "type": "message",
                    "role": "developer",
                    "content": [
                        {
                            "type": "input_image",
                            "image_url": "data:image/png;base64,cHJpdmF0ZQ==",
                        }
                    ],
                }
            ],
        }

        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"}, body
            )

        message = str(raised.exception)
        self.assertIn("index=0", message)
        self.assertIn("type=message", message)
        self.assertIn("parts=input_image", message)
        self.assertIn("class=unsupported_instruction_content", message)
        self.assertNotIn("cHJpdmF0ZQ", message)

    def test_native_projection_drops_plaintext_reasoning_but_keeps_final_output(self):
        provider = {"protocol": "responses", "auth_mode": "account"}
        body = {
            "model": "account/model",
            "input": [
                {
                    "type": "reasoning",
                    "content": [{"type": "reasoning_text", "text": "redacted"}],
                },
                {
                    "type": "reasoning",
                    "encrypted_content": "opaque-native-state",
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "final"}],
                },
            ],
        }

        self.assertEqual(classify_dialect(provider), "codex_native")
        projected = project_request(provider, body)

        self.assertEqual(
            [item["type"] for item in projected["input"]],
            ["reasoning", "message"],
        )
        self.assertEqual(
            projected["input"][0]["encrypted_content"], "opaque-native-state"
        )
        self.assertEqual(projected["input"][1]["content"][0]["text"], "final")

    def test_portable_projection_decodes_only_emp_compaction_summaries(self):
        summary = base64.urlsafe_b64encode(b"portable summary").decode("ascii")
        body = {
            "model": "provider/model",
            "input": [
                {"type": "compaction", "encrypted_content": "emp1:" + summary},
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "continue"}],
                },
            ],
        }

        projected = project_request(
            {"protocol": "responses", "auth_mode": "api_key"}, body
        )

        self.assertEqual(
            [item["type"] for item in projected["input"]], ["message", "message"]
        )
        self.assertIn("portable summary", projected["input"][0]["content"][0]["text"])

    def test_portable_projection_preserves_named_standalone_tool_output(self):
        item = {
            "type": "function_call_output",
            "name": "notifications",
            "namespace": "slack",
            "output": "Alice mentioned you.",
        }

        projected = project_request(
            {"protocol": "responses", "auth_mode": "api_key"},
            {"model": "provider/model", "input": [item]},
        )

        self.assertEqual(projected["input"], [item])
        self.assertEqual(
            request_shape({"input": [item]})["tool_pairing_status"],
            "standalone",
        )

    def test_native_projection_decodes_emp_compaction_and_preserves_surrounding_history(self):
        summary = base64.urlsafe_b64encode(b"portable checkpoint").decode("ascii")
        body = {
            "model": "gpt-native",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "visible"},
                        {
                            "type": "input_image",
                            "image_url": "data:image/png;base64,AA==",
                        },
                    ],
                },
                {"type": "compaction", "encrypted_content": "emp1:" + summary},
                {
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "fixture_tool",
                    "arguments": "{}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_fixture",
                    "output": "fixture-result",
                },
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [{"type": "output_text", "text": "done"}],
                },
            ],
        }

        projected = project_request(
            {"protocol": "responses", "auth_mode": "forward"}, body
        )

        self.assertEqual(
            [item["type"] for item in projected["input"]],
            [
                "message",
                "message",
                "function_call",
                "function_call_output",
                "message",
            ],
        )
        checkpoint = projected["input"][1]
        self.assertEqual(checkpoint["role"], "user")
        self.assertIn("portable checkpoint", checkpoint["content"][0]["text"])
        self.assertNotIn("emp1:", json.dumps(projected))
        self.assertEqual(
            projected["input"][0]["content"][1]["image_url"],
            "data:image/png;base64,AA==",
        )
        self.assertEqual(projected["input"][2]["call_id"], "call_fixture")
        self.assertEqual(projected["input"][3]["call_id"], "call_fixture")

    def test_portable_projection_rejects_opaque_compaction_without_content(self):
        body = {
            "model": "provider/model",
            "input": [
                {"type": "compaction", "encrypted_content": "private-opaque-state"}
            ],
        }

        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"}, body
            )

        message = str(raised.exception)
        self.assertIn("index=0", message)
        self.assertIn("type=compaction", message)
        self.assertIn("class=opaque_compaction", message)
        self.assertNotIn("private-opaque-state", message)

    def test_portable_projection_rejects_item_reference_without_content(self):
        body = {
            "model": "provider/model",
            "input": [{"type": "item_reference", "id": "private-reference"}],
        }

        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"}, body
            )

        message = str(raised.exception)
        self.assertIn("index=0", message)
        self.assertIn("type=item_reference", message)
        self.assertIn("class=opaque_item_reference", message)
        self.assertNotIn("private-reference", message)

    def test_projection_error_reports_shape_without_content(self):
        body = {
            "model": "provider/model",
            "input": [
                {
                    "type": "unsupported_visible_item",
                    "content": [{"type": "custom_part", "text": "private-value"}],
                }
            ],
        }

        with self.assertRaises(ProjectionError) as raised:
            project_request(
                {"protocol": "responses", "auth_mode": "api_key"}, body
            )

        message = str(raised.exception)
        self.assertIn("index=0", message)
        self.assertIn("type=unsupported_visible_item", message)
        self.assertIn("parts=custom_part", message)
        self.assertIn("class=unsupported_item", message)
        self.assertNotIn("private-value", message)

    def test_external_response_projection_never_returns_plaintext_reasoning_state(self):
        response = {
            "id": "response_fixture",
            "status": "completed",
            "output": [
                {
                    "id": "reasoning_fixture",
                    "type": "reasoning",
                    "content": [{"type": "reasoning_text", "text": "redacted"}],
                },
                {
                    "id": "message_fixture",
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {"type": "output_text", "text": "final"},
                        {
                            "type": "output_image",
                            "image_url": "data:image/png;base64,AA==",
                        },
                    ],
                },
                {
                    "id": "function_fixture",
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "fixture_tool",
                    "arguments": "{}",
                },
            ],
        }

        projected = project_response(
            {"protocol": "responses", "auth_mode": "api_key"}, response
        )

        self.assertEqual(
            [item["type"] for item in projected["output"]],
            ["message", "function_call"],
        )
        self.assertEqual(
            [part["type"] for part in projected["output"][0]["content"]],
            ["output_text", "output_image"],
        )
        self.assertNotIn("reasoning", str(projected).lower())

    def test_request_shape_contains_types_and_pairing_but_no_content(self):
        body = {
            "model": "provider/model",
            "input": [
                {
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "private-value"},
                        {"type": "input_image", "image_url": "data:image/png;base64,AA=="},
                    ],
                },
                {
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "fixture_tool",
                    "arguments": "{\"value\":\"private-value\"}",
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_fixture",
                    "output": "private-value",
                },
            ],
        }

        shape = request_shape(body)

        self.assertEqual(shape["request_item_count"], 3)
        self.assertEqual(
            shape["request_item_types"],
            ["message", "function_call", "function_call_output"],
        )
        self.assertEqual(
            shape["content_part_types"], ["input_text", "input_image"]
        )
        self.assertEqual(shape["tool_pairing_status"], "paired")
        self.assertNotIn("private-value", str(shape))
        self.assertNotIn("fixture_tool", str(shape))

    def test_request_shape_tracks_custom_tool_pair_without_content(self):
        shape = request_shape(
            {
                "input": [
                    {
                        "type": "custom_tool_call",
                        "call_id": "call_fixture",
                        "name": "fixture_tool",
                        "input": "private-input",
                    },
                    {
                        "type": "custom_tool_call_output",
                        "call_id": "call_fixture",
                        "output": "private-output",
                    },
                ]
            }
        )

        self.assertEqual(shape["tool_pairing_status"], "paired")
        self.assertNotIn("private", str(shape))

    def test_concurrent_explicit_routes_do_not_mutate_or_share_provider_request_state(self):
        provider = {
            "id": "provider-fixture",
            "enabled": True,
            "protocol": "responses",
            "auth_mode": "api_key",
        }
        model = {
            "id": "provider-fixture/model-fixture",
            "provider": provider["id"],
            "upstream_id": "model-fixture",
            "enabled": True,
        }
        config = {"providers": [provider], "models": [model]}
        original_provider = copy.deepcopy(provider)
        first_entered = threading.Event()
        release_first = threading.Event()

        def forward(_provider, body, _model, _incoming, _context_check=None):
            if len(body["input"]) == 1:
                first_entered.set()
                self.assertTrue(release_first.wait(2))
            else:
                self.assertTrue(first_entered.wait(2))
                release_first.set()
            return 200, "application/json", b"{}"

        first_body = {
            "model": model["id"],
            "input": [{"type": "message", "role": "user", "content": "one"}],
        }
        second_body = {
            "model": model["id"],
            "input": [
                {"type": "message", "role": "user", "content": "one"},
                {"type": "message", "role": "assistant", "content": "two"},
            ],
        }

        with patch.object(router, "forward_responses", side_effect=forward):
            with ThreadPoolExecutor(max_workers=2) as pool:
                first = pool.submit(router.proxy, config, first_body, {})
                self.assertTrue(first_entered.wait(2))
                second = pool.submit(router.proxy, config, second_body, {})
                first_metadata, _ = first.result(timeout=2)
                second_metadata, _ = second.result(timeout=2)

        self.assertEqual(first_metadata["request_item_count"], 1)
        self.assertEqual(second_metadata["request_item_count"], 2)
        self.assertEqual(provider, original_provider)

    def test_forward_responses_applies_request_and_response_projection(self):
        captured = {}

        class Response:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self):
                self._raw = json.dumps(
                    {
                        "status": "completed",
                        "output": [
                            {"type": "reasoning", "content": []},
                            {
                                "type": "message",
                                "role": "assistant",
                                "content": [{"type": "output_text", "text": "final"}],
                            },
                        ],
                    }
                ).encode("utf-8")

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return None

            def read(self, size=-1):
                raw, self._raw = self._raw, b""
                return raw

            def close(self):
                pass

        def request(
            provider,
            payload,
            incoming,
            stream=False,
            operation="",
            context_check=None,
            allow_retries=True,
        ):
            captured["payload"] = payload
            return Response()

        provider = {
            "id": "provider",
            "protocol": "responses",
            "auth_mode": "api_key",
            "base_url": "https://example.com/v1",
        }
        body = {
            "model": "provider/model",
            "client_metadata": {"thread_id": "redacted"},
            "input": [
                {"type": "reasoning", "content": []},
                {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "visible"}],
                },
            ],
        }

        with patch.object(router, "_request", side_effect=request):
            _, _, raw = router.forward_responses(
                provider, body, {"id": "provider/model", "upstream_id": "model"}, {}
            )

        self.assertNotIn("client_metadata", captured["payload"])
        self.assertEqual(
            [item["type"] for item in captured["payload"]["input"]], ["message"]
        )
        self.assertEqual(
            [item["type"] for item in json.loads(raw)["output"]], ["message"]
        )

    def test_external_stream_projection_suppresses_reasoning_events(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        state = set()
        reasoning_added = {
            "type": "response.output_item.added",
            "item": {"id": "reasoning_fixture", "type": "reasoning"},
        }
        reasoning_delta = {
            "type": "response.reasoning_text.delta",
            "item_id": "reasoning_fixture",
            "delta": "redacted",
        }
        visible_delta = {
            "type": "response.output_text.delta",
            "item_id": "message_fixture",
            "delta": "final",
        }
        completed = {
            "type": "response.completed",
            "response": {
                "status": "completed",
                "output": [
                    {"id": "reasoning_fixture", "type": "reasoning"},
                    {
                        "id": "message_fixture",
                        "type": "message",
                        "content": [{"type": "output_text", "text": "final"}],
                    },
                ],
            },
        }

        self.assertIsNone(project_stream_event(provider, reasoning_added, state))
        self.assertIsNone(project_stream_event(provider, reasoning_delta, state))
        self.assertEqual(
            project_stream_event(provider, visible_delta, state), visible_delta
        )
        projected_completed = project_stream_event(provider, completed, state)
        self.assertEqual(
            [item["type"] for item in projected_completed["response"]["output"]],
            ["message"],
        )

    def test_structured_reasoning_summary_and_opaque_state_are_opt_in(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        response = {
            "status": "completed",
            "output": [
                {
                    "type": "reasoning",
                    "id": "reasoning_fixture",
                    "status": "completed",
                    "encrypted_content": "opaque-fixture",
                    "summary": [
                        {"type": "summary_text", "text": "bounded summary"},
                        {"type": "reasoning_text", "text": "private chain"},
                    ],
                    "content": [
                        {"type": "reasoning_text", "text": "private chain"}
                    ],
                }
            ],
        }

        self.assertEqual(project_response(provider, response)["output"], [])
        projected = project_response(
            provider,
            response,
            preserve_reasoning_summary=True,
            preserve_reasoning_state=True,
        )

        self.assertEqual(
            projected["output"],
            [
                {
                    "type": "reasoning",
                    "id": "reasoning_fixture",
                    "status": "completed",
                    "encrypted_content": "opaque-fixture",
                    "summary": [
                        {"type": "summary_text", "text": "bounded summary"}
                    ],
                }
            ],
        )
        self.assertNotIn("private chain", json.dumps(projected))

    def test_portable_response_rejects_invalid_output_items(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        with self.assertRaises(ProjectionError):
            project_response(
                provider,
                {"status": "completed", "output": ["not-an-item"]},
            )

        request = {
            "model": "provider/model",
            "input": [
                {
                    "type": "reasoning",
                    "id": "reasoning_fixture",
                    "status": "completed",
                    "encrypted_content": "opaque-fixture",
                    "content": [
                        {"type": "reasoning_text", "text": "private chain"}
                    ],
                }
            ],
        }
        projected_request = project_request(
            provider, request, preserve_reasoning_state=True
        )
        self.assertEqual(
            projected_request["input"],
            [
                {
                    "type": "reasoning",
                    "id": "reasoning_fixture",
                    "status": "completed",
                    "encrypted_content": "opaque-fixture",
                }
            ],
        )

    def test_reasoning_summary_stream_is_forwarded_only_when_enabled(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        summary = {
            "type": "response.reasoning_summary_text.delta",
            "item_id": "reasoning_fixture",
            "delta": "bounded summary",
        }
        raw_reasoning = {
            "type": "response.reasoning_text.delta",
            "item_id": "reasoning_fixture",
            "delta": "private chain",
        }

        self.assertIsNone(project_stream_event(provider, summary, set()))
        self.assertEqual(
            project_stream_event(
                provider,
                summary,
                set(),
                preserve_reasoning_summary=True,
            ),
            summary,
        )
        self.assertIsNone(
            project_stream_event(
                provider,
                raw_reasoning,
                set(),
                preserve_reasoning_summary=True,
            )
        )

    def test_forward_responses_stream_projects_reasoning_before_codex(self):
        events = [
            {
                "type": "response.created",
                "response": {"id": "response_fixture", "status": "in_progress"},
            },
            {
                "type": "response.output_item.added",
                "item": {"id": "reasoning_fixture", "type": "reasoning"},
            },
            {
                "type": "response.reasoning_text.delta",
                "item_id": "reasoning_fixture",
                "delta": "redacted",
            },
            {
                "type": "response.output_text.delta",
                "item_id": "message_fixture",
                "delta": "final",
            },
            {
                "type": "response.completed",
                "response": {
                    "id": "response_fixture",
                    "status": "completed",
                    "output": [
                        {"id": "reasoning_fixture", "type": "reasoning"},
                        {
                            "id": "message_fixture",
                            "type": "message",
                            "role": "assistant",
                            "content": [
                                {"type": "output_text", "text": "final"}
                            ],
                        },
                    ],
                },
            },
        ]

        class Response:
            status = 200
            headers = {"Content-Type": "text/event-stream"}

            def __iter__(self):
                return iter(
                    [
                        (
                            "event: %s\ndata: %s\n\n"
                            % (event["type"], json.dumps(event))
                        ).encode("utf-8")
                        for event in events
                    ]
                )

            def close(self):
                pass

        provider = {
            "id": "provider",
            "protocol": "responses",
            "auth_mode": "api_key",
            "base_url": "https://example.com/v1",
        }
        body = {
            "model": "provider/model",
            "input": [{"type": "message", "role": "user", "content": []}],
            "stream": True,
        }
        with patch.object(router, "_request", return_value=Response()):
            raw = b"".join(
                router.forward_responses_stream(
                    provider,
                    body,
                    {"id": "provider/model", "upstream_id": "model"},
                    {},
                )
            ).decode("utf-8")

        self.assertNotIn("reasoning", raw.lower())
        self.assertNotIn("redacted", raw)
        self.assertIn("final", raw)
        projected_events = list(sse_json_events([raw.encode("utf-8")]))
        self.assertEqual(
            sum(
                event.get("type") == "response.completed"
                for event in projected_events
            ),
            1,
            projected_events,
        )

    def test_observation_ring_keeps_only_bounded_request_shape(self):
        ring = ObservationRing()
        ring.record(
            {
                "route": "responses",
                "protocol": "responses",
                "dialect": "portable_responses",
                "request_item_count": 3,
                "request_item_types": [
                    "message",
                    "function_call",
                    "function_call_output",
                ],
                "content_part_types": ["input_text", "input_image"],
                "tool_pairing_status": "paired",
                "raw_content": "private-value",
            }
        )

        record = ring.snapshot()["records"][0]
        self.assertEqual(record["dialect"], "portable_responses")
        self.assertEqual(record["request_item_count"], 3)
        self.assertEqual(
            record["request_item_types"],
            ["message", "function_call", "function_call_output"],
        )
        self.assertEqual(record["content_part_types"], ["input_text", "input_image"])
        self.assertEqual(record["tool_pairing_status"], "paired")
        self.assertNotIn("private-value", str(record))

    def test_portable_projection_translates_representable_custom_tools(self):
        body = {
            "model": "provider/model",
            "input": [],
            "tools": [
                {
                    "type": "function",
                    "name": "fixture_tool",
                    "description": "fixture",
                    "parameters": {"type": "object"},
                },
                {"type": "custom", "name": "exec", "description": "Run code"},
            ],
        }

        projected = project_request(
            {"protocol": "responses", "auth_mode": "api_key"}, body
        )

        self.assertEqual([tool["name"] for tool in projected["tools"]], ["fixture_tool", "exec"])
        self.assertEqual(
            projected["tools"][1]["parameters"],
            {
                "type": "object",
                "properties": {"input": {"type": "string"}},
                "required": ["input"],
                "additionalProperties": False,
            },
        )

    def test_portable_custom_tool_round_trip_preserves_call_id(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        body = {
            "model": "provider/model",
            "input": [
                {
                    "type": "additional_tools",
                    "tools": [
                        {
                            "type": "namespace",
                            "name": "functions",
                            "tools": [
                                {"type": "custom", "name": "exec", "description": "Run code"}
                            ],
                        }
                    ],
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
        }

        projected = project_request(provider, body)

        self.assertEqual(projected["tools"][0]["name"], "exec")
        self.assertEqual(projected["input"][0]["type"], "function_call")
        self.assertEqual(projected["input"][0]["call_id"], "call_fixture")
        self.assertEqual(
            json.loads(projected["input"][0]["arguments"]),
            {"input": "text('ok')"},
        )
        self.assertEqual(projected["input"][1]["type"], "function_call_output")
        self.assertEqual(projected["input"][1]["call_id"], "call_fixture")

        response = project_response(
            provider,
            {
                "status": "completed",
                "output": [
                    {
                        "id": "call_fixture",
                        "type": "function_call",
                        "call_id": "call_fixture",
                        "name": "exec",
                        "arguments": '{"input":"text(\\"ok\\")"}',
                    }
                ],
            },
            custom_names={"exec"},
        )
        item = response["output"][0]
        self.assertEqual(item["type"], "custom_tool_call")
        self.assertTrue(item["id"].startswith("ctc_"))
        self.assertEqual(item["call_id"], "call_fixture")
        self.assertEqual(item["input"], 'text("ok")')

    def test_portable_custom_tool_stream_maps_arguments_to_custom_input(self):
        provider = {"protocol": "responses", "auth_mode": "api_key"}
        state = {}
        suppressed = set()
        added = project_stream_event(
            provider,
            {
                "type": "response.output_item.added",
                "output_index": 0,
                "item": {
                    "id": "call_fixture",
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "exec",
                    "arguments": "",
                },
            },
            suppressed,
            custom_names={"exec"},
            custom_state=state,
        )
        self.assertEqual(added["item"]["type"], "custom_tool_call")
        self.assertTrue(added["item"]["id"].startswith("ctc_"))
        self.assertIsNone(
            project_stream_event(
                provider,
                {
                    "type": "response.function_call_arguments.delta",
                    "item_id": "call_fixture",
                    "delta": '{"input":"read',
                },
                suppressed,
                custom_names={"exec"},
                custom_state=state,
            )
        )
        done = project_stream_event(
            provider,
            {
                "type": "response.function_call_arguments.done",
                "item_id": "call_fixture",
                "arguments": '{"input":"read file"}',
            },
            suppressed,
            custom_names={"exec"},
            custom_state=state,
        )
        self.assertEqual(done["type"], "response.custom_tool_call_input.delta")
        self.assertEqual(done["delta"], "read file")

    def test_portable_projection_preserves_assistant_output_images(self):
        body = {
            "model": "provider/model",
            "input": [
                {
                    "type": "message",
                    "role": "assistant",
                    "content": [
                        {
                            "type": "output_image",
                            "image_url": "data:image/png;base64,AA==",
                        }
                    ],
                }
            ],
        }

        projected = project_request(
            {"protocol": "responses", "auth_mode": "api_key"}, body
        )

        self.assertEqual(
            projected["input"][0]["content"][0]["type"], "output_image"
        )


class CanonicalCatalogLabelTests(unittest.TestCase):
    def test_external_labels_use_exact_canonical_slug_and_one_context_suffix(self):
        with tempfile.TemporaryDirectory() as directory:
            native_path = Path(directory) / "native.json"
            native_path.write_text(
                json.dumps(
                    {
                        "models": [
                            {
                                "slug": "native-model",
                                "display_name": "Native Model [258K]",
                                "context_window": 258000,
                            }
                        ]
                    }
                ),
                encoding="utf-8",
            )
            config = normalize(
                {
                    "native_catalog_path": str(native_path),
                    "accounts": [
                        {
                            "id": "account",
                            "name": "Account",
                            "prefix": "account",
                            "auth_file": str(Path(directory) / "account.enc"),
                        }
                    ],
                    "providers": [
                        {
                            "id": "provider-a",
                            "base_url": "https://example.com/v1",
                        },
                        {
                            "id": "provider-b",
                            "base_url": "https://example.org/v1",
                        },
                    ],
                    "models": [
                        {
                            "id": "provider-a/shared-model",
                            "provider": "provider-a",
                            "upstream_id": "shared-model",
                            "display_name": "Friendly Name [250K]",
                            "context_window": 250000,
                        },
                        {
                            "id": "provider-b/shared-model",
                            "provider": "provider-b",
                            "upstream_id": "shared-model",
                            "display_name": "Friendly Name",
                            "context_window": 250000,
                        },
                    ],
                }
            )

            models = {item["slug"]: item for item in build_catalog(config)["models"]}

        self.assertEqual(models["native-model"]["display_name"], "[ 258K]  Native Model")
        self.assertEqual(
            models["account/native-model"]["display_name"],
            "[ 258K]  Account · Native Model",
        )
        self.assertEqual(
            models["provider-a/shared-model"]["display_name"],
            "[ 250K]  provider-a/shared-model",
        )
        self.assertEqual(
            models["provider-b/shared-model"]["display_name"],
            "[ 250K]  provider-b/shared-model",
        )
        self.assertNotEqual(
            models["provider-a/shared-model"]["display_name"],
            models["provider-b/shared-model"]["display_name"],
        )


if __name__ == "__main__":
    unittest.main()
