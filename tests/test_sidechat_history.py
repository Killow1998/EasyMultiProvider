"""Ephemeral Codex fork history: exact checkpoint, never the parent's latest tail."""
import copy
from contextlib import closing
import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.codex_history import HistoryAnchor
from easy_multi_provider.history_continuity import CodexHomeHistoryReader, HistoryContinuityEngine
from easy_multi_provider.router_errors import HistoryReconstructionError

PARENT = "01a00000-0000-7000-8000-000000000001"
CHILD = "01a00000-0000-7000-8000-000000000002"


def record(kind, **payload):
    return {"type": kind, "payload": payload}


class SidechatHistoryTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.home = Path(self.temp.name).resolve()
        self.path = self.home / "parent.jsonl"
        with closing(sqlite3.connect(self.home / "state_5.sqlite")) as db:
            db.execute("CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT)")
            db.execute("INSERT INTO threads VALUES (?, ?)", (PARENT, str(self.path)))
            db.commit()
        self.records = [
            record("session_meta", id=PARENT, history_mode="legacy"),
            record("event_msg", type="task_started", turn_id="before"),
            record("response_item", type="message", role="user", content="parent reference"),
            record("event_msg", type="task_complete", turn_id="before"),
            record("event_msg", type="task_started", turn_id="compact"),
            record("compacted", message="", replacement_history=[
                {"type": "compaction", "encrypted_content": "inherited-checkpoint"}]),
            record("event_msg", type="task_complete", turn_id="compact"),
            record("event_msg", type="task_started", turn_id="later"),
            record("response_item", type="message", role="user", content="DO NOT LEAK LATER PARENT"),
            record("compacted", message="", replacement_history=[
                {"type": "compaction", "encrypted_content": "newer-checkpoint"}]),
        ]
        self.write()
        self.reader = CodexHomeHistoryReader(self.home)
        self.body = {"input": [
            {"type": "compaction", "encrypted_content": "inherited-checkpoint"},
            {"type": "message", "role": "user", "content": "Side conversation boundary: reference only"},
            {"type": "message", "role": "user", "content": "side question"},
        ], "client_metadata": {"x-codex-turn-metadata": json.dumps({
            "thread_id": CHILD, "turn_id": "child-turn", "forked_from_thread_id": PARENT})}}

    def write(self):
        self.path.write_text("".join(json.dumps(r) + "\n" for r in self.records), encoding="utf-8")

    def prepare(self):
        return HistoryContinuityEngine(self.reader).prepare(
            {}, {"protocol": "responses", "auth_mode": "api_key"}, {}, "external", self.body, {})

    def test_ephemeral_fork_reuses_exact_checkpoint_and_preserves_side_boundary(self):
        original = copy.deepcopy(self.body)
        result = self.prepare()
        self.assertIn("parent reference", json.dumps(result["input"]))
        self.assertNotIn("DO NOT LEAK", json.dumps(result["input"]))
        self.assertNotIn("inherited-checkpoint", json.dumps(result["input"]))
        self.assertEqual(result["input"][-2:], original["input"][-2:])
        self.assertEqual(self.body, original)
        self.assertEqual(result["_emp_active_input_start"], len(result["input"]) - 2)

    def test_checkpoint_committed_in_active_parent_turn_is_readable(self):
        self.records = self.records[:6]
        self.write()
        self.assertIn("parent reference", json.dumps(self.prepare()["input"]))

    def test_missing_parent_metadata_never_guesses_other_threads(self):
        self.body["client_metadata"]["x-codex-turn-metadata"] = json.dumps({"thread_id": CHILD, "turn_id": "child-turn"})
        with self.assertRaises(HistoryReconstructionError) as caught:
            self.prepare()
        self.assertEqual(caught.exception.reason, "thread_missing")

    def test_unknown_or_duplicate_checkpoint_is_rejected(self):
        for duplicate in (False, True):
            with self.subTest(duplicate=duplicate):
                if duplicate:
                    self.records.append(copy.deepcopy(self.records[5]))
                    self.write()
                    self.body["input"][0]["encrypted_content"] = "inherited-checkpoint"
                else:
                    self.body["input"][0]["encrypted_content"] = "absent"
                with self.assertRaises(HistoryReconstructionError):
                    self.prepare()

    def test_parent_outside_configured_home_is_rejected(self):
        with closing(sqlite3.connect(self.home / "state_5.sqlite")) as db:
            db.execute("UPDATE threads SET rollout_path=?", (str(self.home.parent / "outside.jsonl"),))
            db.commit()
        with self.assertRaises(HistoryReconstructionError) as caught:
            self.prepare()
        self.assertEqual(caught.exception.reason, "rollout_outside_codex_home")

    def test_malformed_or_self_parent_is_rejected(self):
        for parent in (CHILD, "not-a-uuid", 12):
            with self.subTest(parent=parent):
                self.body["client_metadata"]["x-codex-turn-metadata"] = json.dumps({
                    "thread_id": CHILD, "turn_id": "child-turn", "forked_from_thread_id": parent})
                with self.assertRaises(HistoryReconstructionError):
                    self.prepare()

    def test_native_opaque_fork_is_not_reconstructed(self):
        result = HistoryContinuityEngine(self.reader).prepare(
            {}, {"protocol": "responses", "auth_mode": "forward"}, {}, "native", self.body, {})
        self.assertEqual(result, self.body)

    def test_header_metadata_also_retains_fork_identity(self):
        anchor = HistoryAnchor.from_headers({"x-codex-turn-metadata": self.body["client_metadata"]["x-codex-turn-metadata"]})
        self.assertEqual(anchor.forked_from_thread_id, PARENT)
