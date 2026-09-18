import unittest

from easy_multi_provider.codex_history import normalize_visible_item
from easy_multi_provider.dialects import project_request
from easy_multi_provider.history_continuity import _wire_item
from easy_multi_provider.portable_checkpoint import (
    CompactionSummaryMissingError,
    build_compaction_replacement,
    build_visible_history,
)


class PortableCheckpointTests(unittest.TestCase):
    def test_replacement_contains_only_state_hidden_by_compaction(self):
        persisted = [
            {"kind": "user_message", "content": "old"},
            {"kind": "compaction_summary", "content": "visible summary"},
            {"kind": "assistant_message", "content": "active tail"},
        ]

        replacement = build_compaction_replacement(persisted)
        complete = build_visible_history(persisted)

        self.assertEqual([item["content"] for item in replacement], ["visible summary"])
        self.assertEqual(
            [item["content"] for item in complete],
            ["visible summary", "active tail"],
        )

    def test_opaque_compaction_replays_only_pre_compaction_visible_history(self):
        replacement = build_compaction_replacement(
            [
                {"kind": "user_message", "content": "constraint"},
                {"kind": "assistant_message", "content": "completed work"},
                {"kind": "compaction_summary", "content": ""},
                {"kind": "assistant_message", "content": "active tail"},
            ]
        )

        self.assertEqual(
            [item["content"] for item in replacement],
            ["constraint", "completed work"],
        )

    def test_tool_pairs_follow_codex_prompt_normalization(self):
        complete = build_visible_history(
            [
                {
                    "kind": "tool_result",
                    "call_id": "orphan",
                    "content": {"output": "ignored"},
                    "raw_type": "function_call_output",
                },
                {
                    "kind": "tool_call",
                    "call_id": "call-1",
                    "content": {"name": "read_file", "arguments": "{}"},
                    "raw_type": "custom_tool_call",
                },
            ]
        )

        self.assertEqual([item["call_id"] for item in complete], ["call-1", "call-1"])
        self.assertEqual(complete[1]["raw_type"], "custom_tool_call_output")
        self.assertEqual(complete[1]["content"]["output"], "aborted")

    def test_incomplete_tool_search_keeps_codex_search_pair_shape(self):
        complete = build_visible_history(
            [
                {
                    "kind": "tool_call",
                    "call_id": "search-1",
                    "content": {
                        "name": "tool_search",
                        "execution": "client",
                        "arguments": "{}",
                    },
                    "raw_type": "tool_search_call",
                }
            ]
        )

        self.assertEqual(
            [item["raw_type"] for item in complete],
            ["tool_search_call", "tool_search_output"],
        )
        self.assertEqual(complete[1]["content"]["execution"], "client")
        self.assertEqual(complete[1]["content"]["tools"], [])

    def test_server_tool_search_output_is_retained_as_data_only_history(self):
        item = normalize_visible_item(
            {
                "type": "tool_search_output",
                "call_id": "server-search-1",
                "execution": "server",
                "status": "completed",
                "tools": [{"type": "function", "name": "calendar"}],
            }
        )

        complete = build_visible_history([item])
        wire = _wire_item(complete[0])

        self.assertEqual(len(complete), 1)
        self.assertEqual(wire["type"], "function_call_output")
        self.assertNotIn("call_id", wire)
        self.assertIn('"execution": "server"', wire["output"])
        projected = project_request(
            {"protocol": "responses", "auth_mode": "api_key"},
            {"model": "external/model", "input": [wire]},
        )
        self.assertEqual(projected["input"][0]["type"], "function_call_output")
        self.assertNotIn("call_id", projected["input"][0])

    def test_named_standalone_output_is_not_forced_into_tool_pairing(self):
        item = normalize_visible_item({
            "type": "function_call_output",
            "name": "notifications",
            "namespace": "slack",
            "output": "Alice mentioned you.",
        })

        complete = build_visible_history([item])

        self.assertEqual(complete[0]["kind"], "standalone_tool_output")
        self.assertEqual(complete[0]["content"]["name"], "notifications")
        self.assertEqual(complete[0]["content"]["namespace"], "slack")

    def test_compaction_replacement_requires_a_boundary(self):
        with self.assertRaises(CompactionSummaryMissingError):
            build_compaction_replacement(
                [{"kind": "user_message", "content": "history"}]
            )


if __name__ == "__main__":
    unittest.main()
