"""Cross-provider regressions from issues #8 and #9; synthetic history only."""

import copy
from contextlib import closing
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.codex_history import (
    HistoryAnchor, HistoryAmbiguousError, HistoryMismatchError, HistoryUnavailableError,
)
from easy_multi_provider.diagnostic_journal import NullJournal
from easy_multi_provider.dialects import ProjectionError, project_request
from easy_multi_provider.history_continuity import CodexHomeHistoryReader
from tests.test_codex_history import THREAD, RETRY_TURN, _rollout_text


NATIVE = {"protocol": "responses", "auth_mode": "forward"}


def external_pair(kind="function_call"):
    call = {"type": kind, "id": "call_3078153", "call_id": "call_3078153", "name": "read_file"}
    call["input" if kind == "custom_tool_call" else "arguments"] = '{"path":"fixture.txt"}'
    return [call, {"type": kind + "_output", "call_id": "call_3078153", "output": "visible result"}]


class NativeToolHistoryRegressionTests(unittest.TestCase):
    def test_external_pair_preserves_visible_context_and_pairing_without_external_item_id(self):
        for kind in ("function_call", "custom_tool_call"):
            with self.subTest(kind=kind):
                native_call = {"type": "function_call", "id": "fc_native", "call_id": "call_native", "name": "native_tool", "arguments": "{}"}
                body = {"input": [
                    {"type": "message", "role": "user", "content": "constraint"},
                    native_call,
                    {"type": "function_call_output", "call_id": "call_native", "output": "native result"},
                    *external_pair(kind),
                    {"type": "message", "role": "assistant", "content": "completed work"},
                ]}
                original = copy.deepcopy(body)
                result = project_request(NATIVE, body)
                self.assertNotIn("id", result["input"][3])
                self.assertEqual(result["input"][3]["call_id"], result["input"][4]["call_id"])
                self.assertEqual(result["input"][4]["output"], "visible result")
                self.assertEqual(result["input"][:3], body["input"][:3])
                self.assertEqual(result["input"][-1], body["input"][-1])
                self.assertEqual(body, original)
                self.assertEqual(project_request(NATIVE, result), result)

    def test_ambiguous_or_incomplete_external_pair_is_rejected_without_content(self):
        pair = external_pair()
        for items in ([pair[0]], pair[::-1], [pair[0], pair[1], pair[1]], [pair[0], pair[0], pair[1]]):
            with self.subTest(items=len(items)):
                with self.assertRaises(ProjectionError) as raised:
                    project_request(NATIVE, {"input": items})
                self.assertEqual(raised.exception.failure_class, "incompatible_tool_history")
                self.assertNotIn("fixture.txt", str(raised.exception))
                self.assertNotIn("visible result", str(raised.exception))
                self.assertNotIn("call_3078153", str(raised.exception))

    def test_native_history_is_unchanged_even_when_pair_is_in_opaque_state(self):
        body = {"input": [
            {"type": "compaction", "encrypted_content": "native-opaque"},
            {"type": "function_call_output", "call_id": "call_native", "output": "result"},
            {"type": "function_call", "id": "fc_native", "call_id": "call_pending", "name": "tool", "arguments": "{}"},
        ]}
        self.assertEqual(project_request(NATIVE, body), body)


class HistoryLocationRegressionTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name)
        with closing(sqlite3.connect(self.home / "state_5.sqlite")) as db:
            db.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT)")
            db.commit()
        self.anchor = HistoryAnchor(thread_id=THREAD, turn_id=RETRY_TURN)

    def rollout(self, *, archived=False, text=None):
        folder = self.home / ("archived_sessions" if archived else "sessions/2026/09/04")
        folder.mkdir(parents=True, exist_ok=True)
        path = folder / ("rollout-2026-09-04T12-00-00-%s.jsonl" % THREAD)
        path.write_text(text if text is not None else _rollout_text(), encoding="utf-8")
        return path

    def test_missing_index_with_valid_rollout_can_recover_exact_thread_and_turn(self):
        self.rollout()
        snapshot = CodexHomeHistoryReader(self.home).read_visible_history(self.anchor)
        self.assertEqual(snapshot.thread_id, THREAD)
        self.assertEqual(snapshot.anchor.turn_id, RETRY_TURN)
        self.assertIn("original constraint", json.dumps([item.content for item in snapshot.items]))

    def test_truly_missing_history_remains_fail_closed(self):
        with self.assertRaises(HistoryUnavailableError) as raised:
            CodexHomeHistoryReader(self.home).read_visible_history(self.anchor)
        self.assertEqual(raised.exception.reason, "thread_missing")

    def test_matching_filename_is_not_enough_if_session_metadata_disagrees(self):
        self.rollout(text=_rollout_text().replace(THREAD, "01a00000-0000-7000-8000-000000000099"))
        with self.assertRaises(HistoryMismatchError):
            CodexHomeHistoryReader(self.home).read_visible_history(self.anchor)

    def test_matching_thread_is_not_enough_if_current_turn_is_missing(self):
        self.rollout(text=_rollout_text().replace(RETRY_TURN, "01a00000-0000-7000-8000-000000000099"))
        with self.assertRaises(HistoryUnavailableError) as raised:
            CodexHomeHistoryReader(self.home).read_visible_history(self.anchor)
        self.assertEqual(raised.exception.reason, "turn_not_found")

    def test_two_rollouts_are_not_resolved_by_guessing_the_newest(self):
        self.rollout()
        self.rollout(archived=True)
        with self.assertRaises(HistoryAmbiguousError) as raised:
            CodexHomeHistoryReader(self.home).read_visible_history(self.anchor)
        self.assertEqual(raised.exception.reason, "multiple_rollout_sources")

    def test_side_chat_cannot_silently_borrow_parent_history(self):
        self.rollout()
        child = HistoryAnchor(thread_id="01a00000-0000-7000-8000-000000000088", turn_id=RETRY_TURN)
        with self.assertRaises(HistoryUnavailableError) as raised:
            CodexHomeHistoryReader(self.home).read_visible_history(child)
        self.assertEqual(raised.exception.reason, "thread_missing")

    def test_recovery_diagnostics_contain_only_source_and_pseudonyms(self):
        self.rollout()
        class Journal(NullJournal):
            def __init__(self):
                super().__init__()
                self.events = []
            def event(self, level, name, **fields):
                self.events.append({"event": name, **fields})
        journal = Journal()
        CodexHomeHistoryReader(self.home, journal=journal).read_visible_history(self.anchor)
        self.assertEqual([event["source"] for event in journal.events], ["sqlite", "rollout"])
        self.assertEqual(journal.events[-1]["result"], "found")
        self.assertTrue(journal.events[-1]["fallback"])
        serialized = json.dumps(journal.events)
        for private in (THREAD, RETRY_TURN, str(self.home), "original constraint"):
            self.assertNotIn(private, serialized)


if __name__ == "__main__":
    unittest.main()
