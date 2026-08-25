import unittest

from easy_multi_provider.portable_checkpoint import (
    CompactionSummaryMissingError,
    IncompleteToolPairError,
    build_portable_view,
)


class PortableCheckpointTests(unittest.TestCase):
    def test_visible_codex_summary_replaces_pre_compaction_history(self):
        checkpoint = build_portable_view(
            [
                {"kind": "user_message", "content": "old"},
                {"kind": "compaction_summary", "content": "visible summary"},
                {"kind": "assistant_message", "content": "retained tail"},
            ],
            [{"kind": "user_message", "content": "continue"}],
        )

        self.assertEqual(checkpoint.summary["content"], "visible summary")
        self.assertEqual([item["content"] for item in checkpoint.retained_tail], ["retained tail"])
        self.assertEqual(checkpoint.current_request[0]["content"], "continue")

    def test_opaque_remote_compaction_replays_full_visible_history(self):
        checkpoint = build_portable_view(
            [
                {"kind": "user_message", "content": "constraint"},
                {"kind": "assistant_message", "content": "completed work"},
                {"kind": "compaction_summary", "content": ""},
            ],
            [{"kind": "user_message", "content": "continue"}],
        )

        self.assertIsNone(checkpoint.summary)
        self.assertEqual(
            [item["content"] for item in checkpoint.retained_tail],
            ["constraint", "completed work"],
        )

    def test_incomplete_tool_pair_fails_closed(self):
        with self.assertRaises(IncompleteToolPairError):
            build_portable_view(
                [
                    {"kind": "compaction_summary", "content": "summary"},
                    {"kind": "tool_call", "call_id": "call-1", "content": {}},
                ],
                [{"kind": "user_message", "content": "continue"}],
            )

    def test_missing_compaction_boundary_fails_closed(self):
        with self.assertRaises(CompactionSummaryMissingError):
            build_portable_view(
                [{"kind": "user_message", "content": "history"}],
                [{"kind": "user_message", "content": "continue"}],
            )


if __name__ == "__main__":
    unittest.main()
