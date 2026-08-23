"""Focused tests for official registry integration: discovery + refresh merge."""

import json
import tempfile
import unittest
from pathlib import Path
from unittest.mock import patch

from tests.support import ensure_test_master_key

import easy_multi_provider.router as router
from easy_multi_provider.capabilities import normalize_input_modalities
from easy_multi_provider.config import load, normalize, save
from easy_multi_provider.official_registry import enrich_discovered_models, load_registry
from easy_multi_provider.router import discover_models
from easy_multi_provider.server import (
    _build_new_model_from_discovery,
    _field_source,
    _merge_discovered_field,
    _merge_discovered_nested_bool,
    _SOURCE_RANK,
    AppState,
)

ensure_test_master_key()


class _JsonResponse:
    status = 200
    headers = {"Content-Type": "application/json"}

    def __init__(self, value):
        self._raw = json.dumps(value).encode("utf-8")
        self._read = False

    def read(self, size=-1):
        if self._read:
            return b""
        self._read = True
        return self._raw

    def close(self):
        pass

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


class TestOpenRouterDiscovery(unittest.TestCase):
    """OpenRouter /models: input/output modalities + supported_parameters."""

    def _provider(self):
        return {
            "id": "openrouter",
            "base_url": "https://openrouter.ai/api/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)),              patch.object(router, "_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_input_and_output_modalities_parsed(self):
        models = self._discover({
            "data": [{
                "id": "vision-model",
                "name": "Vision",
                "architecture": {
                    "input_modalities": ["text", "image"],
                    "output_modalities": ["text", "audio"],
                },
                "context_length": 128000,
            }]
        })
        m = models[0]
        self.assertEqual(m["input_modalities"], ["text", "image"])
        self.assertEqual(m["output_modalities"], ["text", "audio"])

    def test_supported_parameters_map_to_capabilities(self):
        models = self._discover({
            "data": [{
                "id": "capable-model",
                "architecture": {"input_modalities": ["text"]},
                "supported_parameters": [
                    "tools", "parallel_tool_calls",
                    "structured_outputs", "reasoning",
                ],
                "streaming": True,
            }]
        })
        m = models[0]
        caps = m.get("capabilities", {})
        self.assertTrue(caps.get("structured_tools"))
        self.assertTrue(caps.get("parallel_tools"))
        self.assertTrue(caps.get("structured_output"))
        self.assertTrue(caps.get("streaming"))
        self.assertTrue(m.get("supports_reasoning"))
        sources = m.get("capability_sources", {})
        self.assertEqual(sources.get("structured_tools", {}).get("source"), "advertised")
        self.assertEqual(sources.get("streaming", {}).get("source"), "advertised")

    def test_absent_streaming_is_unknown(self):
        models = self._discover({
            "data": [{"id": "basic-model", "architecture": {"input_modalities": ["text"]}}]
        })
        caps = models[0].get("capabilities", {})
        self.assertNotIn("streaming", caps)

    def test_reasoning_levels_not_invented(self):
        models = self._discover({
            "data": [{
                "id": "reasoning-model",
                "architecture": {"input_modalities": ["text"]},
                "supported_parameters": ["reasoning"],
            }]
        })
        self.assertTrue(models[0].get("supports_reasoning"))
        self.assertEqual(models[0].get("reasoning_levels"), [])

    def test_enrich_called_for_custom_provider(self):
        """Custom providers still go through enrich; identity is exact root."""
        models = self._discover({
            "data": [{"id": "custom-model", "architecture": {"input_modalities": ["text"]}}]
        })
        # Custom provider should not get official enrichment (no matching root)
        self.assertEqual(models[0]["upstream_id"], "custom-model")
        # Input modalities should remain as advertised, not upgraded
        self.assertEqual(models[0]["input_modalities"], ["text"])


class TestGeminiDiscoveryNoHardcodedFallback(unittest.TestCase):
    """Gemini discovery uses advertised or registry, not hardcoded model IDs."""

    def _provider(self):
        return {
            "id": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)),              patch.object(router, "_discovery_headers", return_value={}),              patch.object(router, "_anthropic_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_gemini_without_advertised_modalities_uses_registry_fallback(self):
        models = self._discover({
            "models": [{
                "name": "models/gemini-3.7-flash",
                "displayName": "Gemini 3.7 Flash",
                "supportedGenerationMethods": ["generateContent"],
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
            }]
        })
        # No hardcoded fallback; registry enriches gemini-3.7-flash with text+image
        self.assertEqual(models[0]["input_modalities"], ["text", "image"])
        self.assertEqual(
            models[0]["capability_sources"]["input_modalities"]["source"],
            "official",
        )

    def test_gemini_with_advertised_output_modalities(self):
        models = self._discover({
            "models": [{
                "name": "models/gemini-3.7-flash",
                "supportedGenerationMethods": ["generateContent"],
                "inputModalities": ["text", "image"],
                "outputModalities": ["text"],
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
            }]
        })
        self.assertEqual(models[0]["input_modalities"], ["text", "image"])
        self.assertEqual(models[0]["output_modalities"], ["text"])
        self.assertEqual(
            models[0]["capability_sources"]["input_modalities"]["source"],
            "advertised",
        )
        self.assertEqual(
            models[0]["capability_sources"]["output_modalities"]["source"],
            "advertised",
        )


class TestAnthropicDiscovery(unittest.TestCase):
    """Anthropic GET /v1/models: never calls /messages."""

    def _provider(self):
        return {
            "id": "anthropic",
            "base_url": "https://api.anthropic.com/v1",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "api_key": "test-key",
        }

    def test_anthropic_models_parsed(self):
        response_data = {
            "data": [
                {
                    "id": "claude-opus-5",
                    "display_name": "Claude Opus 5",
                    "created_at": "2026-01-01T00:00:00Z",
                    "max_input_tokens": 1000000,
                    "max_tokens": 128000,
                },
                {
                    "id": "claude-sonnet-5",
                    "display_name": "Claude Sonnet 5",
                    "created_at": "2026-01-01T00:00:00Z",
                    "max_input_tokens": 1000000,
                    "max_tokens": 128000,
                },
            ],
            "has_more": False,
            "last_id": "claude-sonnet-5",
        }
        with patch.object(router, "urlopen", return_value=_JsonResponse(response_data)),              patch.object(router, "_anthropic_discovery_headers", return_value={}):
            models = discover_models(self._provider())
        self.assertEqual(len(models), 2)
        self.assertEqual(models[0]["upstream_id"], "claude-opus-5")
        self.assertEqual(models[0]["context_window"], 1000000)
        self.assertEqual(models[0]["output_limit"], 128000)

    def test_anthropic_pagination(self):
        page1 = {
            "data": [{"id": "model-a", "display_name": "A"}],
            "has_more": True,
            "last_id": "model-a",
        }
        page2 = {
            "data": [{"id": "model-b", "display_name": "B"}],
            "has_more": False,
            "last_id": "model-b",
        }
        with patch.object(
            router, "urlopen",
            side_effect=[_JsonResponse(page1), _JsonResponse(page2)],
        ), patch.object(router, "_anthropic_discovery_headers", return_value={}):
            models = discover_models(self._provider())
        self.assertEqual(len(models), 2)
        self.assertEqual(models[0]["upstream_id"], "model-a")
        self.assertEqual(models[1]["upstream_id"], "model-b")

    def test_anthropic_never_calls_messages(self):
        """Verify the discovery URL is /models, not /messages."""
        response_data = {"data": [], "has_more": False}
        with patch.object(router, "urlopen", return_value=_JsonResponse(response_data)) as opened,              patch.object(router, "_anthropic_discovery_headers", return_value={}):
            discover_models(self._provider())
        url = opened.call_args.args[0].full_url
        self.assertIn("/models", url)
        self.assertNotIn("/messages", url)


class TestRegistryEnrichment(unittest.TestCase):
    """Registry fills unknown fields but not advertised."""

    def test_registry_fills_unknown_text_placeholder(self):
        registry = load_registry()
        openai_provider = {"base_url": "https://api.openai.com/v1"}
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "unknown",
                    "confidence": 0.0,
                    "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(openai_provider, models, registry)
        # Official should upgrade the unknown text placeholder
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])

    def test_advertised_survives_official_fallback(self):
        registry = load_registry()
        openai_provider = {"base_url": "https://api.openai.com/v1"}
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "advertised",
                    "confidence": 0.75,
                    "observed_at": "2026-08-22T00:00:00+00:00",
                }
            },
        }]
        result = enrich_discovered_models(openai_provider, models, registry)
        self.assertEqual(result[0]["input_modalities"], ["text"])

    def test_custom_provider_no_enrich(self):
        registry = load_registry()
        custom_provider = {"base_url": "https://my-proxy.example.com/v1"}
        models = [{"upstream_id": "gpt-5.6-sol", "context_window": None}]
        result = enrich_discovered_models(custom_provider, models, registry)
        self.assertIsNone(result[0]["context_window"])


