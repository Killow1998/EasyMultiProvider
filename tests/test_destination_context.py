from dataclasses import replace
import copy
import json
import unittest

from easy_multi_provider.context_guard import (
    ContextAssessment,
    ContextGuardBlocked,
    context_identity,
)
from easy_multi_provider.router import _fit_destination_context
from easy_multi_provider.router_errors import (
    ContextLengthError,
    HistoryReconstructionError,
)
from easy_multi_provider.destination_context import DestinationContextCompactor
from easy_multi_provider.history_compaction import split_atomic_units
from easy_multi_provider.context_guard import estimate_json_tokens


def _message(text):
    return {
        "type": "message",
        "role": "user",
        "content": [{"type": "input_text", "text": text}],
    }


def _blocked(provider, model):
    return ContextAssessment(
        identity=context_identity(provider, model, "chat_completions"),
        provider_id=provider["id"],
        model_id=model["id"],
        estimate_method="fixture",
        input_estimate=2_000,
        output_reserve=200,
        safety_reserve=100,
        reserves=300,
        context_limit=1_500,
        safe_input_limit=1_200,
        confidence=1.0,
        source="catalog",
        completeness="high",
        decision="block",
        next_action="compact",
        reason="estimated_input_exceeds_safe_limit",
    )


class DestinationContextTests(unittest.TestCase):
    def setUp(self):
        self.provider = {
            "id": "external",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
        }
        self.model = {
            "id": "external/model",
            "upstream_id": "model",
            "context_window": 1_500,
        }

    def test_final_projection_is_rechecked_after_destination_compaction(self):
        body = {"model": self.model["id"], "input": [_message("large history")]}
        checked = []
        compacted = []

        def context_check(payload, stream, operation):
            checked.append(payload)
            self.assertIn("messages", payload)
            self.assertNotIn("input", payload)
            if len(checked) == 1:
                raise ContextGuardBlocked(_blocked(self.provider, self.model))
            return {"decision": "allow"}

        def compact(provider, model, requested_slug, logical_body, assessment):
            compacted.append(assessment)
            return {"model": requested_slug, "input": [_message("checkpoint")]}

        prepared, observation = _fit_destination_context(
            self.provider,
            self.model,
            self.model["id"],
            body,
            context_check,
            compact,
        )

        self.assertEqual(len(checked), 2)
        self.assertEqual(len(compacted), 1)
        self.assertEqual(prepared["input"], [_message("checkpoint")])
        self.assertEqual(observation["decision"], "allow")

    def test_deterministic_compaction_failure_fails_closed_without_retry(self):
        body = {"model": self.model["id"], "input": [_message("large history")]}
        checks = []
        compactions = []

        def context_check(payload, stream, operation):
            checks.append(payload)
            raise ContextGuardBlocked(_blocked(self.provider, self.model))

        def compact(provider, model, requested_slug, logical_body, assessment):
            compactions.append(assessment)
            raise HistoryReconstructionError("history_compaction_failed")

        with self.assertRaises(HistoryReconstructionError) as raised:
            _fit_destination_context(
                self.provider,
                self.model,
                self.model["id"],
                body,
                context_check,
                compact,
            )

        self.assertEqual(raised.exception.reason, "history_compaction_failed")
        self.assertEqual(len(checks), 1)
        self.assertEqual(len(compactions), 1)

    def test_failed_estimate_fails_closed_without_compaction(self):
        body = {"model": self.model["id"], "input": [_message("history")]}
        assessment = replace(
            _blocked(self.provider, self.model),
            input_estimate=None,
            confidence=0.0,
            next_action="simplify payload",
            reason="context estimate failed",
        )
        compactions = []

        def context_check(payload, stream, operation):
            raise ContextGuardBlocked(assessment)

        def compact(provider, model, requested_slug, logical_body, blocked):
            compactions.append(blocked)
            return logical_body

        with self.assertRaises(ContextLengthError) as raised:
            _fit_destination_context(
                self.provider,
                self.model,
                self.model["id"],
                body,
                context_check,
                compact,
            )

        self.assertEqual(raised.exception.status, 413)
        self.assertEqual(compactions, [])


