import base64
import json
import unittest
from unittest.mock import patch

import easy_multi_provider.router as router

from easy_multi_provider.codex_history import (
    HistoryAnchor,
    HistoryCursor,
    HistorySnapshot,
    VisibleItem,
)
from easy_multi_provider.history_continuity import (
    HistoryContinuityEngine,
    HistoryReconstructionError,
)
from easy_multi_provider.transport import sse_json_events


THREAD = "thread-v075"
TURN = "turn-current"


def _headers():
    return {
        "thread-id": THREAD,
        "x-codex-turn-metadata": json.dumps({"thread_id": THREAD, "turn_id": TURN}),
    }


def _message(text):
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def _opaque_body(text="continue exactly"):
    return {
        "model": "external/model",
        "input": [
            {"type": "compaction", "encrypted_content": "native-opaque"},
            _message(text),
        ],
        "tools": [],
    }


def _portable_body(model="external/model"):
    encoded = base64.urlsafe_b64encode(b"portable visible checkpoint").decode()
    return {
        "model": model,
        "input": [
            {"type": "compaction", "encrypted_content": "emp1:" + encoded},
            _message("continue from the checkpoint"),
        ],
        "tools": [],
    }


class Reader:
    def __init__(self, items):
        self.items = tuple(items)
        self.calls = 0
        self.anchors = []

    def read_visible_history(self, anchor):
        self.calls += 1
        self.anchors.append(anchor)
        return HistorySnapshot(
            anchor=anchor,
            items=self.items,
            cursor=HistoryCursor(kind="legacy", thread_id=THREAD),
            source="rollout",
            source_model="gpt-native",
        )


def _external(context_window=32_000):
    provider = {
        "id": "external",
        "protocol": "responses",
        "auth_mode": "api_key",
    }
    model = {
        "id": "external/model",
        "provider": "external",
        "upstream_id": "model",
        "context_window": context_window,
    }
    return {"providers": [provider], "models": [model]}, provider, model