class TestRefreshMergePrecedence(unittest.TestCase):
    """Field-level merge: manual/observed > advertised > official > inferred > unknown."""

    def test_advertised_overwrites_official(self):
        existing = {
            "context_window": 100000,
            "capability_sources": {
                "context_window": {"source": "official", "confidence": 0.95, "observed_at": None}
            },
        }
        item = {"context_window": 200000}
        _merge_discovered_field(existing, item, "context_window", "advertised", "2026-08-22T00:00:00+00:00")
        self.assertEqual(existing["context_window"], 200000)
        self.assertEqual(existing["capability_sources"]["context_window"]["source"], "advertised")

    def test_official_does_not_overwrite_advertised(self):
        existing = {
            "context_window": 200000,
            "capability_sources": {
                "context_window": {"source": "advertised", "confidence": 0.75, "observed_at": None}
            },
        }
        item = {"context_window": 100000}
        _merge_discovered_field(existing, item, "context_window", "official", "2026-08-22T00:00:00+00:00")
        self.assertEqual(existing["context_window"], 200000)

    def test_manual_not_overwritten_by_advertised(self):
        existing = {
            "context_window": 50000,
            "capability_sources": {
                "context_window": {"source": "manual", "confidence": 1.0, "observed_at": None}
            },
        }
        item = {"context_window": 200000}
        _merge_discovered_field(existing, item, "context_window", "advertised", "2026-08-22T00:00:00+00:00")
        self.assertEqual(existing["context_window"], 50000)

    def test_observed_not_overwritten_by_official(self):
        existing = {
            "supports_reasoning": False,
            "capability_sources": {
                "supports_reasoning": {"source": "observed", "confidence": 1.0, "observed_at": None}
            },
        }
        item = {"supports_reasoning": True}
        _merge_discovered_field(existing, item, "supports_reasoning", "official", "2026-08-22T00:00:00+00:00")
        self.assertFalse(existing["supports_reasoning"])

    def test_official_overwrites_inferred(self):
        existing = {
            "context_window": 50000,
            "capability_sources": {
                "context_window": {"source": "inferred", "confidence": 0.35, "observed_at": None}
            },
        }
        item = {"context_window": 200000}
        _merge_discovered_field(existing, item, "context_window", "official", "2026-08-22T00:00:00+00:00")
        self.assertEqual(existing["context_window"], 200000)

    def test_official_overwrites_unknown(self):
        existing = {
            "context_window": 0,
            "capability_sources": {
                "context_window": {"source": "unknown", "confidence": 0.0, "observed_at": None}
            },
        }
        item = {"context_window": 200000}
        _merge_discovered_field(existing, item, "context_window", "official", "2026-08-22T00:00:00+00:00")
        self.assertEqual(existing["context_window"], 200000)

    def test_nested_capability_merge(self):
        existing = {
            "capabilities": {"streaming": False},
            "capability_sources": {
                "streaming": {"source": "official", "confidence": 0.95, "observed_at": None}
            },
        }
        item = {"capabilities": {"streaming": True}}
        _merge_discovered_nested_bool(existing, item, "streaming", "advertised", "2026-08-22T00:00:00+00:00")
        self.assertTrue(existing["capabilities"]["streaming"])
        self.assertEqual(existing["capability_sources"]["streaming"]["source"], "advertised")

    def test_nested_capability_manual_survives(self):
        existing = {
            "capabilities": {"structured_output": False},
            "capability_sources": {
                "structured_output": {"source": "manual", "confidence": 1.0, "observed_at": None}
            },
        }
        item = {"capabilities": {"structured_output": True}}
        _merge_discovered_nested_bool(existing, item, "structured_output", "advertised", "2026-08-22T00:00:00+00:00")
        self.assertFalse(existing["capabilities"]["structured_output"])


