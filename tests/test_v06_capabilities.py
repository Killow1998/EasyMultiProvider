import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider import router
from easy_multi_provider.capabilities import capability_record, make_provenance
from easy_multi_provider.catalog import build_catalog
from easy_multi_provider.config import load, normalize, save
from easy_multi_provider.server import AppState, WEB_FILE


class CapabilityTruthTests(unittest.TestCase):
    def test_inherited_is_a_supported_capability_source(self):
        provenance = make_provenance("inherited")

        self.assertEqual(provenance["source"], "inherited")
        self.assertGreater(provenance["confidence"], 0)

    def test_reasoning_support_without_exact_levels_does_not_invent_effort(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize(
                {
                    "native_catalog_path": str(Path(directory) / "missing.json"),
                    "providers": [
                        {
                            "id": "provider",
                            "base_url": "https://example.com/v1",
                            "protocol": "responses",
                        }
                    ],
                    "models": [
                        {
                            "id": "provider/model",
                            "provider": "provider",
                            "upstream_id": "model",
                            "supports_reasoning": True,
                            "reasoning_levels": [],
                            "capability_sources": {
                                "supports_reasoning": {
                                    "source": "advertised",
                                    "confidence": 0.75,
                                    "observed_at": "2026-08-22T00:00:00+00:00",
                                }
                            },
                        }
                    ],
                }
            )

            model = config["models"][0]
            entry = build_catalog(config)["models"][0]
            record = capability_record(config["providers"][0], model).to_dict()

        self.assertTrue(model["supports_reasoning"])
        self.assertEqual(model["reasoning_levels"], [])
        self.assertNotIn("default_reasoning_level", entry)
        self.assertEqual(entry["supported_reasoning_levels"], [])
        self.assertEqual(record["capabilities"]["supports_reasoning"]["value"], True)
        self.assertEqual(
            record["capabilities"]["supports_reasoning"]["source"], "advertised"
        )
        self.assertEqual(record["capabilities"]["reasoning_levels"]["value"], "unknown")

    def test_discovery_separates_reasoning_support_from_exact_levels(self):
        class Response:
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return json.dumps(
                    {
                        "data": [
                            {
                                "id": "support-only",
                                "supported_parameters": ["reasoning"],
                            },
                            {
                                "id": "exact-levels",
                                "supported_parameters": ["reasoning"],
                                "reasoning_levels": ["low", "high"],
                            },
                        ]
                    }
                ).encode("utf-8")

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return None

        provider = {
            "id": "provider",
            "base_url": "https://example.com/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "fixture-key",
        }
        with patch.object(router, "urlopen", return_value=Response()):
            models = router.discover_models(provider)

        self.assertTrue(models[0]["supports_reasoning"])
        self.assertEqual(models[0]["reasoning_levels"], [])
        self.assertEqual(
            models[0]["capability_sources"]["supports_reasoning"]["source"],
            "advertised",
        )
        self.assertTrue(models[1]["supports_reasoning"])
        self.assertEqual(models[1]["reasoning_levels"], ["low", "high"])
        self.assertEqual(
            models[1]["capability_sources"]["reasoning_levels"]["source"],
            "advertised",
        )

    def test_reasoning_levels_use_codex_progression_and_middle_default(self):
        with tempfile.TemporaryDirectory() as directory:
            config = normalize(
                {
                    "native_catalog_path": str(Path(directory) / "missing.json"),
                    "providers": [
                        {
                            "id": "provider",
                            "base_url": "https://example.com/v1",
                            "protocol": "responses",
                        }
                    ],
                    "models": [
                        {
                            "id": "provider/model",
                            "provider": "provider",
                            "reasoning_levels": ["max", "high", "low", "HIGH"],
                        }
                    ],
                }
            )

            model = config["models"][0]
            entry = build_catalog(config)["models"][0]

        self.assertEqual(model["reasoning_levels"], ["low", "high", "max"])
        self.assertEqual(
            [item["effort"] for item in entry["supported_reasoning_levels"]],
            ["low", "high", "max"],
        )
        self.assertEqual(entry["default_reasoning_level"], "high")

    def test_generic_reasoning_signal_never_uses_family_inference_for_levels(self):
        class Response:
            sent = False

            def read(self, size=-1):
                if self.sent:
                    return b""
                self.sent = True
                return json.dumps(
                    {
                        "models": [
                            {
                                "name": "models/reasoning-model",
                                "supportedGenerationMethods": ["generateContent"],
                                "thinking": True,
                            }
                        ]
                    }
                ).encode("utf-8")

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return None

        provider = {
            "id": "provider",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "fixture-key",
        }
        with patch.object(router, "urlopen", return_value=Response()):
            model = router.discover_models(provider)[0]

        self.assertTrue(model["supports_reasoning"])
        self.assertEqual(model["reasoning_levels"], [])
        self.assertEqual(
            model["capability_sources"]["supports_reasoning"]["source"],
            "advertised",
        )
        self.assertEqual(
            model["capability_sources"]["reasoning_levels"]["source"], "unknown"
        )

    def test_unknown_effort_levels_are_omitted_from_all_external_protocols(self):
        captured = []

        class Response:
            status = 200
            headers = {"Content-Type": "application/json"}

            def __init__(self, raw):
                self.raw = raw

            def __enter__(self):
                return self

            def __exit__(self, *args):
                return None

            def read(self, size=-1):
                raw, self.raw = self.raw, b""
                return raw

            def close(self):
                pass

        def request(provider, payload, incoming, stream=False, operation="", context_check=None):
            captured.append(payload)
            if provider["protocol"] == "chat_completions":
                return Response(b'{"choices":[{"message":{"content":"ok"}}]}')
            return Response(b'{"status":"completed","output":[]}')

        model = {
            "id": "provider/model",
            "upstream_id": "model",
            "supports_reasoning": True,
            "reasoning_levels": [],
        }
        body = {
            "model": "provider/model",
            "input": [],
            "reasoning": {"effort": "high"},
        }
        with patch.object(router, "_request", side_effect=request):
            router.chat_completion(
                {
                    "id": "provider",
                    "protocol": "chat_completions",
                    "auth_mode": "api_key",
                    "base_url": "https://example.com/v1",
                },
                body,
                model,
                {},
            )
            router.forward_responses(
                {
                    "id": "provider",
                    "protocol": "responses",
                    "auth_mode": "api_key",
                    "base_url": "https://example.com/v1",
                },
                body,
                model,
                {},
            )

        self.assertNotIn("reasoning_effort", captured[0])
        self.assertNotIn("reasoning", captured[1])

    def test_manual_reasoning_context_and_visibility_survive_refresh(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            observed_at = "2026-08-22T00:00:00+00:00"
            save(
                normalize(
                    {
                        "native_catalog_path": str(root / "missing.json"),
                        "providers": [
                            {
                                "id": "provider",
                                "base_url": "https://example.com/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                                "api_key": "fixture-key",
                            }
                        ],
                        "models": [
                            {
                                "id": "provider/model",
                                "provider": "provider",
                                "upstream_id": "model",
                                "supports_reasoning": False,
                                "reasoning_levels": [],
                                "context_window": 77777,
                                "visibility": "hide",
                                "capability_sources": {
                                    "supports_reasoning": {
                                        "source": "manual",
                                        "confidence": 1,
                                        "observed_at": observed_at,
                                    },
                                    "reasoning_levels": {
                                        "source": "manual",
                                        "confidence": 1,
                                        "observed_at": observed_at,
                                    },
                                    "context_window": {
                                        "source": "manual",
                                        "confidence": 1,
                                        "observed_at": observed_at,
                                    },
                                },
                            }
                        ],
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            discovered = [
                {
                    "upstream_id": "model",
                    "supports_reasoning": True,
                    "reasoning_levels": ["high"],
                    "context_window": 99999,
                    "capability_sources": {
                        "supports_reasoning": {"source": "advertised"},
                        "reasoning_levels": {"source": "advertised"},
                    },
                }
            ]
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=discovered,
            ), patch(
                "easy_multi_provider.server.generated_catalog_path",
                return_value=root / "catalog.json",
            ):
                state.discover_provider_models("provider", ["model"])

            model = load(config_path)["models"][0]

        self.assertFalse(model["supports_reasoning"])
        self.assertEqual(model["reasoning_levels"], [])
        self.assertEqual(model["context_window"], 77777)
        self.assertEqual(model["visibility"], "hide")
        for field in ("supports_reasoning", "reasoning_levels", "context_window"):
            self.assertEqual(model["capability_sources"][field]["source"], "manual")

    def test_discovered_support_only_model_persists_unknown_effort_levels(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            config_path = root / "config.json"
            save(
                normalize(
                    {
                        "native_catalog_path": str(root / "missing.json"),
                        "providers": [
                            {
                                "id": "provider",
                                "base_url": "https://example.com/v1",
                                "protocol": "responses",
                                "auth_mode": "api_key",
                                "api_key": "fixture-key",
                            }
                        ],
                    }
                ),
                config_path,
            )
            state = AppState(config_path)
            discovered = [
                {
                    "upstream_id": "model",
                    "supports_reasoning": True,
                    "reasoning_levels": [],
                    "capability_sources": {
                        "supports_reasoning": {"source": "advertised"},
                        "reasoning_levels": {"source": "unknown"},
                    },
                }
            ]
            with patch(
                "easy_multi_provider.server.discover_models",
                return_value=discovered,
            ), patch(
                "easy_multi_provider.server.generated_catalog_path",
                return_value=root / "catalog.json",
            ):
                state.discover_provider_models("provider", ["model"])

            model = load(config_path)["models"][0]

        self.assertTrue(model["supports_reasoning"])
        self.assertEqual(model["reasoning_levels"], [])
        self.assertEqual(
            model["capability_sources"]["supports_reasoning"]["source"],
            "advertised",
        )
        self.assertEqual(
            model["capability_sources"]["reasoning_levels"]["source"], "unknown"
        )

    def test_web_model_editor_does_not_create_a_default_effort_level(self):
        html = WEB_FILE.read_text(encoding="utf-8")

        self.assertNotIn("否则使用默认 medium", html)
        self.assertNotIn("previous?.reasoning_levels || ['medium']", html)
        self.assertIn("支持推理，档位未知", html)


if __name__ == "__main__":
    unittest.main()
