from dataclasses import replace
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


if __name__ == "__main__":
    unittest.main()