class TestNewModelFromDiscoveryPreservesSource(unittest.TestCase):
    """Newly selected models persist actual incoming source, never hardcoded."""

    def test_advertised_source_preserved(self):
        item = {
            "upstream_id": "test-model",
            "display_name": "Test",
            "context_window": 128000,
            "input_modalities": ["text", "image"],
            "capability_sources": {
                "context_window": {"source": "advertised", "confidence": 0.75, "observed_at": None},
                "input_modalities": {"source": "advertised", "confidence": 0.75, "observed_at": None},
            },
        }
        model = _build_new_model_from_discovery(item, "demo", "2026-08-22T00:00:00+00:00")
        self.assertEqual(model["capability_sources"]["context_window"]["source"], "advertised")
        self.assertEqual(model["capability_sources"]["input_modalities"]["source"], "advertised")

    def test_official_source_preserved(self):
        item = {
            "upstream_id": "gpt-5.6-sol",
            "display_name": "GPT-5.6 Sol",
            "context_window": 1050000,
            "input_modalities": ["text", "image"],
            "capability_sources": {
                "context_window": {"source": "official", "confidence": 0.95, "observed_at": None},
                "input_modalities": {"source": "official", "confidence": 0.95, "observed_at": None},
            },
        }
        model = _build_new_model_from_discovery(item, "demo", "2026-08-22T00:00:00+00:00")
        self.assertEqual(model["capability_sources"]["context_window"]["source"], "official")


