import json
import sqlite3
import tempfile
import unittest
from pathlib import Path

from easy_multi_provider.codex_history import (
    AppServerReader,
    HistoryAnchor,
    HistoryCorruptError,
    HistoryMismatchError,
    HistoryUnavailableError,
    RolloutReader,
    SQLiteReader,
)


THREAD = "01a00000-0000-7000-8000-000000000001"
NATIVE_TURN = "01a00000-0000-7000-8000-000000000002"
COMPACT_TURN = "01a00000-0000-7000-8000-000000000003"
FAILED_TURN = "01a00000-0000-7000-8000-000000000004"
RETRY_TURN = "01a00000-0000-7000-8000-000000000005"


def _record(record_type, payload):
    return json.dumps({"type": record_type, "payload": payload}) + "\n"


def _rollout_text():
    records = [
        _record("session_meta", {"id": THREAD, "history_mode": "legacy"}),
        _record("event_msg", {"type": "task_started", "turn_id": NATIVE_TURN}),
        _record("turn_context", {"turn_id": NATIVE_TURN, "model": "gpt-native"}),
        _record(
            "response_item",
            {"type": "message", "role": "user", "content": "original constraint"},
        ),
        _record(
            "response_item",
            {"type": "message", "role": "assistant", "content": "completed work"},
        ),
        _record("event_msg", {"type": "task_complete", "turn_id": NATIVE_TURN}),
        _record("event_msg", {"type": "task_started", "turn_id": COMPACT_TURN}),
        _record(
            "compacted",
            {
                "message": "",
                "replacement_history": [
                    {
                        "type": "message",
                        "role": "user",
                        "content": "original constraint",
                    },
                    {
                        "type": "message",
                        "role": "assistant",
                        "content": "completed work",
                    },
                ],
                "window_id": "window-1",
            },
        ),
        _record("event_msg", {"type": "task_complete", "turn_id": COMPACT_TURN}),
        _record("event_msg", {"type": "task_started", "turn_id": FAILED_TURN}),
        _record("turn_context", {"turn_id": FAILED_TURN, "model": "external/model"}),
        _record(
            "response_item",
            {"type": "message", "role": "user", "content": "failed attempt marker"},
        ),
        _record(
            "event_msg",
            {"type": "task_complete", "turn_id": FAILED_TURN, "error": {"code": "failed"}},
        ),
        _record("event_msg", {"type": "task_started", "turn_id": RETRY_TURN}),
        _record("turn_context", {"turn_id": RETRY_TURN, "model": "external/model"}),
    ]
    return "".join(records)


