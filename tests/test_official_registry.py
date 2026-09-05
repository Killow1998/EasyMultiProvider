"""Tests for easy_multi_provider.official_registry.

Uses a tempfile-based fixture registry for schema/validation tests and the
release-bundled data file for integration tests.  No network connections.
"""

import copy
import json
import os
import tempfile
import unittest

from easy_multi_provider.official_registry import (
    RegistryError,
    enrich_discovered_models,
    identify_provider,
    load_registry,
)


def _make_registry():
    """Build a minimal valid registry for tests."""
    return {
        "schema_version": 1,
        "reviewed_at": "2026-08-22",
        "providers": [
            {
                "key": "openai",
                "display_name": "OpenAI",
                "role": ["api_provider", "publisher"],
                "api_base_urls": ["https://api.openai.com/v1"],
                "auth_modes": ["bearer_api_key"],
                "supported_protocols": ["responses", "chat_completions"],
                "preferred_protocol": "responses",
                "model_list_endpoint": "https://api.openai.com/v1/models",
                "sources": ["https://developers.openai.com/api/docs/models"],
                "reviewed_at": "2026-08-22",
            },
            {
                "key": "zhipu_glm",
                "display_name": "Zhipu AI / GLM",
                "role": ["api_provider", "publisher"],
                "api_base_urls": ["https://open.bigmodel.cn/api/paas/v4"],
                "auth_modes": ["bearer_api_key"],
                "supported_protocols": ["chat_completions"],
                "preferred_protocol": "chat_completions",
                "model_list_endpoint": None,
                "sources": ["https://docs.z.ai/guides/overview/overview"],
                "reviewed_at": "2026-08-22",
            },
        ],
        "models": [
            {
                "provider_key": "openai",
                "model_id": "gpt-5.6-sol",
                "display_name": "GPT-5.6 Sol",
                "context_window": 1050000,
                "max_input_tokens": 922000,
                "max_output_tokens": 128000,
                "input_modalities": ["text", "image"],
                "output_modalities": ["text"],
                "supports_reasoning": True,
                "reasoning_levels": ["none", "low", "medium", "high", "xhigh", "max"],
                "reasoning_control": "reasoning.effort enum",
                "tool_calling": True,
                "parallel_tool_calling": False,
                "structured_output": True,
                "streaming": True,
                "web_search": True,
                "protocols": ["responses", "chat_completions"],
                "aliases": ["gpt-5.6"],
                "sources_by_field": {
                    "all": [
                        "https://developers.openai.com/api/docs/models/gpt-5.6-sol"
                    ]
                },
            },
            {
                "provider_key": "zhipu_glm",
                "model_id": "glm-4.6",
                "display_name": "GLM-4.6",
                "context_window": 200000,
                "max_output_tokens": 131072,
                "input_modalities": ["text"],
                "output_modalities": ["text"],
                "supports_reasoning": True,
                "tool_calling": True,
                "streaming": True,
                "protocols": ["chat_completions"],
                "aliases": [],
                "sources_by_field": {
                    "all": ["https://docs.z.ai/guides/overview/overview"]
                },
            },
        ],
    }


class _TempRegistryFixture(unittest.TestCase):
    """Base class that writes a registry to a temp file for each test."""

    def setUp(self):
        self._tmpdir = tempfile.mkdtemp(prefix="emp_registry_test_")
        self.registry_path = os.path.join(self._tmpdir, "official_models.json")
        self.registry_data = _make_registry()
        with open(self.registry_path, "w", encoding="utf-8") as f:
            json.dump(self.registry_data, f)

    def tearDown(self):
        import shutil

        shutil.rmtree(self._tmpdir, ignore_errors=True)

    def load(self):
        return load_registry(self.registry_path)

    def _write_bad(self, name, data):
        path = os.path.join(self._tmpdir, name)
        with open(path, "w", encoding="utf-8") as f:
            json.dump(data, f)
        return path