class TestEndToEndDiscoveryAndMerge(unittest.TestCase):
    """Integration: discovery -> enrich -> merge into config."""

    def _provider(self):
        return {
            "id": "openrouter",
            "base_url": "https://openrouter.ai/api/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _raw_config(self, root, models):
        return {
            "native_catalog_path": str(Path(root) / "native-models.json"),
            "providers": [self._provider()],
            "models": models,
        }

    def _state(self, root, models=None):
        root = Path(root)
        path = root / "config.json"
        save(normalize(self._raw_config(root, models or [])), path)
        return AppState(path, catalog_path=root / "catalog.json"), path

    def test_manual_context_survives_refresh(self):
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "openrouter/vision-model",
                "provider": "openrouter",
                "upstream_id": "vision-model",
                "context_window": 77777,
                "visibility": "hide",
                "input_modalities": ["text", "image"],
                "capability_sources": {
                    "context_window": {
                        "source": "manual",
                        "confidence": 1.0,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                    "input_modalities": {
                        "source": "manual",
                        "confidence": 1.0,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                },
            }])
            discovered = [{
                "upstream_id": "vision-model",
                "input_modalities": ["text"],
                "context_window": 4096,
                "capability_sources": {
                    "input_modalities": {"source": "advertised", "confidence": 0.75, "observed_at": None},
                    "context_window": {"source": "advertised", "confidence": 0.75, "observed_at": None},
                },
            }]
            with patch("easy_multi_provider.server.discover_models", return_value=discovered),                  patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("openrouter", ["vision-model"])
            model = load(path)["models"][0]
            self.assertEqual(model["context_window"], 77777)
            self.assertEqual(model.get("visibility"), "hide")
            self.assertEqual(model.get("input_modalities"), ["text", "image"])

    def test_official_context_replaces_unknown(self):
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "openrouter/text-model",
                "provider": "openrouter",
                "upstream_id": "text-model",
                "context_window": 0,
                "capability_sources": {
                    "context_window": {"source": "unknown", "confidence": 0.0, "observed_at": None},
                },
            }])
            discovered = [{
                "upstream_id": "text-model",
                "context_window": 128000,
                "capability_sources": {
                    "context_window": {"source": "advertised", "confidence": 0.75, "observed_at": None},
                },
            }]
            with patch("easy_multi_provider.server.discover_models", return_value=discovered),                  patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("openrouter", ["text-model"])
            model = load(path)["models"][0]
            self.assertEqual(model["context_window"], 128000)

    def test_new_model_gets_actual_source_not_hardcoded(self):
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory)
            discovered = [{
                "upstream_id": "new-model",
                "display_name": "New",
                "context_window": 128000,
                "input_modalities": ["text", "image"],
                "capability_sources": {
                    "context_window": {"source": "official", "confidence": 0.95, "observed_at": None},
                    "input_modalities": {"source": "advertised", "confidence": 0.75, "observed_at": None},
                },
            }]
            with patch("easy_multi_provider.server.discover_models", return_value=discovered),                  patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("openrouter", ["new-model"])
            model = load(path)["models"][0]
            self.assertEqual(
                model["capability_sources"]["context_window"]["source"],
                "official",
            )
            self.assertEqual(
                model["capability_sources"]["input_modalities"]["source"],
                "advertised",
            )


