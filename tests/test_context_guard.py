import json
import unittest

from easy_multi_provider.capabilities import endpoint_fingerprint
from easy_multi_provider.context_guard import (
    SAFETY_RESERVE_TOKENS,
    assess_context,
    calibration_for,
    context_identity,
    estimate_input_tokens,
    format_context_error,
    is_explicit_context_error,
    update_calibration,
)


class ContextGuardTests(unittest.TestCase):
    def setUp(self):
        self.provider = {
            "id": "demo",
            "base_url": "https://example.com/v1?tenant=private",
            "protocol": "auto",
        }
        self.model = {
            "id": "demo/model",
            "upstream_id": "model",
            "context_window": 4096,
            "capability_sources": {
                "context_window": {
                    "source": "manual",
                    "confidence": 1.0,
                    "observed_at": "2026-08-21T00:00:00+00:00",
                }
            },
        }

    def test_translated_estimate_includes_tools_and_schema(self):
        base = {
            "model": "model",
            "messages": [{"role": "user", "content": "hello"}],
        }
        with_tools = dict(base)
        with_tools["tools"] = [{
            "type": "function",
            "function": {
                "name": "private_tool",
                "description": "schema-secret",
                "parameters": {"type": "object", "properties": {"value": {"type": "string"}}},
            },
        }]
        self.assertGreater(
            estimate_input_tokens(with_tools, "chat_completions"),
            estimate_input_tokens(base, "chat_completions"),
        )

    def test_image_transport_bytes_do_not_become_text_tokens(self):
        def payload(image_url):
            return {
                "model": "model",
                "input": [{
                    "type": "message",
                    "role": "user",
                    "content": [
                        {"type": "input_text", "text": "inspect this image"},
                        {"type": "input_image", "image_url": image_url},
                    ],
                }],
            }

        url_estimate = estimate_input_tokens(
            payload("https://example.com/image.png"), "responses"
        )
        base64_estimate = estimate_input_tokens(
            payload("data:image/png;base64," + "A" * (5 * 1024 * 1024)),
            "responses",
        )

        self.assertEqual(base64_estimate, url_estimate)
        self.assertLess(base64_estimate, 10_000)

    def test_effective_context_percentage_is_applied_to_native_catalog_limit(self):
        model = dict(self.model)
        model.update(
            {
                "context_window": 272_000,
                "effective_context_window_percent": 95,
                "capability_sources": {
                    "context_window": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": None,
                    }
                },
            }
        )

        assessment = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 400},
        )

        self.assertEqual(assessment.context_limit, 258_400)
        self.assertEqual(assessment.safe_input_limit, 257_744)

    def test_manual_and_advertised_provenance_and_unknown_are_explicit(self):
        manual = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertEqual(manual["source"], "manual")
        self.assertEqual(manual["confidence"], 1.0)

        advertised_model = dict(self.model)
        advertised_model["capability_sources"] = {
            "context_window": {
                "source": "advertised",
                "confidence": 0.75,
                "observed_at": "2026-08-21T00:00:00+00:00",
            }
        }
        advertised = assess_context(
            self.provider,
            advertised_model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertEqual(advertised["source"], "advertised")
        self.assertEqual(advertised["confidence"], 0.75)

        unknown_model = dict(self.model)
        unknown_model["context_window"] = 0
        unknown_model["capability_sources"] = {}
        unknown = assess_context(
            self.provider,
            unknown_model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertEqual(unknown["context_limit"], None)
        self.assertEqual(unknown["decision"], "warn")
        self.assertEqual(unknown["source"], "unknown")

    def test_high_confidence_budget_allows_and_blocks(self):
        allowed = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(allowed.decision, "allow")
        self.assertEqual(allowed.reserves, 128 + SAFETY_RESERVE_TOKENS)
        self.assertEqual(allowed.safe_input_limit, 4096 - allowed.reserves)

        blocked = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "x" * 10000, "max_output_tokens": 128},
        )
        self.assertEqual(blocked.decision, "block")
        self.assertEqual(blocked.context_decision, "blocked")

    def test_lost_state_warns_without_false_precision(self):
        assessment = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "x" * 10000, "max_output_tokens": 128},
            completeness="lost",
        )
        self.assertEqual(assessment.decision, "warn")
        self.assertLessEqual(assessment.confidence, 0.25)

    def test_calibration_identity_is_exact_and_updates_monotonically(self):
        observation = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(update_calibration(self.model, observation, "success", 1000))
        self.assertFalse(update_calibration(self.model, observation, "success", 900))
        self.assertTrue(update_calibration(self.model, observation, "success", 1200))
        self.assertTrue(update_calibration(self.model, observation, "explicit_failure", 1300))
        self.assertFalse(update_calibration(self.model, observation, "explicit_failure", 1400))
        self.assertTrue(update_calibration(self.model, observation, "explicit_failure", 1250))
        current = calibration_for(self.provider, self.model, "responses")
        self.assertEqual(current["largest_success_estimate"], 1200)
        self.assertEqual(current["smallest_failure_estimate"], 1250)
        recalibrated = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(recalibrated.context_limit, 4096)
        self.assertEqual(recalibrated.safe_input_limit, 1249)
        self.assertEqual(recalibrated.source, "observed")

        mismatched_provider = dict(self.provider, base_url="https://other.example/v1")
        self.assertIsNone(calibration_for(mismatched_provider, self.model, "responses"))
        self.assertIsNone(calibration_for(self.provider, self.model, "chat_completions"))
        self.assertEqual(
            context_identity(self.provider, self.model, "responses").endpoint_fingerprint,
            endpoint_fingerprint(self.provider["base_url"]),
        )

    def test_largest_success_is_lower_bound_only(self):
        observation = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(update_calibration(self.model, observation, "success", 1000))
        self.assertTrue(update_calibration(self.model, observation, "success", 1200))
        assessment = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(assessment.context_limit, 4096)
        self.assertEqual(assessment.safe_input_limit, 4096 - 128 - SAFETY_RESERVE_TOKENS)
        self.assertEqual(assessment.source, "manual")

    def test_largest_success_without_upper_bound_stays_unknown(self):
        model = dict(self.model, context_window=0, capability_sources={})
        observation = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(update_calibration(model, observation, "success", 1000))
        assessment = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "x" * 10000, "max_output_tokens": 128},
        )
        self.assertEqual(assessment.context_limit, None)
        self.assertEqual(assessment.safe_input_limit, None)
        self.assertEqual(assessment.decision, "warn")
        self.assertEqual(assessment.source, "unknown")

    def test_failure_boundary_is_input_limit_without_double_reserving(self):
        observation = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(
            update_calibration(self.model, observation, "explicit_failure", 1250)
        )
        assessment = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(assessment.context_limit, 4096)
        self.assertEqual(assessment.safe_input_limit, 1249)
        self.assertEqual(assessment.source, "observed")
        self.assertEqual(assessment.confidence, 1.0)

    def test_failure_only_can_supply_a_high_confidence_input_ceiling(self):
        model = dict(self.model, context_window=0, capability_sources={})
        observation = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(
            update_calibration(model, observation, "explicit_failure", 1250)
        )
        assessment = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "x" * 10000, "max_output_tokens": 128},
        )
        self.assertIsNone(assessment.context_limit)
        self.assertEqual(assessment.safe_input_limit, 1249)
        self.assertEqual(assessment.source, "observed")
        self.assertEqual(assessment.confidence, 1.0)
        self.assertEqual(assessment.decision, "block")

    def test_base_safe_input_and_failure_select_the_smaller_bound(self):
        model = dict(self.model, context_window=2000)
        observation = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(
            update_calibration(model, observation, "explicit_failure", 1800)
        )
        base_bound = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(base_bound.context_limit, 2000)
        self.assertEqual(base_bound.safe_input_limit, 2000 - 128 - SAFETY_RESERVE_TOKENS)
        self.assertEqual(base_bound.source, "manual")

        self.assertTrue(
            update_calibration(model, observation, "explicit_failure", 1200)
        )
        observed_bound = assess_context(
            self.provider,
            model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertEqual(observed_bound.safe_input_limit, 1199)
        self.assertEqual(observed_bound.source, "observed")
        self.assertEqual(observed_bound.confidence, 1.0)

    def test_identity_mismatch_ignores_success_and_failure_calibration(self):
        observation = assess_context(
            self.provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        ).to_safe_dict()
        self.assertTrue(update_calibration(self.model, observation, "success", 1000))
        self.assertTrue(
            update_calibration(self.model, observation, "explicit_failure", 1250)
        )
        mismatched_provider = dict(self.provider, base_url="https://other.example/v1")
        assessment = assess_context(
            mismatched_provider,
            self.model,
            "responses",
            {"model": "model", "input": "hello", "max_output_tokens": 128},
        )
        self.assertIsNone(calibration_for(mismatched_provider, self.model, "responses"))
        self.assertEqual(assessment.context_limit, 4096)
        self.assertEqual(assessment.safe_input_limit, 4096 - 128 - SAFETY_RESERVE_TOKENS)
        self.assertEqual(assessment.source, "manual")

    def test_context_error_classifier_requires_structured_evidence(self):
        explicit = json.dumps({
            "error": {"code": "context_length_exceeded", "message": "input too long"}
        }).encode()
        self.assertTrue(is_explicit_context_error(400, "application/json", explicit))
        self.assertTrue(is_explicit_context_error(
            400,
            "application/json",
            b'{"message":"maximum context length is 4096 tokens"}',
        ))
        self.assertFalse(is_explicit_context_error(
            400, "text/html", b"<html>context length exceeded secret-body</html>"
        ))
        self.assertFalse(is_explicit_context_error(
            403, "application/json", b'{"message":"WAF denied request"}'
        ))
        self.assertFalse(is_explicit_context_error(
            403,
            "application/json",
            b'{"error":{"code":"context_length_exceeded"}}',
        ))
        self.assertFalse(is_explicit_context_error(
            500, "application/json", b'{"message":"server context length exceeded"}'
        ))

    def test_safe_assessment_and_error_never_include_payload_content(self):
        assessment = assess_context(
            self.provider,
            self.model,
            "responses",
            {
                "model": "model",
                "input": "prompt-secret",
                "tools": [{"name": "tool-args-secret"}],
                "max_output_tokens": 128,
            },
        ).to_safe_dict()
        serialized = json.dumps(assessment)
        self.assertNotIn("prompt-secret", serialized)
        self.assertNotIn("tool-args-secret", serialized)
        message = format_context_error(assessment)
        self.assertNotIn("prompt-secret", message)
        self.assertNotIn("https://", message)
        self.assertIn("demo", message)
        self.assertIn("demo/model", message)


if __name__ == "__main__":
    unittest.main()
