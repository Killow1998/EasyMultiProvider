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
    HISTORY_REBUILD_MARKER,
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


class Reader:
    def __init__(self, items):
        self.items = tuple(items)
        self.calls = 0

    def read_visible_history(self, anchor):
        self.calls += 1
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

    def test_websocket_route_change_rebuilds_even_for_native_destination(self):
        reader = Reader(
            [
                VisibleItem("compaction_summary", content="visible checkpoint"),
                VisibleItem("assistant_message", content="completed work"),
            ]
        )
        native = {
            "id": "imported-account",
            "protocol": "responses",
            "auth_mode": "account",
        }
        model = {
            "id": "account/model",
            "context_window": 32_000,
        }
        body = {
            "model": model["id"],
            "input": [
                {
                    "type": "compaction",
                    "encrypted_content": HISTORY_REBUILD_MARKER,
                },
                _message("continue exactly"),
            ],
        }

        prepared = HistoryContinuityEngine(reader).prepare(
            {"models": [model]}, native, model, model["id"], body, _headers()
        )

        rendered = json.dumps(prepared["input"], ensure_ascii=False)
        self.assertEqual(reader.calls, 1)
        self.assertNotIn(HISTORY_REBUILD_MARKER, rendered)
        self.assertIn("visible checkpoint", rendered)
        self.assertIn("continue exactly", rendered)

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
        self.assertIn("Cross-model handoff boundary", json.dumps(prepared["input"][-2]))
        self.assertEqual(prepared["input"][-1], _message("continue exactly"))

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

    def test_context_window_without_output_limit_uses_explicit_safety_margin(self):
        reader = Reader(
            [
                VisibleItem("compaction_summary", content="visible checkpoint"),
                VisibleItem("assistant_message", content="tail"),
            ]
        )
        config, provider, model = _external(context_window=8_000)

        prepared = HistoryContinuityEngine(reader).prepare(
            config, provider, model, model["id"], _opaque_body(), _headers()
        )

        self.assertIn("visible checkpoint", json.dumps(prepared["input"]))

    def test_missing_context_capability_fails_closed(self):
        reader = Reader([VisibleItem("compaction_summary", content="checkpoint")])
        config, provider, model = _external()
        model.pop("context_window")

        with self.assertRaises(HistoryReconstructionError) as raised:
            HistoryContinuityEngine(reader).prepare(
                config, provider, model, model["id"], _opaque_body(), _headers()
            )

        self.assertEqual(raised.exception.reason, "context_budget_unknown")

    def test_oversized_lossless_projection_fails_instead_of_clipping(self):
        reader = Reader(
            [
                VisibleItem("user_message", content="x" * 20_000),
                VisibleItem("compaction_summary", content=""),
            ]
        )
        config, provider, model = _external(context_window=1_000)

        with self.assertRaises(HistoryReconstructionError) as raised:
            HistoryContinuityEngine(reader).prepare(
                config, provider, model, model["id"], _opaque_body(), _headers()
            )

        self.assertEqual(raised.exception.reason, "context_budget_exceeded")


if __name__ == "__main__":
    unittest.main()
