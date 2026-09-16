"""Cross-protocol namespace and client tool discovery round trips."""
import copy
import json
import unittest
from unittest.mock import patch

from easy_multi_provider.tool_bridge import ExternalTools
from easy_multi_provider.dialects import project_request
from easy_multi_provider.protocol_projection import responses_to_chat, responses_to_anthropic
from easy_multi_provider.stream_adapters import _reliable_responses_stream
from easy_multi_provider.transport import sse_json_events
from easy_multi_provider.router_errors import RouterError
from easy_multi_provider.codex_history.models import normalize_visible_item
from easy_multi_provider.history_continuity import _wire_item
from easy_multi_provider.router import proxy
from easy_multi_provider.protocol_adapters import protocol_adapter
from easy_multi_provider.dialects import classify_dialect
from tests.test_codex_cli_demo import _tool_response_stream


def function(name="search"):
    return {"type": "function", "name": name, "parameters": {"type": "object"}}


def namespace(name, tool=None):
    return {"type": "namespace", "name": name, "tools": [tool or function()]}


def search():
    return {"type": "tool_search", "execution": "client", "description": "Search deferred tools",
            "parameters": {"type": "object", "properties": {"query": {"type": "string"}}, "required": ["query"]}}


class ExternalToolsTests(unittest.TestCase):
    def test_namespaced_names_choice_and_history_round_trip_in_all_protocols(self):
        body = {"input": [{"type": "function_call", "namespace": "one", "name": "search",
                           "call_id": "call", "arguments": '{"name":"do not rewrite"}'},
                          {"type": "function_call_output", "call_id": "call", "output": "result"}],
                "tools": [namespace("one"), namespace("two"), function()],
                "tool_choice": {"type": "function", "namespace": "two", "name": "search"}}
        original = copy.deepcopy(body)
        bridge = ExternalTools()
        prepared = bridge.prepare(body)
        names = [tool["name"] for tool in prepared["tools"]]
        self.assertEqual(len(set(names)), 3)
        self.assertEqual(names[-1], "search")
        self.assertIn("one.search", prepared["tools"][0]["description"])
        self.assertTrue(all(len(name) <= 64 for name in names))
        self.assertEqual(prepared["input"][0]["name"], names[0])
        self.assertEqual(prepared["tool_choice"]["name"], names[1])
        self.assertEqual(prepared["input"][0]["arguments"], body["input"][0]["arguments"])
        for convert in (lambda value: project_request({"protocol": "responses"}, value),
                        lambda value: responses_to_chat(value, "model"),
                        lambda value: responses_to_anthropic(value, "model")):
            self.assertEqual(len(convert(prepared)["tools"]), 3)
        for index, ns in enumerate(("one", "two")):
            restored = bridge.restore_response({"output": [{"type": "function_call", "name": names[index],
                                                            "call_id": "call", "arguments": "{}"}]})
            self.assertEqual(restored["output"][0]["namespace"], ns)
            self.assertEqual(restored["output"][0]["name"], "search")
        self.assertEqual(body, original)
        # New requests, definitions reordered, and interleaved sessions do not
        # depend on a process-wide alias registry.
        reversed_body = dict(body, tools=list(reversed(body["tools"])))
        again = ExternalTools().prepare(reversed_body)
        self.assertEqual(again["tool_choice"], prepared["tool_choice"])

    def test_custom_namespace_stream_and_cancellation(self):
        bridge = ExternalTools()
        prepared = bridge.prepare({"tools": [namespace("functions", {"type": "custom", "name": "exec"})]})
        event = bridge.restore_event({"type": "response.output_item.done", "item": {
            "type": "custom_tool_call", "name": prepared["tools"][0]["name"], "input": "text('hello')"}})
        self.assertEqual(event["item"]["namespace"], "functions")
        self.assertEqual(event["item"]["input"], "text('hello')")
        closed = []
        def upstream():
            try:
                yield _tool_response_stream("fixture", prepared["tools"][0]["name"])
            finally:
                closed.append(True)
        stream = _reliable_responses_stream(upstream, event_transform=bridge.restore_event)
        next(stream)
        stream.close()
        self.assertEqual(closed, [True])

    def test_search_stream_discovery_followup_and_history_reconstruction(self):
        bridge = ExternalTools()
        body = bridge.prepare({"input": "search calendar", "tools": [search()]})
        alias = body["tools"][0]["name"]
        # Use the same output frames as an ordinary external function call.
        raw = _tool_response_stream("fixture", alias)
        # The fake shell arguments are replaced with the actual search contract.
        events = list(sse_json_events([raw]))
        arguments = json.dumps({"query": "calendar", "limit": 1})
        for event in events:
            if event.get("type", "").endswith("arguments.delta"):
                event["delta"] = arguments
            if event.get("type", "").endswith("arguments.done"):
                event["arguments"] = arguments
            if event.get("type") == "response.output_item.done":
                event["item"]["arguments"] = arguments
            if "response" in event and event["response"].get("output"):
                event["response"]["output"][0]["arguments"] = arguments
        chunks = [("data: " + json.dumps(event) + "\n\n").encode() for event in events]
        observed, terminal = [], {}
        stream = _reliable_responses_stream(lambda: iter(chunks), terminal.update,
                                            event_transform=bridge.restore_event, on_event=observed.append)
        returned = list(sse_json_events(stream))
        self.assertTrue(terminal["success"])
        self.assertTrue(terminal["tool_activity"])
        self.assertFalse(any("function_call_arguments" in e["type"] for e in returned))
        call = next(e["item"] for e in returned if e["type"] == "response.output_item.done")
        self.assertEqual(call["type"], "tool_search_call")
        self.assertEqual(call["arguments"], {"query": "calendar", "limit": 1})
        self.assertTrue(call["id"].startswith("tsc_"))
        self.assertEqual(observed[-1]["response"]["output"][0], call)
        output = {"type": "tool_search_output", "call_id": call["call_id"], "execution": "client",
                  "status": "completed", "tools": [namespace("calendar", dict(function("create"), defer_loading=True))]}
        for item in (call, output):
            normalized = normalize_visible_item(item)
            self.assertIsNotNone(normalized)
            # Runtime wire types and discovered definitions survive materialization.
            materialized = _wire_item({"kind": normalized.kind, "content": normalized.content,
                                       "raw_type": normalized.raw_type, "call_id": normalized.call_id})
            self.assertEqual(materialized["type"], item["type"])
            if item is output:
                self.assertEqual(materialized["tools"], output["tools"])
        following = ExternalTools().prepare({"tools": [search()], "input": [call, output]})
        self.assertEqual(following["input"][1]["call_id"], call["call_id"])
        self.assertEqual(len(following["tools"]), 2)
        for convert in (lambda value: project_request({"protocol": "responses"}, value),
                        lambda value: responses_to_chat(value, "fixture"),
                        lambda value: responses_to_anthropic(value, "fixture")):
            self.assertEqual(len(convert(following)["tools"]), 2)

    def test_malformed_search_and_schema_collision_are_not_hidden(self):
        with self.assertRaises(RouterError):
            ExternalTools().prepare({"tools": [dict(search(), execution="server")]})
        with self.assertRaises(RouterError):
            ExternalTools().prepare({"tools": [namespace("a"), namespace("a", dict(function(), description="different"))]})
        bridge = ExternalTools()
        alias = bridge.prepare({"tools": [search()]})["tools"][0]["name"]
        with self.assertRaises(RouterError):
            bridge.restore_response({"output": [{"type": "function_call", "name": alias, "arguments": "[1]"}]})

    def test_allowed_tools_restricts_current_definitions_without_old_state(self):
        first = {"tools": [namespace("one"), namespace("two")], "tool_choice": {
            "type": "allowed_tools", "mode": "required", "tools": [{"type": "function", "name": "search", "namespace": "two"}]}}
        prepared = ExternalTools().prepare(first)
        self.assertEqual(len(prepared["tools"]), 1)
        self.assertEqual(prepared["tool_choice"], "required")
        second = ExternalTools().prepare({"tools": [function("new_tool")]})
        self.assertEqual(second["tools"][0]["name"], "new_tool")

    def test_router_restores_each_external_protocol_and_its_observer(self):
        for protocol, full_name, stream_name in (
            ("responses", "forward_responses", "forward_responses_stream"),
            ("chat_completions", "chat_completion", "stream_chat_completion"),
            ("anthropic_messages", "anthropic_completion", "stream_anthropic_completion"),
        ):
            config = {"providers": [{"id": "demo", "enabled": True, "auth_mode": "api_key", "protocol": protocol}],
                      "models": [{"id": "demo/model", "provider": "demo", "enabled": True}]}
            body = {"model": "demo/model", "input": "hello", "tools": [namespace("one"), namespace("two")]}
            def upstream(provider, prepared, model, incoming, *args, **kwargs):
                payload = protocol_adapter(classify_dialect(provider)).project_request(provider, prepared, model, "fixture")
                names = [t.get("function", t)["name"] for t in payload["tools"]]
                self.assertEqual(len(set(names)), 2)
                raw = _tool_response_stream("fixture", wire_name=names[1], tool_arguments={})
                if prepared.get("stream"):
                    return iter([raw])
                response = list(sse_json_events([raw]))[-1]["response"]
                return 200, "application/json", json.dumps(response).encode()
            with self.subTest(protocol=protocol), patch("easy_multi_provider.router." + full_name, upstream), \
                    patch("easy_multi_provider.router." + stream_name, upstream):
                metadata, raw = proxy(config, body, {})
                self.assertEqual(json.loads(raw)["output"][0]["namespace"], "two")
                observed = []
                def observer(event):
                    observed.append(event)
                    raise RuntimeError("fixture observer failure must not break forwarding")
                metadata, stream = proxy(config, dict(body, stream=True), {}, on_stream_event=observer)
                returned = list(sse_json_events(stream))
                self.assertEqual(returned[-1]["response"]["output"][0]["namespace"], "two")
                self.assertEqual(observed[-1]["response"]["output"][0]["name"], "search")


if __name__ == "__main__":
    unittest.main()
