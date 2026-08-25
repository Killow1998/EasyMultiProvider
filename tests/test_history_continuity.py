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

    def test_native_route_change_compacts_oversized_visible_history(self):
        history = []
        for index in range(8):
            history.extend(
                [
                    VisibleItem(
                        "user_message",
                        content="requirement-%d %s" % (index, "x" * 700),
                        turn_id="turn-%d" % index,
                    ),
                    VisibleItem(
                        "assistant_message",
                        content="completed-%d %s" % (index, "y" * 700),
                        turn_id="turn-%d" % index,
                    ),
                ]
            )
        native = {
            "id": "imported-account",
            "protocol": "responses",
            "auth_mode": "account",
        }
        model = {"id": "account/model", "context_window": 5_000}
        body = {
            "model": model["id"],
            "input": [
                {"type": "compaction", "encrypted_content": HISTORY_REBUILD_MARKER},
                _message("active request"),
            ],
        }
        calls = []

        def summarize(request):
            calls.append(request.stage)
            return "%s checkpoint" % request.stage

        engine = HistoryContinuityEngine(
            Reader(history), destination_summarizer=summarize
        )
        prepared = engine.prepare(
            {"models": [model]}, native, model, model["id"], body, _headers()
        )

        self.assertIn("map", calls)
        self.assertEqual(prepared["input"][-1], _message("active request"))
        self.assertEqual(engine.last_compaction_metrics.status, "compacted")

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
        self.assertEqual(prepared["input"][-1], _message("continue exactly"))

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

    def test_oversized_history_uses_destination_map_reduce_and_keeps_tail_and_request(self):
        history = []
        for index in range(9):
            turn_id = "turn-%d" % index
            history.append(
                VisibleItem(
                    "user_message",
                    content="history-%d %s" % (index, "x" * 650),
                    turn_id=turn_id,
                )
            )
            if index == 8:
                history.extend(
                    [
                        VisibleItem(
                            "tool_call",
                            content={"name": "read_file", "arguments": "{}"},
                            turn_id=turn_id,
                            call_id="tail-call",
                            raw_type="custom_tool_call",
                        ),
                        VisibleItem(
                            "tool_result",
                            content={"output": "tail result"},
                            turn_id=turn_id,
                            call_id="tail-call",
                            raw_type="custom_tool_call_output",
                        ),
                    ]
                )
            history.append(
                VisibleItem(
                    "assistant_message",
                    content="completed-%d %s" % (index, "y" * 650),
                    turn_id=turn_id,
                )
            )
        history.append(VisibleItem("compaction_summary", content=""))

        config, provider, model = _external(context_window=5_000)
        active_request = _message("active request must remain byte-for-byte visible")
        body = _opaque_body()
        body["input"][-1] = active_request
        calls = []

        def summarize(request):
            calls.append(request)
            encoded = json.dumps(
                request.body,
                ensure_ascii=False,
                sort_keys=True,
                separators=(",", ":"),
            ).encode("utf-8")
            self.assertLessEqual(
                (len(encoded) + 1) // 2,
                request.safe_input_budget - request.output_limit,
            )
            self.assertFalse(request.body.get("stream"))
            self.assertEqual(request.body.get("tools"), [])
            self.assertNotIn("previous_response_id", request.body)
            return "%s checkpoint %d" % (request.stage, len(calls))

        engine = HistoryContinuityEngine(
            Reader(history), destination_summarizer=summarize
        )
        prepared = engine.prepare(config, provider, model, model["id"], body, _headers())

        self.assertGreaterEqual(len(calls), 3)
        self.assertIn("map", [request.stage for request in calls])
        self.assertIn("reduce", [request.stage for request in calls])
        self.assertEqual(engine.last_compaction_metrics.status, "compacted")
        self.assertGreater(engine.last_compaction_metrics.map_calls, 1)
        self.assertGreater(engine.last_compaction_metrics.reduce_calls, 0)
        self.assertNotIn("history-8", json.dumps(engine.last_compaction_metrics.to_safe_dict()))
        self.assertEqual(prepared["input"][-1], active_request)
        rendered = json.dumps(prepared["input"], ensure_ascii=False)
        self.assertIn("history-8", rendered)
        self.assertIn("tail-call", rendered)
        self.assertEqual(
            sum(
                item.get("call_id") == "tail-call"
                for item in prepared["input"]
                if isinstance(item, dict)
            ),
            2,
        )
        final_encoded = json.dumps(
            {
                key: prepared[key]
                for key in ("input", "instructions", "tools", "text", "response_format")
                if key in prepared
            },
            ensure_ascii=False,
            sort_keys=True,
            separators=(",", ":"),
        ).encode("utf-8")
        self.assertLessEqual((len(final_encoded) + 1) // 2, 4_500)

        first_call_count = len(calls)
        repeated = engine.prepare(
            config, provider, model, model["id"], body, _headers()
        )
        self.assertEqual(len(calls), first_call_count)
        self.assertTrue(engine.last_compaction_metrics.cache_hit)
        self.assertEqual(repeated["input"][-1], active_request)


if __name__ == "__main__":
    unittest.main()
