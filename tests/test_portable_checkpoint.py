import unittest

from easy_multi_provider.codex_history import normalize_visible_item
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