class HistoryContinuityTests(unittest.TestCase):
    def test_native_subscription_family_keeps_native_compaction_unchanged(self):
        reader = Reader([])
        engine = HistoryContinuityEngine(reader)
        body = _opaque_body()
        native = {
            "id": "imported-account",
            "protocol": "responses",
            "auth_mode": "account",
            "account": {"id": "imported-account"},
        }

        prepared = engine.prepare({}, native, {"id": "account/model"}, "account/model", body, _headers())

        self.assertEqual(prepared, body)
        self.assertEqual(reader.calls, 0)

    def test_portable_checkpoint_to_external_is_visible_without_rollout(self):
        reader = Reader([])
        config, provider, model = _external()

        prepared = HistoryContinuityEngine(reader).prepare(
            config,
            provider,
            model,
            model["id"],
            _portable_body(),
            _headers(),
        )

        rendered = json.dumps(prepared["input"], ensure_ascii=False)
        self.assertEqual(reader.calls, 0)
        self.assertNotIn("emp1:", rendered)
        self.assertIn("portable visible checkpoint", rendered)

    def test_portable_checkpoint_to_native_becomes_visible_without_rollout(self):
        reader = Reader([])
        native = {
            "id": "imported-account",
            "protocol": "responses",
            "auth_mode": "account",
        }
        model = {"id": "account/model", "context_window": 32_000}

        prepared = HistoryContinuityEngine(reader).prepare(
            {},
            native,
            model,
            model["id"],
            _portable_body(model["id"]),
            _headers(),
        )

        rendered = json.dumps(prepared["input"], ensure_ascii=False)
        self.assertEqual(reader.calls, 0)
        self.assertNotIn("emp1:", rendered)
        self.assertIn("portable visible checkpoint", rendered)

    def test_websocket_uses_request_client_metadata_instead_of_handshake_headers(self):
        reader = Reader([VisibleItem("compaction_summary", content="checkpoint")])
        config, provider, model = _external()
        body = _opaque_body()
        body["client_metadata"] = {
            "x-codex-turn-metadata": json.dumps(
                {"thread_id": THREAD, "turn_id": TURN}
            )
        }

        prepared = HistoryContinuityEngine(reader).prepare(
            config,
            provider,
            model,
            model["id"],
            body,
            {"x-codex-turn-metadata": json.dumps({"turn_id": {"stale": True}})},
        )

        self.assertEqual(reader.calls, 1)
        self.assertIn("checkpoint", json.dumps(prepared["input"]))

    def test_websocket_current_request_window_overrides_stale_handshake_window(self):
        reader = Reader([VisibleItem("compaction_summary", content="checkpoint")])
        config, provider, model = _external()
        body = _opaque_body()
        body["client_metadata"] = {
            "x-codex-turn-metadata": json.dumps(
                {
                    "thread_id": THREAD,
                    "turn_id": TURN,
                    "window_id": "window-after-compact",
                }
            )
        }

        prepared = HistoryContinuityEngine(reader).prepare(
            config,
            provider,
            model,
            model["id"],
            body,
            {
                "thread-id": THREAD,
                "x-codex-window-id": "window-before-compact",
                "x-codex-turn-metadata": json.dumps(
                    {
                        "thread_id": THREAD,
                        "turn_id": "turn-before-compact",
                        "window_id": "window-before-compact",
                    }
                ),
            },
        )

        self.assertEqual(reader.calls, 1)
        self.assertEqual(reader.anchors[0].window_id, "window-after-compact")
        self.assertIn("checkpoint", json.dumps(prepared["input"]))

    def test_websocket_current_window_does_not_weaken_thread_identity(self):
        reader = Reader([VisibleItem("compaction_summary", content="checkpoint")])
        config, provider, model = _external()
        body = _opaque_body()
        body["client_metadata"] = {
            "x-codex-turn-metadata": json.dumps(
                {
                    "thread_id": THREAD,
                    "turn_id": TURN,
                    "window_id": "window-after-compact",
                }
            )
        }

        with self.assertRaises(HistoryReconstructionError) as raised:
            HistoryContinuityEngine(reader).prepare(
                config,
                provider,
                model,
                model["id"],
                body,
                {
                    "thread-id": "different-thread",
                    "x-codex-window-id": "window-before-compact",
                },
            )

        self.assertEqual(raised.exception.reason, "conflicting_thread_identity")
        self.assertEqual(reader.calls, 0)

    def test_native_opaque_to_external_replays_visible_history_on_first_send(self):
        reader = Reader(
            [
                VisibleItem("user_message", content="PID overshoot must stay below ten percent"),
                VisibleItem("assistant_message", content="implemented the response chart"),
                VisibleItem("compaction_summary", content=""),
            ]
        )
        config, provider, model = _external()

        prepared = HistoryContinuityEngine(reader).prepare(
            config, provider, model, model["id"], _opaque_body(), _headers()
        )

        rendered = json.dumps(prepared["input"], ensure_ascii=False)
        self.assertEqual(reader.calls, 1)
        self.assertNotIn("native-opaque", rendered)
        self.assertIn("PID overshoot", rendered)
        self.assertIn("implemented the response chart", rendered)
        self.assertIn("continue exactly", rendered)
        self.assertEqual(prepared["input"][-1], _message("continue exactly"))

    def test_previous_response_id_never_enters_history_reader(self):
        reader = Reader([VisibleItem("compaction_summary", content="checkpoint")])
        config, _, _ = _external()
        body = _opaque_body()
        body["previous_response_id"] = "resp_transport_only"

        with self.assertRaises(router.RouterError):
            router.proxy(
                config,
                body,
                _headers(),
                history_preparer=HistoryContinuityEngine(reader).prepare,
            )

        self.assertEqual(reader.calls, 0)

    def test_full_codex_input_replaces_only_opaque_compaction_without_duplicating_tail(self):
        """Codex sends its complete normalized prompt, not a current-turn delta."""

        reader = Reader(
            [
                VisibleItem("user_message", content="old requirement"),
                VisibleItem("assistant_message", content="old implementation"),
                VisibleItem("compaction_summary", content=""),
                VisibleItem("user_message", content="inspect the repository"),
                VisibleItem(
                    "tool_call",
                    content={"name": "read_file", "arguments": "{}"},
                    item_id="item-call-1",
                    call_id="call-1",
                    raw_type="custom_tool_call",
                ),
                VisibleItem(
                    "tool_result",
                    content={"output": "README contents"},
                    call_id="call-1",
                    raw_type="custom_tool_call_output",
                ),
                VisibleItem("assistant_message", content="repository inspected"),
            ]
        )
        config, provider, model = _external()
        active_tail = [
            _message("inspect the repository"),
            {
                "type": "custom_tool_call",
                "id": "item-call-1",
                "call_id": "call-1",
                "name": "read_file",
                "input": "{}",
            },
            {
                "type": "custom_tool_call_output",
                "call_id": "call-1",
                "output": "README contents",
            },
            {
                "type": "message",
                "role": "assistant",
                "content": [{"type": "output_text", "text": "repository inspected"}],
            },
            _message("continue the task"),
        ]
        body = {
            "model": model["id"],
            "input": [
                {"type": "compaction", "encrypted_content": "native-opaque"},
                *active_tail,
            ],
            "tools": [],
        }

        upstream_response = json.dumps(
            {
                "id": "resp_external",
                "object": "response",
                "status": "completed",
                "output": [],
            }
        ).encode()
        with patch.object(
            router,
            "forward_responses",
            return_value=(200, "application/json", upstream_response),
        ) as forwarded:
            metadata, result = router.proxy(
                config,
                body,
                _headers(),
                history_preparer=HistoryContinuityEngine(reader).prepare,
            )
        prepared = forwarded.call_args.args[1]

        self.assertEqual(metadata["status"], 200)
        self.assertEqual(result, upstream_response)
        self.assertNotIn("native-opaque", json.dumps(prepared["input"]))
        self.assertEqual(prepared["input"][-len(active_tail) :], active_tail)
        self.assertEqual(
            sum(
                item.get("call_id") == "call-1"
                for item in prepared["input"]
                if isinstance(item, dict)
            ),
            2,
        )

    def test_external_compaction_after_history_rebuild_returns_one_compaction_item(self):
        reader = Reader(
            [
                VisibleItem("compaction_summary", content="visible checkpoint"),
                VisibleItem("assistant_message", content="completed work"),
            ]
        )
        config, provider, model = _external()
        body = _opaque_body()
        body["stream"] = True
        body["input"].append({"type": "compaction_trigger"})
        summary_response = json.dumps(
            {
                "id": "resp_summary",
                "object": "response",
                "status": "completed",
                "output": [],
                "output_text": "portable PID checkpoint",
            }
        ).encode()

        engine = HistoryContinuityEngine(reader)
        with patch.object(
            router,
            "forward_responses",
            return_value=(200, "application/json", summary_response),
        ) as summarized:
            metadata, stream = router.proxy(
                config,
                body,
                _headers(),
                history_preparer=engine.prepare,
            )
            events = list(sse_json_events(stream))

        self.assertEqual(reader.calls, 1)
        self.assertEqual(metadata["kind"], "stream")
        done = [event for event in events if event.get("type") == "response.output_item.done"]
        self.assertEqual(len(done), 1)
        self.assertEqual(done[0]["item"]["type"], "compaction")
        summary_request = summarized.call_args.args[1]
        rendered = json.dumps(summary_request["input"], ensure_ascii=False)
        self.assertNotIn("native-opaque", rendered)
        self.assertNotIn("compaction_trigger", rendered)
        self.assertIn("visible checkpoint", rendered)

if __name__ == "__main__":
    unittest.main()
