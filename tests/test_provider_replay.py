import json
import unittest

from easy_multi_provider.provider_replay import (
    ProviderReplayCache,
    ProviderReplayScope,
)
from easy_multi_provider.protocol_projection import responses_to_chat
from easy_multi_provider.route_plan import resolve_route
from easy_multi_provider.codex_dispatch import provider_replay_scope


def _tool_item(call_id="call_fixture", signature="signature-fixture"):
    return {
        "type": "function_call",
        "id": call_id,
        "call_id": call_id,
        "name": "exec",
        "arguments": '{"cmd":"pwd"}',
        "extra_content": {"google": {"thought_signature": signature}},
    }


def _scope(
    model_id="demo/model",
    thread_id="thread-a",
    window_id="window-1",
    endpoint="sha256:endpoint-a",
    deployment="default",
    provider_id="demo",
    upstream_model="upstream-model",
):
    return ProviderReplayScope(
        provider_id=provider_id,
        endpoint_fingerprint=endpoint,
        deployment_identity=deployment,
        model_id=model_id,
        upstream_model=upstream_model,
        thread_id=thread_id,
        window_id=window_id,
    )


class ProviderReplayTests(unittest.TestCase):
    def test_scope_changes_when_resolved_upstream_model_changes(self):
        provider = {
            "id": "demo",
            "base_url": "https://example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
        }
        body = {
            "model": "demo/logical",
            "client_metadata": {
                "x-codex-turn-metadata": json.dumps({
                    "thread_id": "thread-a",
                    "turn_id": "turn-a",
                    "window_id": "window-a",
                })
            },
        }

        scopes = []
        for upstream in ("gemini-X", "gemini-Y"):
            config = {
                    "providers": [provider],
                    "models": [{
                        "id": "demo/logical",
                        "provider": "demo",
                        "upstream_id": upstream,
                    }],
                }
            scopes.append(provider_replay_scope(
                resolve_route(config, "demo/logical"),
                body,
                {},
            ))

        self.assertIsNotNone(scopes[0])
        self.assertIsNotNone(scopes[1])
        self.assertNotEqual(scopes[0].key("call-1"), scopes[1].key("call-1"))

    def test_stream_signature_is_replayed_into_next_chat_tool_call(self):
        cache = ProviderReplayCache()
        scope = _scope()
        event = {
            "type": "response.output_item.done",
            "item": _tool_item(),
        }
        raw = (
            "event: response.output_item.done\r\n"
            "data: %s\r\n\r\n" % json.dumps(event)
        ).encode()
        chunks = [raw[:31], raw[31:]]

        self.assertEqual(b"".join(cache.observe_stream(scope, iter(chunks))), raw)

        original = {
            "model": "demo/model",
            "input": [
                {
                    "type": "function_call",
                    "id": "call_fixture",
                    "call_id": "call_fixture",
                    "name": "exec",
                    "arguments": '{"cmd":"pwd"}',
                },
                {
                    "type": "function_call_output",
                    "call_id": "call_fixture",
                    "output": "/tmp",
                },
            ],
        }
        prepared = cache.prepare(original, scope)
        payload = responses_to_chat(prepared, "upstream-model")

        self.assertNotIn("extra_content", original["input"][0])
        self.assertEqual(
            payload["messages"][0]["tool_calls"][0]["extra_content"],
            {"google": {"thought_signature": "signature-fixture"}},
        )

    def test_nonstream_signature_is_model_and_call_scoped(self):
        cache = ProviderReplayCache()
        scope = _scope()
        cache.observe_bytes(
            scope,
            json.dumps({"output": [_tool_item()]}).encode(),
        )
        base = {
            "model": "other/model",
            "input": [
                {
                    "type": "function_call",
                    "call_id": "call_fixture",
                    "name": "exec",
                    "arguments": "{}",
                }
            ],
        }
        self.assertIs(cache.prepare(base, _scope(model_id="other/model")), base)
        base["model"] = "demo/model"
        base["input"][0]["call_id"] = "other_call"
        self.assertIs(cache.prepare(base, scope), base)

    def test_expired_and_evicted_signatures_are_not_replayed(self):
        now = [10.0]
        cache = ProviderReplayCache(capacity=1, ttl_seconds=1, clock=lambda: now[0])
        scope = _scope()
        cache.observe_value(scope, {"output": [_tool_item("call_old", "old")]})
        cache.observe_value(scope, {"output": [_tool_item("call_new", "new")]})

        old = {
            "model": "demo/model",
            "input": [{"type": "function_call", "call_id": "call_old"}],
        }
        self.assertIs(cache.prepare(old, scope), old)

        now[0] += 2
        new = {
            "model": "demo/model",
            "input": [{"type": "function_call", "call_id": "call_new"}],
        }
        self.assertIs(cache.prepare(new, scope), new)

    def test_signatures_are_isolated_by_route_thread_and_window(self):
        cache = ProviderReplayCache()
        scope_a = _scope(thread_id="thread-a")
        scope_b = _scope(thread_id="thread-b")
        cache.observe_value(scope_a, {"output": [_tool_item("call-1", "sig-A")]})
        cache.observe_value(scope_b, {"output": [_tool_item("call-1", "sig-B")]})
        body = {
            "model": "demo/model",
            "input": [{"type": "function_call", "call_id": "call-1"}],
        }

        prepared_a = cache.prepare(body, scope_a)
        prepared_b = cache.prepare(body, scope_b)

        self.assertEqual(
            prepared_a["input"][0]["extra_content"]["google"]["thought_signature"],
            "sig-A",
        )
        self.assertEqual(
            prepared_b["input"][0]["extra_content"]["google"]["thought_signature"],
            "sig-B",
        )
        self.assertIs(cache.prepare(body, _scope(window_id="window-2")), body)
        self.assertIs(cache.prepare(body, _scope(endpoint="sha256:endpoint-b")), body)
        self.assertIs(cache.prepare(body, _scope(provider_id="other")), body)
        self.assertIs(cache.prepare(body, None), body)


if __name__ == "__main__":
    unittest.main()