class CodexHistoryReaderTests(unittest.TestCase):
    def test_history_anchor_carries_window_identity_and_rejects_conflicts(self):
        anchor = HistoryAnchor.from_headers(
            {
                "thread-id": THREAD,
                "x-codex-window-id": "window-7",
                "x-codex-turn-metadata": json.dumps(
                    {
                        "thread_id": THREAD,
                        "turn_id": NATIVE_TURN,
                        "window_id": "window-7",
                    }
                ),
            }
        )

        self.assertEqual(anchor.window_id, "window-7")
        with self.assertRaises(HistoryMismatchError):
            HistoryAnchor.from_headers(
                {
                    "x-codex-window-id": "window-a",
                    "x-codex-turn-metadata": json.dumps(
                        {"window_id": "window-b"}
                    ),
                }
            )

    def test_failed_destination_turn_does_not_poison_retry_source_or_history(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(_rollout_text())
            snapshot = RolloutReader(path).read_visible_history(
                HistoryAnchor(THREAD, RETRY_TURN)
            )

        self.assertEqual(snapshot.source_model, "gpt-native")
        visible = json.dumps([item.content for item in snapshot.items])
        self.assertIn("original constraint", visible)
        self.assertIn("completed work", visible)
        self.assertNotIn("failed attempt marker", visible)
        self.assertEqual(snapshot.items[-1].kind, "compaction_marker")

    def test_remote_compaction_v2_uses_replacement_base_plus_tail(self):
        tail_turn = "01a00000-0000-7000-8000-000000000006"
        incoming_turn = "01a00000-0000-7000-8000-000000000007"
        records = [
            _record("session_meta", {"id": THREAD, "history_mode": "legacy"}),
            _record("event_msg", {"type": "task_started", "turn_id": NATIVE_TURN}),
            _record(
                "response_item",
                {"type": "message", "role": "user", "content": "superseded base"},
            ),
            _record("event_msg", {"type": "task_complete", "turn_id": NATIVE_TURN}),
            _record("event_msg", {"type": "task_started", "turn_id": COMPACT_TURN}),
            _record(
                "compacted",
                {
                    "message": "",
                    "replacement_history": [
                        {
                            "type": "message",
                            "role": "user",
                            "content": "replacement base",
                        }
                    ],
                    "window_id": "window-v2",
                },
            ),
            _record("event_msg", {"type": "task_complete", "turn_id": COMPACT_TURN}),
            _record("event_msg", {"type": "task_started", "turn_id": tail_turn}),
            _record(
                "response_item",
                {"type": "message", "role": "assistant", "content": "post checkpoint tail"},
            ),
            _record("event_msg", {"type": "task_complete", "turn_id": tail_turn}),
            _record("event_msg", {"type": "task_started", "turn_id": incoming_turn}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("".join(records))
            snapshot = RolloutReader(path).read_visible_history(
                HistoryAnchor(THREAD, incoming_turn, "window-v2")
            )

        visible = json.dumps([item.content for item in snapshot.items])
        self.assertNotIn("superseded base", visible)
        self.assertIn("replacement base", visible)
        self.assertIn("post checkpoint tail", visible)
        self.assertEqual(snapshot.items[1].kind, "compaction_marker")

    def test_remote_replacement_opaque_item_uses_prior_visible_base(self):
        incoming_turn = "01a00000-0000-7000-8000-000000000008"
        records = [
            _record("session_meta", {"id": THREAD, "history_mode": "legacy"}),
            _record("event_msg", {"type": "task_started", "turn_id": NATIVE_TURN}),
            _record(
                "response_item",
                {"type": "message", "role": "user", "content": "visible before opaque"},
            ),
            _record("event_msg", {"type": "task_complete", "turn_id": NATIVE_TURN}),
            _record("event_msg", {"type": "task_started", "turn_id": COMPACT_TURN}),
            _record(
                "compacted",
                {
                    "message": "",
                    "replacement_history": [
                        {"type": "compaction", "encrypted_content": "native-opaque"},
                        {
                            "type": "message",
                            "role": "assistant",
                            "content": "replacement tail",
                        },
                    ],
                },
            ),
            _record("event_msg", {"type": "task_complete", "turn_id": COMPACT_TURN}),
            _record("event_msg", {"type": "task_started", "turn_id": incoming_turn}),
        ]
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text("".join(records))
            snapshot = RolloutReader(path).read_visible_history(
                HistoryAnchor(THREAD, incoming_turn)
            )

        visible = json.dumps([item.content for item in snapshot.items])
        self.assertIn("visible before opaque", visible)
        self.assertIn("replacement tail", visible)
        self.assertNotIn("native-opaque", visible)
        self.assertEqual(snapshot.items[-1].kind, "compaction_marker")

    def test_reader_stops_before_first_record_of_incoming_turn(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(_rollout_text())
            snapshot = RolloutReader(path).read_visible_history(
                HistoryAnchor(THREAD, FAILED_TURN)
            )

        self.assertEqual(snapshot.source_model, "gpt-native")
        self.assertLess(snapshot.cursor.line_index, 9)

    def test_ephemeral_incoming_turn_uses_only_a_settled_rollout_prefix(self):
        settled = "".join(_rollout_text().splitlines(keepends=True)[:-2])
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(settled)
            snapshot = RolloutReader(path).read_visible_history(
                HistoryAnchor(THREAD, "ephemeral-turn-not-written")
            )

        self.assertEqual(snapshot.source_model, "gpt-native")
        self.assertNotIn(
            "failed attempt marker",
            json.dumps([item.content for item in snapshot.items]),
        )

    def test_missing_anchor_with_an_unsettled_turn_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(_rollout_text())
            with self.assertRaises(HistoryUnavailableError) as raised:
                RolloutReader(path).read_visible_history(
                    HistoryAnchor(THREAD, "different-unwritten-turn")
                )

        self.assertEqual(raised.exception.reason, "turn_not_found")

    def test_app_server_uses_only_completed_turns_before_incoming_turn(self):
        response = {
            "thread": {
                "id": THREAD,
                "turns": [
                    {
                        "id": NATIVE_TURN,
                        "status": "completed",
                        "itemsView": "full",
                        "items": [
                            {"type": "turn_context", "payload": {"model": "gpt-native"}},
                            {"type": "message", "role": "user", "content": "constraint"},
                        ],
                    },
                    {
                        "id": COMPACT_TURN,
                        "status": "completed",
                        "itemsView": "full",
                        "items": [{"type": "compacted", "message": "visible summary"}],
                    },
                    {
                        "id": FAILED_TURN,
                        "status": "failed",
                        "error": {"message": "failed"},
                        "itemsView": "full",
                        "items": [
                            {"type": "turn_context", "payload": {"model": "external/model"}},
                            {"type": "message", "role": "user", "content": "failed marker"},
                        ],
                    },
                    {
                        "id": RETRY_TURN,
                        "status": "inProgress",
                        "itemsView": "full",
                        "items": [],
                    },
                ],
            }
        }
        snapshot = AppServerReader(lambda method, params: response).read_visible_history(
            HistoryAnchor(THREAD, RETRY_TURN)
        )

        self.assertEqual(snapshot.source_model, "gpt-native")
        visible = json.dumps([item.content for item in snapshot.items])
        self.assertIn("constraint", visible)
        self.assertIn("visible summary", visible)
        self.assertNotIn("failed marker", visible)

    def test_sqlite_locator_and_rollout_reader_are_read_only(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            rollout = root / "rollout.jsonl"
            rollout.write_text(_rollout_text())
            database = root / "state_5.sqlite"
            connection = sqlite3.connect(str(database))
            connection.execute(
                "CREATE TABLE threads (id TEXT PRIMARY KEY, rollout_path TEXT, history_mode TEXT, model TEXT)"
            )
            connection.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?)",
                (THREAD, str(rollout), "legacy", "external/model"),
            )
            connection.commit()
            connection.close()
            before_db = database.read_bytes()
            before_rollout = rollout.read_bytes()

            snapshot = SQLiteReader(database).read_visible_history(
                HistoryAnchor(THREAD, FAILED_TURN)
            )

            self.assertEqual(snapshot.source_model, "gpt-native")
            self.assertEqual(database.read_bytes(), before_db)
            self.assertEqual(rollout.read_bytes(), before_rollout)

    def test_interior_json_corruption_fails_closed(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "rollout.jsonl"
            path.write_text(_record("session_meta", {"id": THREAD}) + "{broken}\n")
            with self.assertRaises(HistoryCorruptError):
                RolloutReader(path).read_visible_history(HistoryAnchor(THREAD))


if __name__ == "__main__":
    unittest.main()