class TestGenericOutputLimitWithoutTopProvider(unittest.TestCase):
    """Output-limit extraction must work when top_provider is absent."""

    def _provider(self):
        return {
            "id": "openrouter",
            "base_url": "https://openrouter.ai/api/v1",
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)), \
             patch.object(router, "_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_max_tokens_without_top_provider(self):
        models = self._discover({
            "data": [{
                "id": "model-a",
                "architecture": {"input_modalities": ["text"]},
                "max_tokens": 8192,
            }]
        })
        self.assertEqual(models[0]["output_limit"], 8192)
        self.assertEqual(
            models[0]["capability_sources"]["output_limit"]["source"],
            "advertised",
        )

    def test_max_output_tokens_without_top_provider(self):
        models = self._discover({
            "data": [{
                "id": "model-b",
                "architecture": {"input_modalities": ["text"]},
                "max_output_tokens": 16384,
            }]
        })
        self.assertEqual(models[0]["output_limit"], 16384)

    def test_output_limit_field_without_top_provider(self):
        models = self._discover({
            "data": [{
                "id": "model-c",
                "architecture": {"input_modalities": ["text"]},
                "output_limit": 4096,
            }]
        })
        self.assertEqual(models[0]["output_limit"], 4096)

    def test_top_provider_max_completion_tokens(self):
        models = self._discover({
            "data": [{
                "id": "model-d",
                "architecture": {"input_modalities": ["text"]},
                "top_provider": {"max_completion_tokens": 32768},
            }]
        })
        self.assertEqual(models[0]["output_limit"], 32768)

    def test_context_window_provenance_advertised(self):
        models = self._discover({
            "data": [{
                "id": "model-e",
                "architecture": {"input_modalities": ["text"]},
                "context_length": 200000,
            }]
        })
        self.assertEqual(models[0]["context_window"], 200000)
        self.assertEqual(
            models[0]["capability_sources"]["context_window"]["source"],
            "advertised",
        )

    def test_max_input_tokens_provenance_advertised(self):
        models = self._discover({
            "data": [{
                "id": "model-f",
                "architecture": {"input_modalities": ["text"]},
                "max_input_tokens": 180000,
            }]
        })
        self.assertEqual(models[0]["max_input_tokens"], 180000)
        self.assertEqual(
            models[0]["capability_sources"]["max_input_tokens"]["source"],
            "advertised",
        )


class TestGeminiTokenProvenance(unittest.TestCase):
    """Live Gemini token fields retain advertised provenance."""

    def _provider(self):
        return {
            "id": "gemini",
            "base_url": "https://generativelanguage.googleapis.com/v1beta/openai",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)), \
             patch.object(router, "_discovery_headers", return_value={}), \
             patch.object(router, "_anthropic_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_gemini_context_window_provenance(self):
        models = self._discover({
            "models": [{
                "name": "models/gemini-2.5-flash",
                "supportedGenerationMethods": ["generateContent"],
                "inputTokenLimit": 1048576,
                "outputTokenLimit": 65536,
            }]
        })
        m = models[0]
        self.assertEqual(m["context_window"], 1048576)
        self.assertEqual(
            m["capability_sources"]["context_window"]["source"],
            "advertised",
        )
        self.assertEqual(m["output_limit"], 65536)
        self.assertEqual(
            m["capability_sources"]["output_limit"]["source"],
            "advertised",
        )


class TestAnthropicRFC3339AndCapabilities(unittest.TestCase):
    """Anthropic RFC3339 date parsing and capability booleans."""

    def _provider(self):
        return {
            "id": "anthropic",
            "base_url": "https://api.anthropic.com/v1",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)), \
             patch.object(router, "_anthropic_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_rfc3339_created_at_parsed_to_timestamp(self):
        models = self._discover({
            "data": [{
                "id": "claude-opus-5",
                "display_name": "Claude Opus 5",
                "created_at": "2026-01-15T12:30:00Z",
                "max_input_tokens": 1000000,
                "max_tokens": 128000,
            }],
            "has_more": False,
        })
        self.assertGreater(models[0]["created_at"], 0)

    def test_rfc3339_with_offset_parsed(self):
        models = self._discover({
            "data": [{
                "id": "claude-sonnet-5",
                "display_name": "Claude Sonnet 5",
                "created_at": "2026-06-01T08:00:00-08:00",
                "max_input_tokens": 1000000,
                "max_tokens": 128000,
            }],
            "has_more": False,
        })
        self.assertGreater(models[0]["created_at"], 0)

    def test_nested_capability_objects_parsed(self):
        """Anthropic Models API uses nested capability objects, not flat
        booleans.  Effort children are CapabilitySupport objects
        (effort.<level>.supported), not bare booleans.
        Uses a model ID not in the registry so enrichment does not add
        capabilities from the official docs layer."""
        models = self._discover({
            "data": [{
                "id": "claude-test-unlisted",
                "display_name": "Claude Test Unlisted",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 1000000,
                "max_tokens": 128000,
                "capabilities": {
                    "thinking": {"supported": True},
                    "effort": {
                        "supported": True,
                        "low": {"supported": True},
                        "medium": {"supported": True},
                        "high": {"supported": True},
                        "xhigh": {"supported": False},
                        "max": {"supported": True},
                    },
                    "image_input": {"supported": True},
                    "pdf_input": {"supported": False},
                    "structured_outputs": {"supported": True},
                },
            }],
            "has_more": False,
        })
        # Only documented fields are parsed; no generic tool_use/streaming/etc.
        caps = models[0].get("capabilities", {})
        self.assertNotIn("structured_tools", caps)
        self.assertNotIn("streaming", caps)
        self.assertNotIn("web_search", caps)
        self.assertNotIn("parallel_tools", caps)
        self.assertTrue(caps.get("structured_output"))
        sources = models[0]["capability_sources"]
        self.assertEqual(sources["structured_output"]["source"], "advertised")
        # Reasoning support from thinking.supported
        self.assertTrue(models[0]["supports_reasoning"])
        # Reasoning levels from effort children (canonical order preserved)
        self.assertEqual(
            models[0]["reasoning_levels"],
            ["low", "medium", "high", "max"],
        )
        # Input modalities from image_input/pdf_input
        self.assertIn("image", models[0]["input_modalities"])
        self.assertNotIn("pdf", models[0]["input_modalities"])

    def test_no_invented_capabilities_when_absent(self):
        """When capabilities object is absent or empty, nothing is invented.
        Uses a model ID not in the registry so enrichment does not add
        capabilities from the official docs layer."""
        models = self._discover({
            "data": [{
                "id": "claude-test-unlisted-2",
                "display_name": "Claude Test Unlisted 2",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
            }],
            "has_more": False,
        })
        caps = models[0].get("capabilities", {})
        self.assertNotIn("structured_tools", caps)
        self.assertNotIn("streaming", caps)
        self.assertNotIn("web_search", caps)
        self.assertNotIn("parallel_tools", caps)
        self.assertNotIn("structured_output", caps)
        self.assertIsNone(models[0]["supports_reasoning"])
        self.assertEqual(models[0]["reasoning_levels"], [])

    def test_anthropic_url_never_includes_messages(self):
        response_data = {"data": [], "has_more": False}
        with patch.object(router, "urlopen", return_value=_JsonResponse(response_data)) as opened, \
             patch.object(router, "_anthropic_discovery_headers", return_value={}):
            discover_models(self._provider())
        url = opened.call_args.args[0].full_url
        self.assertIn("/models", url)
        self.assertNotIn("/messages", url)

    def test_anthropic_token_provenance(self):
        models = self._discover({
            "data": [{
                "id": "claude-haiku-4-5",
                "display_name": "Claude Haiku 4.5",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
            }],
            "has_more": False,
        })
        m = models[0]
        self.assertEqual(m["context_window"], 200000)
        self.assertEqual(
            m["capability_sources"]["context_window"]["source"],
            "advertised",
        )
        self.assertEqual(m["output_limit"], 64000)
        self.assertEqual(
            m["capability_sources"]["output_limit"]["source"],
            "advertised",
        )


class TestRefreshPreservesExplicitUnknown(unittest.TestCase):
    """An explicit unknown source on a normalized ['text'] default must stay unknown."""

    def test_unknown_modality_stays_unknown_through_refresh(self):
        existing = {
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "unknown",
                    "confidence": 0.0,
                    "observed_at": None,
                }
            },
        }
        item = {
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {"source": "unknown", "confidence": 0.0, "observed_at": None},
            },
        }
        _merge_discovered_field(
            existing, item, "input_modalities", "unknown",
            "2026-08-22T00:00:00+00:00",
        )
        self.assertEqual(existing["input_modalities"], ["text"])
        self.assertEqual(
            existing["capability_sources"]["input_modalities"]["source"],
            "unknown",
        )

    def test_unknown_source_can_be_replaced_by_official_later(self):
        from easy_multi_provider.official_registry import enrich_discovered_models, load_registry
        registry = load_registry()
        openai_provider = {"base_url": "https://api.openai.com/v1"}
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "unknown", "confidence": 0.0, "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(openai_provider, models, registry)
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])
        self.assertEqual(
            result[0]["capability_sources"]["input_modalities"]["source"],
            "official",
        )

    def test_advertised_provenance_only_when_genuinely_live(self):
        """When discovery omits a source entry and value is live metadata,
        derive advertised only for genuine live fields."""
        item = {
            "upstream_id": "test-model",
            "display_name": "Test",
            "context_window": 128000,
            "output_limit": 8192,
            "input_modalities": ["text", "image"],
            # No capability_sources at all — genuinely omitted
        }
        model = _build_new_model_from_discovery(
            item, "demo", "2026-08-22T00:00:00+00:00"
        )
        sources = model["capability_sources"]
        self.assertEqual(sources["context_window"]["source"], "advertised")
        self.assertEqual(sources["output_limit"]["source"], "advertised")
        self.assertEqual(sources["input_modalities"]["source"], "advertised")