class TestLoadRegistry(_TempRegistryFixture):
    def test_load_valid_registry(self):
        reg = self.load()
        self.assertEqual(reg["schema_version"], 1)
        self.assertEqual(len(reg["providers"]), 2)
        self.assertEqual(len(reg["models"]), 2)

    def test_missing_file_raises(self):
        with self.assertRaises(RegistryError):
            load_registry(os.path.join(self._tmpdir, "missing.json"))

    def test_invalid_json_raises(self):
        path = os.path.join(self._tmpdir, "bad.json")
        with open(path, "w", encoding="utf-8") as f:
            f.write("{not valid json")
        with self.assertRaises(RegistryError):
            load_registry(path)

    def test_missing_top_level_key_raises(self):
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("no_models.json", {"schema_version": 1, "providers": []}))

    def test_non_dict_root_raises(self):
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("list_root.json", []))

    def test_null_providers_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["providers"] = None
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("null_providers.json", bad))

    def test_null_models_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"] = None
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("null_models.json", bad))

    def test_null_provider_entry_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["providers"].append(None)
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("null_provider.json", bad))

    def test_null_model_entry_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"].append(None)
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("null_model.json", bad))

    def test_duplicate_provider_key_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["providers"].append({
            "key": "openai",
            "display_name": "dup",
            "api_base_urls": [],
        })
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("dup_provider.json", bad))

    def test_unknown_model_provider_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["provider_key"] = "not_registered"
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("unknown_model_provider.json", bad))

    def test_duplicate_model_id_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"].append(copy.deepcopy(bad["models"][0]))
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("dup_model.json", bad))

    def test_duplicate_alias_raises(self):
        bad = copy.deepcopy(self.registry_data)
        duplicate = copy.deepcopy(bad["models"][0])
        duplicate["model_id"] = "another-model"
        duplicate["aliases"] = ["gpt-5.6"]
        bad["models"].append(duplicate)
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("dup_alias.json", bad))

    def test_alias_conflicting_with_model_id_raises(self):
        bad = copy.deepcopy(self.registry_data)
        conflict = copy.deepcopy(bad["models"][0])
        conflict["model_id"] = "another-openai-model"
        conflict["aliases"] = ["gpt-5.6-sol"]
        bad["models"].append(conflict)
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("alias_conflict.json", bad))

    def test_missing_capability_source_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["sources_by_field"] = {}
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("missing_source.json", bad))

    def test_non_https_capability_source_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["sources_by_field"] = {
            "all": ["http://developers.openai.com/api/docs/models/gpt-5.6-sol"]
        }
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("http_source.json", bad))

    def test_empty_capability_source_list_raises(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["sources_by_field"] = {"all": []}
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("empty_source.json", bad))


class TestSchemaValidation(_TempRegistryFixture):
    def test_schema_version_must_be_positive_int(self):
        bad = copy.deepcopy(self.registry_data)
        bad["schema_version"] = 0
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("zero_schema.json", bad))

    def test_schema_version_must_be_int_not_bool(self):
        bad = copy.deepcopy(self.registry_data)
        bad["schema_version"] = True
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("bool_schema.json", bad))

    def test_unsupported_schema_version_rejected(self):
        bad = copy.deepcopy(self.registry_data)
        bad["schema_version"] = 2
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("v2_schema.json", bad))

    def test_reviewed_at_must_be_non_empty_string(self):
        bad = copy.deepcopy(self.registry_data)
        bad["reviewed_at"] = ""
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("empty_reviewed.json", bad))

    def test_reviewed_at_must_be_iso_date(self):
        bad = copy.deepcopy(self.registry_data)
        bad["reviewed_at"] = "not-a-date"
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("bad_reviewed.json", bad))

    def test_all_sources_by_field_entry_validated(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["sources_by_field"]["all"] = ["http://not-https.com"]
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("http_all.json", bad))

    def test_invalid_url_in_field_source_rejected(self):
        bad = copy.deepcopy(self.registry_data)
        bad["models"][0]["sources_by_field"]["context_window"] = ["not-a-url"]
        with self.assertRaises(RegistryError):
            load_registry(self._write_bad("bad_url.json", bad))


