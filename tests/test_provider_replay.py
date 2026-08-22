import json
import unittest

from easy_multi_provider.provider_replay import ProviderReplayCache
from easy_multi_provider.router import responses_to_chat


def _tool_item(call_id="call_fixture", signature="signature-fixture"):
    return {
        "type": "function_call",
        "id": call_id,
        "call_id": call_id,
        "name": "exec",
        "arguments": '{"cmd":"pwd"}',
        "extra_content": {"google": {"thought_signature": signature}},
    }


class ProviderReplayTests(unittest.TestCase):
    def test_stream_signature_is_replayed_into_next_chat_tool_call(self):
        cache = ProviderReplayCache()
        event = {
            "type": "response.output_item.done",
            "item": _tool_item(),
        }
        raw = (
            "event: response.output_item.done\r\n"
            "data: %s\r\n\r\n" % json.dumps(event)
        ).encode()
        chunks = [raw[:31], raw[31:]]

        self.assertEqual(b"".join(cache.observe_stream("demo/model", iter(chunks))), raw)

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
        prepared = cache.prepare(original)
        payload = responses_to_chat(prepared, "upstream-model")

        self.assertNotIn("extra_content", original["input"][0])
        self.assertEqual(
            payload["messages"][0]["tool_calls"][0]["extra_content"],
            {"google": {"thought_signature": "signature-fixture"}},
        )

    def test_nonstream_signature_is_model_and_call_scoped(self):
        cache = ProviderReplayCache()
        cache.observe_bytes(
            "demo/model",
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
        self.assertIs(cache.prepare(base), base)
        base["model"] = "demo/model"
        base["input"][0]["call_id"] = "other_call"
        self.assertIs(cache.prepare(base), base)

    def test_expired_and_evicted_signatures_are_not_replayed(self):
        now = [10.0]
        cache = ProviderReplayCache(capacity=1, ttl_seconds=1, clock=lambda: now[0])
        cache.observe_value("demo/model", {"output": [_tool_item("call_old", "old")]})
        cache.observe_value("demo/model", {"output": [_tool_item("call_new", "new")]})

        old = {
            "model": "demo/model",
            "input": [{"type": "function_call", "call_id": "call_old"}],
        }
        self.assertIs(cache.prepare(old), old)

        now[0] += 2
        new = {
            "model": "demo/model",
            "input": [{"type": "function_call", "call_id": "call_new"}],
        }
        self.assertIs(cache.prepare(new), new)


if __name__ == "__main__":
    unittest.main()