class TestAppStateProvenanceDerivation(unittest.TestCase):
    """AppState-level regression: explicit unknown provenance is preserved;
    absent provenance entry is derived as advertised."""

    def _provider(self):
        return {
            "id": "custom",
            "base_url": "https://custom.example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _raw_config(self, root, models):
        return {
            "native_catalog_path": str(Path(root) / "native-models.json"),
            "providers": [self._provider()],
            "models": models,
        }

    def _state(self, root, models=None):
        root = Path(root)
        path = root / "config.json"
        save(normalize(self._raw_config(root, models or [])), path)
        return AppState(path, catalog_path=root / "catalog.json"), path

    def test_explicit_unknown_stays_unknown_through_appstate_refresh(self):
        """An explicit {source: 'unknown'} on a normalized ['text'] default
        must remain unknown through the real AppState.discover_provider_models
        flow, including config save and reload."""
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "custom/text-model",
                "provider": "custom",
                "upstream_id": "text-model",
                "input_modalities": ["text"],
                "capability_sources": {
                    "input_modalities": {
                        "source": "unknown",
                        "confidence": 0.0,
                        "observed_at": None,
                    },
                },
            }])
            discovered = [{
                "upstream_id": "text-model",
                "input_modalities": ["text"],
                "capability_sources": {
                    "input_modalities": {
                        "source": "unknown",
                        "confidence": 0.0,
                        "observed_at": None,
                    },
                },
            }]
            with patch("easy_multi_provider.server.discover_models",
                       return_value=discovered),                  patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("custom", ["text-model"])
            model = load(path)["models"][0]
            self.assertEqual(model["input_modalities"], ["text"])
            self.assertEqual(
                model["capability_sources"]["input_modalities"]["source"],
                "unknown",
            )

    def test_absent_provenance_derived_as_advertised(self):
        """When discovery omits a provenance entry entirely for a live field,
        the AppState flow derives advertised provenance."""
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "custom/new-model",
                "provider": "custom",
                "upstream_id": "new-model",
                "input_modalities": ["text"],
                "capability_sources": {
                    "input_modalities": {
                        "source": "advertised",
                        "confidence": 0.75,
                        "observed_at": "2026-08-22T00:00:00+00:00",
                    },
                },
            }])
            # Discovery payload with NO capability_sources at all
            discovered = [{
                "upstream_id": "new-model",
                "input_modalities": ["text", "image"],
                "context_window": 128000,
            }]
            with patch("easy_multi_provider.server.discover_models",
                       return_value=discovered),                  patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("custom", ["new-model"])
            model = load(path)["models"][0]
            self.assertEqual(
                model["capability_sources"]["input_modalities"]["source"],
                "advertised",
            )
            self.assertEqual(
                model["capability_sources"]["context_window"]["source"],
                "advertised",
            )