class TestIdentifyProvider(_TempRegistryFixture):
    def test_exact_root_url_match(self):
        provider = {"base_url": "https://api.openai.com/v1"}
        self.assertEqual(identify_provider(provider, self.load()), "openai")

    def test_trailing_slash_matches(self):
        provider = {"base_url": "https://api.openai.com/v1/"}
        self.assertEqual(identify_provider(provider, self.load()), "openai")

    def test_userinfo_is_rejected(self):
        provider = {"base_url": "https://user:secret@api.openai.com/v1"}
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_query_is_rejected(self):
        provider = {"base_url": "https://api.openai.com/v1?version=1"}
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_fragment_is_rejected(self):
        provider = {"base_url": "https://api.openai.com/v1#models"}
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_wrong_path_returns_none(self):
        provider = {"base_url": "https://api.openai.com/v2"}
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_no_base_url_returns_none(self):
        self.assertIsNone(identify_provider({"id": "custom"}, self.load()))

    def test_non_dict_provider_returns_none(self):
        self.assertIsNone(identify_provider("not a dict", self.load()))

    def test_explicit_provider_with_registered_root(self):
        provider = {
            "base_url": "https://api.openai.com/v1/",
            "official_provider": "openai",
        }
        self.assertEqual(identify_provider(provider, self.load()), "openai")

    def test_explicit_unknown_key_is_rejected(self):
        provider = {
            "base_url": "https://api.openai.com/v1",
            "official_provider": "some_unknown",
        }
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_explicit_key_with_unregistered_url_is_rejected(self):
        provider = {
            "base_url": "https://my-proxy.example.com/v1",
            "official_provider": "openai",
        }
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_explicit_key_without_base_url_is_rejected(self):
        provider = {"official_provider": "openai"}
        self.assertIsNone(identify_provider(provider, self.load()))

    def test_zhipu_root_match(self):
        provider = {"base_url": "https://open.bigmodel.cn/api/paas/v4"}
        self.assertEqual(identify_provider(provider, self.load()), "zhipu_glm")


