import json
import unittest

from easy_multi_provider.capabilities import (
    CAPABILITY_NAMES,
    capability_record,
    endpoint_fingerprint,
    normalize_output_modalities,
    normalize_supported_protocols,
    output_modalities_known,
    safe_capability_list,
    supported_protocols_known,
)


class CapabilityTests(unittest.TestCase):
    def test_endpoint_fingerprint_is_canonical_and_record_is_secret_free(self):
        first = endpoint_fingerprint(
            "https://user:api-secret@example.com/v1/?api_key=query-secret"
        )
        second = endpoint_fingerprint("https://example.com/v1")
        self.assertEqual(first, second)
        self.assertRegex(first, r"^sha256:[0-9a-f]{64}$")

        record = capability_record(
            {
                "id": "demo",
                "base_url": "https://user:api-secret@example.com/v1?token=query-secret",
                "protocol": "responses",
                "api_key": "provider-secret",
            },
            {"id": "demo/model", "upstream_id": "model"},
        ).to_dict()
        serialized = json.dumps(record)
        for value in (
            "https://",
            "api-secret",
            "query-secret",
            "provider-secret",
            "userinfo",
        ):
            self.assertNotIn(value, serialized)
        self.assertNotIn("base_url", record)
        self.assertEqual(set(record["key"]), {
            "endpoint_fingerprint",
            "upstream_model",
            "protocol_identity",
            "deployment_identity",
        })

    def test_unknown_capabilities_are_explicit_and_never_true(self):
        record = capability_record(
            {
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "auto",
            },
            {"id": "demo/model", "upstream_id": "model"},
        ).to_dict()
        for name in CAPABILITY_NAMES:
            value = record["capabilities"][name]
            self.assertEqual(set(value), {"value", "source", "confidence", "observed_at"})
            self.assertIn(
                value["source"],
                {"official", "advertised", "observed", "manual", "inferred", "unknown"},
            )
        self.assertEqual(record["capabilities"]["effective_protocol"]["value"], "unknown")
        for name in (
            "streaming",
            "structured_tools",
            "parallel_tools",
            "websocket",
            "structured_output",
            "web_search",
        ):
            self.assertNotEqual(record["capabilities"][name]["value"], True)
            self.assertEqual(record["capabilities"][name]["value"], "unknown")

    def test_unknown_provenance_does_not_promote_a_configured_true(self):
        record = capability_record(
            {
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "responses",
            },
            {
                "id": "demo/model",
                "upstream_id": "model",
                "streaming": True,
                "capability_sources": {
                    "streaming": {
                        "source": "unknown",
                        "confidence": 0,
                        "observed_at": None,
                    }
                },
            },
        ).to_dict()
        self.assertEqual(record["capabilities"]["streaming"]["value"], "unknown")
        self.assertEqual(record["capabilities"]["streaming"]["source"], "unknown")

    def test_manual_and_advertised_provenance_is_preserved(self):
        record = capability_record(
            {
                "id": "demo",
                "base_url": "https://example.com/v1",
                "protocol": "responses",
            },
            {
                "id": "demo/model",
                "upstream_id": "model",
                "reasoning_levels": ["low", "high"],
                "context_window": 128000,
                "output_limit": 4096,
                "capability_sources": {
                    "reasoning_levels": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-21T00:00:00+00:00",
                    },
                    "context_window": {
                        "source": "manual",
                        "confidence": 1,
                        "observed_at": "2026-08-21T00:01:00+00:00",
                    },
                },
            },
        ).to_dict()
        self.assertEqual(
            record["capabilities"]["reasoning_levels"]["source"], "advertised"
        )
        self.assertEqual(record["capabilities"]["context_window"]["source"], "manual")
        self.assertEqual(
            record["capabilities"]["output_limit"]["source"], "inferred"
        )

    def test_safe_list_only_contains_enabled_provider_models(self):
        records = safe_capability_list(
            {
                "providers": [
                    {"id": "enabled", "base_url": "https://example.com/v1", "enabled": True},
                    {"id": "disabled", "base_url": "https://example.org/v1", "enabled": False},
                ],
                "models": [
                    {"id": "enabled/model", "provider": "enabled", "upstream_id": "model"},
                    {"id": "disabled/model", "provider": "disabled", "upstream_id": "model"},
                ],
            }
        )
        self.assertEqual([item["model_id"] for item in records], ["enabled/model"])