class TestAnthropicNestedSchemaDiscovery(unittest.TestCase):
    """Verify the Anthropic Models API nested capability schema is parsed
    correctly through the real discovery path."""

    def _provider(self):
        return {
            "id": "anthropic",
            "base_url": "https://api.anthropic.com/v1",
            "protocol": "anthropic_messages",
            "auth_mode": "anthropic_api_key",
            "api_key": "test-key",
        }

    def _discover(self, value):
        with patch.object(router, "urlopen", return_value=_JsonResponse(value)),              patch.object(router, "_anthropic_discovery_headers", return_value={}):
            return discover_models(self._provider())

    def test_nested_capability_schema_parsed(self):
        """Effort children are CapabilitySupport objects, not booleans.

        Parse effort.<level>.supported, not a boolean at effort.<level>.
        supports_reasoning is the logical union of any explicitly present
        thinking.supported and effort.supported; preserve exact supported
        effort order.  Assert effort levels, reasoning support, image/PDF
        modalities, structured output, and no invented capabilities.
        """
        models = self._discover({
            "data": [{
                "id": "claude-test-unlisted-3",
                "display_name": "Claude Test Unlisted 3",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 1000000,
                "max_tokens": 128000,
                "capabilities": {
                    "thinking": {"supported": True},
                    "effort": {
                        "supported": True,
                        "low": {"supported": True},
                        "high": {"supported": True},
                        "max": {"supported": True},
                    },
                    "image_input": {"supported": True},
                    "pdf_input": {"supported": True},
                    "structured_outputs": {"supported": True},
                },
            }],
            "has_more": False,
        })
        model = models[0]
        caps = model.get("capabilities", {})
        # No invented capabilities.
        self.assertNotIn("structured_tools", caps)
        self.assertNotIn("streaming", caps)
        self.assertNotIn("web_search", caps)
        self.assertNotIn("parallel_tools", caps)
        # Structured output parsed from nested object.
        self.assertTrue(caps.get("structured_output"))
        # Reasoning support derived from thinking + effort union.
        self.assertTrue(model["supports_reasoning"])
        # Exact supported effort levels in canonical order.
        self.assertEqual(model["reasoning_levels"], ["low", "high", "max"])
        # Image and PDF modalities from explicit capability objects.
        self.assertIn("image", model["input_modalities"])
        self.assertIn("pdf", model["input_modalities"])
        self.assertEqual(
            model["capability_sources"]["input_modalities"]["source"],
            "advertised",
        )
        self.assertEqual(
            model["capability_sources"]["structured_output"]["source"],
            "advertised",
        )

    def test_no_modality_evidence_keeps_unknown_provenance(self):
        """When the Anthropic response exposes neither image_input.supported
        nor pdf_input.supported, normalize to text but keep provenance
        unknown rather than marking it as advertised."""
        models = self._discover({
            "data": [{
                "id": "claude-test-unlisted-4",
                "display_name": "Claude Test Unlisted 4",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
                "capabilities": {
                    "thinking": {"supported": False},
                },
            }],
            "has_more": False,
        })
        model = models[0]
        self.assertEqual(model["input_modalities"], ["text"])
        self.assertEqual(
            model["capability_sources"]["input_modalities"]["source"],
            "unknown",
        )

    def test_effort_child_object_not_boolean(self):
        """effort.high as a bare boolean (old schema) must not be accepted;
        only effort.high.supported objects are parsed."""
        models = self._discover({
            "data": [{
                "id": "claude-test-unlisted-5",
                "display_name": "Claude Test Unlisted 5",
                "created_at": "2026-01-01T00:00:00Z",
                "max_input_tokens": 200000,
                "max_tokens": 64000,
                "capabilities": {
                    "effort": {
                        "supported": True,
                        "high": True,
                    },
                },
            }],
            "has_more": False,
        })
        model = models[0]
        # Bare boolean effort child is not parsed as a supported level.
        self.assertEqual(model["reasoning_levels"], [])