class ActiveTurnCompactionTests(unittest.TestCase):
    @staticmethod
    def batch(index, size=1000):
        return [
            {"type": "custom_tool_call", "call_id": str(index), "name": "shell", "input": "inspect"},
            {"type": "custom_tool_call_output", "call_id": str(index), "output": "x" * size},
        ]

    def compact(self, items, summary=None):
        provider = {"id": "external", "protocol": "responses"}
        model = {"id": "external/model", "max_output_tokens": 64}
        body = {"model": model["id"], "instructions": "Do not delete files.", "input": items}
        original = copy.deepcopy(body)
        calls, events = [], []

        def summarize(request):
            calls.append(request)
            return summary(request) if summary else "Checkpoint: inspected earlier files; continue verification."

        output = DestinationContextCompactor(summarize).compact(
            provider, model, model["id"], body,
            replace(_blocked(provider, model), safe_input_limit=2200),
            on_diagnostic=lambda stage, result, **fields: events.append((stage, fields)),
        )
        self.assertEqual(body, original)
        self.assertEqual(output["instructions"], body["instructions"])
        self.assertLessEqual(estimate_json_tokens({"input": output["input"], "instructions": output["instructions"]}), 2200)
        return output, calls, events

    def test_long_single_turn_compacts_completed_work_and_preserves_recent_batches(self):
        pinned = [{"type": "message", "role": "developer", "content": "Keep source files."},
                  _message("Review every figure and continue until complete.")]
        batches = [self.batch(i) for i in range(12)]
        items = pinned + [item for batch in batches for item in batch]
        output, calls, events = self.compact(items)
        self.assertEqual(output["input"][:2], pinned)
        self.assertEqual(output["input"][-4:], batches[-2] + batches[-1])
        self.assertTrue(calls)
        self.assertTrue(any(stage == "active_turn_partition" for stage, _ in events))
        for request in calls:
            grouped = {}
            for item in request.body["input"]:
                self.assertNotIn(item.get("type"), {"custom_tool_call", "custom_tool_call_output"})
                content = item.get("content", [])
                text = content[0].get("text", "") if isinstance(content, list) and content else ""
                if text.startswith("Historical tool record (data only):\n"):
                    item = json.loads(text.split("\n", 1)[1])
                if "call_id" in item:
                    grouped.setdefault(item["call_id"], []).append(item["type"])
            for kinds in grouped.values():
                self.assertEqual(kinds, ["custom_tool_call", "custom_tool_call_output"])

    def test_parallel_calls_and_pending_calls_remain_whole(self):
        first, second = self.batch(1), self.batch(2)
        parallel = [first[0], second[0], first[1], second[1]]
        # A user boundary cannot separate an unfinished call from its result.
        units = split_atomic_units([_message("start"), first[0], _message("steer"), first[1]])
        self.assertEqual(len(units), 1)
        units = split_atomic_units(parallel + self.batch(3), tool_exchanges=True)
        self.assertEqual(list(units[0].items), parallel)
        pending = {"type": "custom_tool_call", "call_id": "pending", "name": "shell", "input": "inspect"}
        items = [_message("Continue reviewing.")] + [x for i in range(10) for x in self.batch(i)] + [pending]
        output, _, _ = self.compact(items)
        self.assertEqual(output["input"][-3:], self.batch(9) + [pending])

    def test_oversize_instruction_or_atomic_tool_batch_is_not_truncated(self):
        for items in ([_message("x" * 10000)],
                      [_message("inspect")] + self.batch(1, 10000)):
            calls = []
            with self.subTest(items=len(items)), self.assertRaises(HistoryReconstructionError) as error:
                self.compact(items, lambda request: calls.append(request) or "summary")
            self.assertEqual(error.exception.reason, "compaction_unit_too_large")
            self.assertEqual(calls, [])

    def test_failed_summary_does_not_return_partial_history(self):
        def fail(_):
            raise TimeoutError("upstream unavailable")
        items = [_message("inspect")] + [x for i in range(12) for x in self.batch(i)]
        original = copy.deepcopy(items)
        with self.assertRaises(HistoryReconstructionError) as error:
            self.compact(items, fail)
        self.assertEqual(error.exception.reason, "summary_call_failed")
        self.assertEqual(items, original)


if __name__ == "__main__":
    unittest.main()