class TestOutputModalitiesNormalization(unittest.TestCase):
    def test_valid_modalities_are_normalized(self):
        result = normalize_output_modalities(["Text", "AUDIO", "text", "video"])
        self.assertEqual(result, ["text", "audio", "video"])

    def test_empty_returns_default_text(self):
        self.assertEqual(normalize_output_modalities([]), ["text"])
        self.assertEqual(normalize_output_modalities(None), ["text"])

    def test_non_codex_modalities_preserved(self):
        result = normalize_output_modalities(["text", "audio", "file", "pdf"])
        self.assertEqual(result, ["text", "audio", "file", "pdf"])

    def test_known_check(self):
        self.assertTrue(output_modalities_known(["text", "audio"]))
        self.assertFalse(output_modalities_known(None))
        self.assertFalse(output_modalities_known([]))
        self.assertFalse(output_modalities_known("text"))


class TestSupportedProtocolsNormalization(unittest.TestCase):
    def test_valid_protocols_retained(self):
        result = normalize_supported_protocols(["responses", "chat_completions"])
        self.assertEqual(result, ["responses", "chat_completions"])

    def test_auto_is_excluded(self):
        result = normalize_supported_protocols(["auto", "responses"])
        self.assertEqual(result, ["responses"])

    def test_unknown_protocols_excluded(self):
        result = normalize_supported_protocols(["responses", "weird_protocol"])
        self.assertEqual(result, ["responses"])

    def test_empty_returns_empty(self):
        self.assertEqual(normalize_supported_protocols([]), [])
        self.assertEqual(normalize_supported_protocols(None), [])

    def test_known_check(self):
        self.assertTrue(supported_protocols_known(["responses"]))
        self.assertFalse(supported_protocols_known([]))
        self.assertFalse(supported_protocols_known(None))


class TestNewCapabilityRecordFields(unittest.TestCase):
    provider = {
        "id": "demo",
        "base_url": "https://example.com/v1",
        "protocol": "responses",
    }

    def test_output_modalities_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "output_modalities": ["text", "audio"],
                "capability_sources": {
                    "output_modalities": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["output_modalities"]
        self.assertEqual(cap["value"], ["text", "audio"])
        self.assertEqual(cap["source"], "advertised")

    def test_output_modalities_defaults_to_unknown(self):
        record = capability_record(
            self.provider,
            {"id": "demo/model", "upstream_id": "model"},
        ).to_dict()
        cap = record["capabilities"]["output_modalities"]
        self.assertEqual(cap["value"], "unknown")
        self.assertEqual(cap["source"], "unknown")

    def test_supported_protocols_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "supported_protocols": ["responses", "chat_completions"],
                "capability_sources": {
                    "supported_protocols": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["supported_protocols"]
        self.assertEqual(cap["value"], ["responses", "chat_completions"])
        self.assertEqual(cap["source"], "official")

    def test_max_input_tokens_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "max_input_tokens": 100000,
                "capability_sources": {
                    "max_input_tokens": {
                        "source": "manual",
                        "confidence": 1.0,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["max_input_tokens"]
        self.assertEqual(cap["value"], 100000)
        self.assertEqual(cap["source"], "manual")

    def test_reasoning_control_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "reasoning_control": "reasoning.effort enum: low, high",
                "capability_sources": {
                    "reasoning_control": {
                        "source": "official",
                        "confidence": 0.95,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["reasoning_control"]
        self.assertEqual(cap["value"], "reasoning.effort enum: low, high")
        self.assertEqual(cap["source"], "official")

    def test_structured_output_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "capabilities": {"structured_output": True},
                "capability_sources": {
                    "structured_output": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["structured_output"]
        self.assertEqual(cap["value"], True)
        self.assertEqual(cap["source"], "advertised")

    def test_web_search_in_record(self):
        record = capability_record(
            self.provider,
            {
                "id": "demo/model",
                "upstream_id": "model",
                "capabilities": {"web_search": False},
                "capability_sources": {
                    "web_search": {
                        "source": "observed",
                        "confidence": 1.0,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    }
                },
            },
        ).to_dict()
        cap = record["capabilities"]["web_search"]
        self.assertEqual(cap["value"], False)
        self.assertEqual(cap["source"], "observed")

    def test_all_new_fields_present_in_record(self):
        record = capability_record(
            self.provider,
            {"id": "demo/model", "upstream_id": "model"},
        ).to_dict()
        for name in (
            "output_modalities",
            "supported_protocols",
            "max_input_tokens",
            "reasoning_control",
            "structured_output",
            "web_search",
        ):
            self.assertIn(name, record["capabilities"])


if __name__ == "__main__":
    unittest.main()