class TestEnrichDiscoveredModels(_TempRegistryFixture):
    provider = {"base_url": "https://api.openai.com/v1"}

    def test_fills_missing_fields_and_projects(self):
        models = [{"upstream_id": "gpt-5.6-sol"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        m = result[0]
        self.assertEqual(m["context_window"], 1050000)
        self.assertEqual(m["input_modalities"], ["text", "image"])
        self.assertEqual(m["output_modalities"], ["text"])
        self.assertEqual(m["reasoning_levels"], ["none", "low", "medium", "high", "xhigh", "max"])
        self.assertEqual(m["output_limit"], 128000)
        self.assertNotIn("max_output_tokens", m)
        self.assertEqual(m["supported_protocols"], ["responses", "chat_completions"])
        self.assertNotIn("protocols", m)
        self.assertEqual(m["capabilities"]["structured_tools"], True)
        self.assertEqual(m["capabilities"]["parallel_tools"], False)
        self.assertEqual(m["capabilities"]["streaming"], True)
        self.assertEqual(m["capabilities"]["structured_output"], True)

    def test_official_provenance_shape(self):
        models = [{"upstream_id": "gpt-5.6-sol"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        expected = {
            "source": "official",
            "confidence": 0.95,
            "observed_at": "2026-08-22",
        }
        sources = result[0]["capability_sources"]
        self.assertEqual(sources["context_window"], expected)
        self.assertEqual(sources["output_limit"], expected)
        self.assertEqual(sources["structured_tools"], expected)
        self.assertIsInstance(sources["structured_tools"], dict)

    def test_upstream_id_takes_priority(self):
        models = [{"upstream_id": "gpt-5.6-sol", "id": "live-display-id"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 1050000)

    def test_model_id_fallback(self):
        models = [{"model_id": "gpt-5.6-sol"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 1050000)

    def test_id_fallback(self):
        models = [{"id": "gpt-5.6-sol"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 1050000)

    def test_advertised_value_wins(self):
        models = [{"upstream_id": "gpt-5.6-sol", "context_window": 500000}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 500000)
        self.assertNotIn("context_window", result[0]["capability_sources"])

    def test_empty_list_and_string_are_missing(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": [],
            "reasoning_control": "",
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])
        self.assertEqual(result[0]["reasoning_control"], "reasoning.effort enum")

    def test_nested_advertised_value_wins(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "capabilities": {"streaming": False},
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertFalse(result[0]["capabilities"]["streaming"])
        self.assertNotIn("streaming", result[0]["capability_sources"])

    def test_nested_missing_value_is_filled(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "capabilities": {"streaming": None, "other": True},
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertTrue(result[0]["capabilities"]["streaming"])
        self.assertTrue(result[0]["capabilities"]["other"])

    def test_false_is_a_valid_value(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "web_search": False,
            "capabilities": {"structured_tools": False},
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertFalse(result[0]["web_search"])
        self.assertFalse(result[0]["capabilities"]["structured_tools"])

    def test_input_is_not_mutated(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "capabilities": {"streaming": None},
        }]
        original = copy.deepcopy(models)
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(models, original)
        self.assertIsNot(result, models)
        self.assertIsNot(result[0], models[0])

    def test_unknown_model_is_not_enriched(self):
        models = [{"upstream_id": "some-future-model", "context_window": None}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertIsNone(result[0]["context_window"])
        self.assertNotIn("capability_sources", result[0])

    def test_other_provider_model_is_not_enriched(self):
        models = [{"upstream_id": "glm-4.6", "context_window": None}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertIsNone(result[0]["context_window"])

    def test_custom_provider_is_not_enriched(self):
        provider = {"base_url": "https://my-proxy.example.com/v1"}
        models = [{"upstream_id": "gpt-5.6-sol", "context_window": None}]
        result = enrich_discovered_models(provider, models, self.load())
        self.assertIsNone(result[0]["context_window"])
        self.assertNotIn("capability_sources", result[0])

    def test_alias_match(self):
        models = [{"upstream_id": "gpt-5.6"}]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 1050000)

    def test_empty_models_list(self):
        self.assertEqual(enrich_discovered_models(self.provider, [], self.load()), [])

    def test_unknown_source_is_overridden_by_official(self):
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
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])
        self.assertEqual(
            result[0]["capability_sources"]["input_modalities"]["source"],
            "official",
        )

    def test_inferred_source_is_overridden_by_official(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "inferred",
                    "confidence": 0.35,
                    "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])

    def test_official_source_is_overridden_by_official(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "context_window": 500000,
            "capability_sources": {
                "context_window": {
                    "source": "official",
                    "confidence": 0.95,
                    "observed_at": "2026-01-01",
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["context_window"], 1050000)

    def test_advertised_source_is_not_overridden(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "advertised",
                    "confidence": 0.75,
                    "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text"])

    def test_observed_source_is_not_overridden(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "observed",
                    "confidence": 1.0,
                    "observed_at": "2026-08-22",
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text"])

    def test_manual_source_is_not_overridden(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {
                "input_modalities": {
                    "source": "manual",
                    "confidence": 1.0,
                    "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text"])

    def test_live_value_without_provenance_is_kept(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text"])

    def test_nested_unknown_source_is_overridden(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "capabilities": {"streaming": False},
            "capability_sources": {
                "streaming": {
                    "source": "unknown",
                    "confidence": 0.0,
                    "observed_at": None,
                }
            },
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertTrue(result[0]["capabilities"]["streaming"])

    def test_string_source_unknown_is_overridden(self):
        models = [{
            "upstream_id": "gpt-5.6-sol",
            "input_modalities": ["text"],
            "capability_sources": {"input_modalities": "unknown"},
        }]
        result = enrich_discovered_models(self.provider, models, self.load())
        self.assertEqual(result[0]["input_modalities"], ["text", "image"])


class TestBundledRegistry(unittest.TestCase):
    """Tests against the release-bundled official_models.json."""

    def setUp(self):
        self.registry = load_registry()

    def test_bundled_loads_without_error(self):
        self.assertIsInstance(self.registry, dict)
        self.assertIn("providers", self.registry)
        self.assertIn("models", self.registry)

    def test_has_nine_provider_keys(self):
        keys = {p["key"] for p in self.registry["providers"]}
        expected = {
            "openai",
            "google_gemini",
            "openrouter",
            "anthropic",
            "xai",
            "meta",
            "moonshot",
            "deepseek",
            "zhipu_glm",
        }
        self.assertEqual(keys, expected)

    def test_no_null_provider_entries(self):
        for p in self.registry["providers"]:
            self.assertIsInstance(p, dict)
            self.assertTrue(p.get("key"))

    def test_no_null_model_entries(self):
        for m in self.registry["models"]:
            self.assertIsInstance(m, dict)
            self.assertTrue(m.get("model_id"))
            self.assertTrue(m.get("provider_key"))

    def test_openrouter_has_no_static_models(self):
        or_models = [
            m for m in self.registry["models"]
            if m["provider_key"] == "openrouter"
        ]
        self.assertEqual(or_models, [])

    def test_meta_has_no_api_roots(self):
        meta = next(
            p for p in self.registry["providers"] if p["key"] == "meta"
        )
        self.assertEqual(meta["api_base_urls"], [])

    def test_meta_provider_does_not_auto_match(self):
        result = identify_provider(
            {"base_url": "https://api.llama.com/v1"}, self.registry
        )
        self.assertIsNone(result)

    def test_all_capability_sources_are_https(self):
        for m in self.registry["models"]:
            sources = m.get("sources_by_field", {})
            for field, urls in sources.items():
                if field == "all":
                    continue
                if not isinstance(urls, list):
                    continue
                for url in urls:
                    self.assertTrue(
                        url.startswith("https://"),
                        msg="%s/%s.%s has non-HTTPS source: %s"
                        % (m["provider_key"], m["model_id"], field, url),
                    )

    def test_all_non_null_fields_have_sources(self):
        cap_fields = [
            "context_window", "max_input_tokens", "max_output_tokens",
            "input_modalities", "output_modalities", "supports_reasoning",
            "reasoning_levels", "reasoning_control", "tool_calling",
            "parallel_tool_calling", "structured_output", "streaming",
            "web_search", "protocols",
        ]
        for m in self.registry["models"]:
            sources = m.get("sources_by_field", {})
            for field in cap_fields:
                val = m.get(field)
                if val is None:
                    continue
                if isinstance(val, (list, str)) and not val:
                    continue
                has_src = (
                    (isinstance(sources.get(field), list) and sources[field])
                    or (isinstance(sources.get("all"), list) and sources["all"])
                )
                self.assertTrue(
                    has_src,
                    msg="%s/%s.%s has no source"
                    % (m["provider_key"], m["model_id"], field),
                )

    def test_google_gemini_has_required_models(self):
        gm_models = {
            m["model_id"]
            for m in self.registry["models"]
            if m["provider_key"] == "google_gemini"
        }
        for required in (
            "gemini-3.7-flash",
            "gemini-3.6-flash",
            "gemini-2.5-flash",
            "gemini-2.5-pro",
        ):
            self.assertIn(required, gm_models)

    def test_meta_has_llama4_models(self):
        meta_models = {
            m["model_id"]
            for m in self.registry["models"]
            if m["provider_key"] == "meta"
        }
        self.assertIn("llama-4-scout", meta_models)
        self.assertIn("llama-4-maverick", meta_models)

    def test_each_provider_has_a_bounded_curated_model_set(self):
        from collections import Counter

        counts = Counter(m["provider_key"] for m in self.registry["models"])
        for key, count in counts.items():
            if key == "openrouter":
                self.assertEqual(count, 0)
            else:
                self.assertGreaterEqual(
                    count, 2, msg="%s has only %d models" % (key, count)
                )
                self.assertLessEqual(
                    count, 5 if key == "openai" else 4,
                    msg="%s has %d models" % (key, count)
                )


    def test_grok_build_0_1_has_text_image_input(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "xai" and m["model_id"] == "grok-build-0.1"
        )
        self.assertEqual(m["input_modalities"], ["text", "image"])

    def test_grok_build_0_1_cites_exact_model_page(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "xai" and m["model_id"] == "grok-build-0.1"
        )
        urls = m["sources_by_field"]["input_modalities"]
        self.assertIn("https://docs.x.ai/developers/models/grok-build-0.1", urls)

    def test_grok_build_0_1_has_confirmed_aliases(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "xai" and m["model_id"] == "grok-build-0.1"
        )
        self.assertIn("grok-code-fast-1", m.get("aliases", []))

    def test_gemini_2_5_uses_thinking_budget_not_level(self):
        for mid in ("gemini-2.5-flash", "gemini-2.5-pro"):
            m = next(
                m for m in self.registry["models"]
                if m["provider_key"] == "google_gemini" and m["model_id"] == mid
            )
            self.assertIsNone(m["reasoning_levels"])
            self.assertIn("thinkingBudget", m["reasoning_control"])

    def test_gemini_2_5_flash_pro_descriptions_differ(self):
        flash = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "google_gemini" and m["model_id"] == "gemini-2.5-flash"
        )
        pro = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "google_gemini" and m["model_id"] == "gemini-2.5-pro"
        )
        # Flash: 0..24576, 0 disables
        self.assertIn("24576", flash["reasoning_control"])
        self.assertIn("0 disables", flash["reasoning_control"])
        # Pro: 128..32768, cannot be disabled
        self.assertIn("32768", pro["reasoning_control"])
        self.assertIn("cannot be disabled", pro["reasoning_control"])
        # Descriptions must differ
        self.assertNotEqual(flash["reasoning_control"], pro["reasoning_control"])

    def test_gemini_2_5_preserves_audio_video_input(self):
        for mid in ("gemini-2.5-flash", "gemini-2.5-pro"):
            m = next(
                m for m in self.registry["models"]
                if m["provider_key"] == "google_gemini" and m["model_id"] == mid
            )
            self.assertIn("audio", m["input_modalities"])
            self.assertIn("video", m["input_modalities"])

    def test_glm_5_1_cites_exact_guide(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "zhipu_glm" and m["model_id"] == "glm-5.1"
        )
        urls = m["sources_by_field"].get("reasoning", [])
        self.assertIn("https://docs.z.ai/guides/llm/glm-5.1", urls)

    def test_kimi_k3_max_output_tokens_is_null(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "moonshot" and m["model_id"] == "kimi-k3"
        )
        self.assertIsNone(m["max_output_tokens"])

    def test_kimi_k3_has_reasoning_facts(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "moonshot" and m["model_id"] == "kimi-k3"
        )
        self.assertTrue(m["supports_reasoning"])
        self.assertEqual(m["reasoning_levels"], ["low", "high", "max"])

    def test_gpt_5_6_max_input_tokens_is_null(self):
        for mid in ("gpt-5.6-sol", "gpt-5.6-luna"):
            m = next(
                m for m in self.registry["models"]
                if m["provider_key"] == "openai" and m["model_id"] == mid
            )
            self.assertIsNone(m["max_input_tokens"])

    def test_deepseek_has_anthropic_root(self):
        p = next(
            p for p in self.registry["providers"] if p["key"] == "deepseek"
        )
        self.assertIn("https://api.deepseek.com/anthropic", p["api_base_urls"])

    def test_deepseek_no_x_api_key(self):
        p = next(
            p for p in self.registry["providers"] if p["key"] == "deepseek"
        )
        self.assertNotIn("x-api-key", p["auth_modes"])

    def test_zhipu_has_z_ai_root(self):
        p = next(
            p for p in self.registry["providers"] if p["key"] == "zhipu_glm"
        )
        self.assertIn("https://api.z.ai/api/paas/v4", p["api_base_urls"])


    def test_zhipu_bigmodel_cn_has_first_party_source(self):
        prov = next(
            p for p in self.registry["providers"] if p["key"] == "zhipu_glm"
        )
        self.assertIn("https://open.bigmodel.cn/api/paas/v4", prov["api_base_urls"])
        self.assertIn(
            "https://docs.bigmodel.cn/cn/api/introduction",
            prov["sources"],
        )
    def test_kimi_k2_6_has_video_input(self):
        m = next(
            m for m in self.registry["models"]
            if m["provider_key"] == "moonshot" and m["model_id"] == "kimi-k2.6"
        )
        self.assertIn("video", m["input_modalities"])

    def test_gemini_auth_modes_distinguish_native(self):
        p = next(
            p for p in self.registry["providers"] if p["key"] == "google_gemini"
        )
        self.assertIn("x-goog-api-key", p["auth_modes"])
        self.assertIn("bearer_api_key", p["auth_modes"])

    def test_openrouter_static_model_rejected(self):
        bad = copy.deepcopy(self.registry)
        bad["models"].append({
            "provider_key": "openrouter",
            "model_id": "should-not-exist",
            "sources_by_field": {"all": ["https://openrouter.ai/docs"]},
        })
        with tempfile.NamedTemporaryFile(
            mode="w", suffix=".json", delete=False
        ) as tmp:
            json.dump(bad, tmp)
            tmp_path = tmp.name
        with self.assertRaises(RegistryError):
            load_registry(tmp_path)

    def test_deepseek_anthropic_root_identifies_provider(self):
        result = identify_provider(
            {"base_url": "https://api.deepseek.com/anthropic"},
            self.registry,
        )
        self.assertEqual(result, "deepseek")

    def test_zhipu_z_ai_root_identifies_provider(self):
        result = identify_provider(
            {"base_url": "https://api.z.ai/api/paas/v4"},
            self.registry,
        )
        self.assertEqual(result, "zhipu_glm")


class TestPackageDataDeclaration(unittest.TestCase):
    def test_pyproject_includes_data_json(self):
        project_root = os.path.dirname(os.path.dirname(__file__))
        toml_path = os.path.join(project_root, "pyproject.toml")
        with open(toml_path, "r", encoding="utf-8") as f:
            content = f.read()
        self.assertIn("data/*.json", content)


if __name__ == "__main__":
    unittest.main()