class TestAppStateNestedProvenance(unittest.TestCase):
    """AppState-level nested provenance: explicit unknown preserved, absent
    entry derived as advertised, including config save/reload."""

    def _provider(self):
        return {
            "id": "custom",
            "base_url": "https://custom.example.com/v1",
            "protocol": "chat_completions",
            "auth_mode": "api_key",
            "api_key": "test-key",
        }

    def _raw_config(self, root, models):
        return {
            "native_catalog_path": str(Path(root) / "native-models.json"),
            "providers": [self._provider()],
            "models": models,
        }

    def _state(self, root, models=None):
        root = Path(root)
        path = root / "config.json"
        save(normalize(self._raw_config(root, models or [])), path)
        return AppState(path, catalog_path=root / "catalog.json"), path

    def test_explicit_nested_unknown_stays_unknown(self):
        """An explicit {source: 'unknown'} on a nested boolean must remain
        unknown through the real AppState.discover_provider_models flow,
        including config save and reload."""
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "custom/nested-model",
                "provider": "custom",
                "upstream_id": "nested-model",
                "input_modalities": ["text"],
                "capabilities": {"structured_output": True},
                "capability_sources": {
                    "structured_output": {
                        "source": "unknown",
                        "confidence": 0.0,
                        "observed_at": None,
                    },
                },
            }])
            discovered = [{
                "upstream_id": "nested-model",
                "input_modalities": ["text"],
                "capabilities": {"structured_output": True},
                "capability_sources": {
                    "structured_output": {
                        "source": "unknown",
                        "confidence": 0.0,
                        "observed_at": None,
                    },
                },
            }]
            with patch("easy_multi_provider.server.discover_models",
                       return_value=discovered), \
                 patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("custom", ["nested-model"])
            model = load(path)["models"][0]
            self.assertEqual(
                model["capability_sources"]["structured_output"]["source"],
                "unknown",
            )

    def test_absent_nested_provenance_derived_as_advertised(self):
        """When discovery omits a provenance entry for a nested boolean,
        the AppState flow derives advertised provenance."""
        with tempfile.TemporaryDirectory() as directory:
            state, path = self._state(directory, [{
                "id": "custom/nested-model-2",
                "provider": "custom",
                "upstream_id": "nested-model-2",
                "input_modalities": ["text"],
                "capability_sources": {},
            }])
            discovered = [{
                "upstream_id": "nested-model-2",
                "input_modalities": ["text"],
                "capabilities": {"structured_output": True},
            }]
            with patch("easy_multi_provider.server.discover_models",
                       return_value=discovered), \
                 patch("easy_multi_provider.server.generated_catalog_path",
                       return_value=Path(directory) / "catalog.json"):
                state.discover_provider_models("custom", ["nested-model-2"])
            model = load(path)["models"][0]
            self.assertEqual(
                model["capability_sources"]["structured_output"]["source"],
                "advertised",
            )


class TestSourceRankPrecedence(unittest.TestCase):
    """manual outranks observed; observed cannot replace manual."""

    def test_observed_cannot_overwrite_manual(self):
        existing = {
            "context_window": 100000,
            "capability_sources": {
                "context_window": {
                    "source": "manual",
                    "confidence": 1.0,
                    "observed_at": None,
                }
            },
        }
        item = {"context_window": 200000}
        _merge_discovered_field(
            existing, item, "context_window", "observed",
            "2026-08-22T00:00:00+00:00",
        )
        self.assertEqual(existing["context_window"], 100000)
        self.assertEqual(
            existing["capability_sources"]["context_window"]["source"],
            "manual",
        )


class TestExplicitEmptyListProvenance(unittest.TestCase):
    def test_explicit_unknown_empty_list_updates_existing_unknown(self):
        existing = {
            "reasoning_levels": ["high"],
            "capability_sources": {
                "reasoning_levels": {"source": "unknown"},
            },
        }
        item = {
            "reasoning_levels": [],
            "capability_sources": {
                "reasoning_levels": {"source": "unknown"},
            },
        }
        _merge_discovered_field(
            existing,
            item,
            "reasoning_levels",
            "unknown",
            "2026-08-22T00:00:00+00:00",
        )
        self.assertEqual(existing["reasoning_levels"], [])
        self.assertEqual(
            existing["capability_sources"]["reasoning_levels"]["source"],
            "unknown",
        )

    def test_empty_list_without_provenance_does_not_erase_existing(self):
        existing = {
            "reasoning_levels": ["high"],
            "capability_sources": {
                "reasoning_levels": {"source": "unknown"},
            },
        }
        _merge_discovered_field(
            existing,
            {"reasoning_levels": []},
            "reasoning_levels",
            "advertised",
            "2026-08-22T00:00:00+00:00",
        )
        self.assertEqual(existing["reasoning_levels"], ["high"])

    def test_manual_can_replace_observed(self):
        existing = {
            "context_window": 200000,
            "capability_sources": {
                "context_window": {
                    "source": "observed",
                    "confidence": 1.0,
                    "observed_at": None,
                }
            },
        }
        item = {"context_window": 300000}
        _merge_discovered_field(
            existing, item, "context_window", "manual",
            "2026-08-22T00:00:00+00:00",
        )
        self.assertEqual(existing["context_window"], 300000)
        self.assertEqual(
            existing["capability_sources"]["context_window"]["source"],
            "manual",
        )


if __name__ == "__main__":
    unittest.main()
