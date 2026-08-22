import json
import unittest

from easy_multi_provider.capabilities import (
    CAPABILITY_NAMES,
    endpoint_fingerprint,
    capability_record,
    safe_capability_list,
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


if __name__ == "__main__":
    unittest.main()
