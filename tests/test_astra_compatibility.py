"""Astra catalog boundaries and opt-in real CLI protocol acceptance."""

import copy
import io
import json
import os
import subprocess
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.catalog import _external_entry, build_catalog, write_catalog
from easy_multi_provider.config import normalize, save
from easy_multi_provider.dialects import project_request
from easy_multi_provider.official_registry import enrich_discovered_models
from easy_multi_provider.server import AppState, make_handler
from tests.test_codex_cli_demo import FixedResponsesHandler, FIXED_REPLY


class AstraCompatibilityTests(unittest.TestCase):
    def test_api_metadata_does_not_invent_codex_ultra(self):
        models = enrich_discovered_models(
            {"base_url": "https://api.openai.com/v1"}, [{"id": "gpt-6-astra"}]
        )
        self.assertEqual(models[0]["context_window"], 1050000)
        self.assertEqual(models[0]["output_limit"], 128000)
        self.assertEqual(models[0]["reasoning_levels"], ["low", "medium", "high", "xhigh", "max"])

    def test_external_models_do_not_inherit_native_routing_capabilities(self):
        template = {"service_tiers": [{"id": "priority"}], "additional_speed_tiers": ["fast"],
                    "default_service_tier": "priority", "use_responses_lite": True,
                    "experimental_supported_tools": ["send_user_message_async", "clock"],
                    "multi_agent_reasoning_effort": "xhigh", "comp_hash": "3000",
                    "minimal_client_version": "0.153.0", "available_in_plans": ["pro"]}
        original = copy.deepcopy(template)
        model = _external_entry({"id": "other/model"}, template, {"protocol": "responses"})
        self.assertEqual(template, original)
        self.assertEqual(model["service_tiers"], [])
        self.assertEqual(model["experimental_supported_tools"], [])
        self.assertIsNone(model["default_service_tier"])
        self.assertFalse(model["use_responses_lite"])
        self.assertNotIn("comp_hash", model)
        self.assertNotIn("minimal_client_version", model)

    def test_native_catalog_preserves_astra_metadata_and_user_presentation(self):
        native = {"slug": "gpt-6-astra", "display_name": "GPT-6-Astra",
                  "context_window": 272000, "max_context_window": 872000,
                  "supported_reasoning_levels": [{"effort": "ultra", "description": "Delegation"}],
                  "service_tiers": [{"id": "priority", "name": "Fast"}],
                  "experimental_supported_tools": ["send_user_message_async"],
                  "model_messages": {"auto_review": {"node_repl_policy": "fixture"}}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "models.json"
            path.write_text(json.dumps({"models": [native]}), encoding="utf-8")
            config = normalize({"native_catalog_path": str(path),
                                "catalog_family_presentations": {"gpt-6-astra": {
                                    "catalog_alias": "Astra coding", "show_context": False}}})
            result = build_catalog(config)["models"][0]
        self.assertEqual(result["display_name"], "Astra coding")
        for key in ("context_window", "max_context_window", "supported_reasoning_levels",
                    "service_tiers", "experimental_supported_tools", "model_messages"):
            self.assertEqual(result[key], native[key])

    def test_portable_responses_keeps_explicit_service_tier(self):
        for tier in ("default", "priority", "fast", "ultrafast"):
            body = {"model": "gpt-6-astra", "input": "hello", "service_tier": tier}
            projected = project_request({"protocol": "responses", "auth_mode": "api_key"}, body)
            self.assertEqual(projected["service_tier"], tier)
        projected = project_request({"protocol": "responses", "auth_mode": "api_key"}, {"input": "hello"})
        self.assertNotIn("service_tier", projected)


@unittest.skipUnless(os.environ.get("EMP_CODEX_TEST_BINARY") and os.environ.get("EMP_CODEX_TEST_CATALOG"),
                     "set EMP_CODEX_TEST_BINARY and EMP_CODEX_TEST_CATALOG for isolated official CLI test")
class RealAstraCliTests(unittest.TestCase):
    def test_catalog_and_standard_fast_responses(self):
        binary = str(Path(os.environ["EMP_CODEX_TEST_BINARY"]).resolve())
        source = str(Path(os.environ["EMP_CODEX_TEST_CATALOG"]).resolve())
        class TierHandler(FixedResponsesHandler):
            def do_POST(self):
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                self.server.seen_tiers.append(json.loads(raw).get("service_tier"))
                self.rfile = io.BytesIO(raw)
                super().do_POST()

        upstream = ThreadingHTTPServer(("127.0.0.1", 0), TierHandler)
        upstream.seen_models = []
        upstream.seen_tiers = []
        threading.Thread(target=upstream.serve_forever, daemon=True).start()
        self.addCleanup(upstream.server_close)
        self.addCleanup(upstream.shutdown)
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            env = dict(os.environ, CODEX_HOME=str(root / "codex"), OPENAI_API_KEY="fixture-only")
            Path(env["CODEX_HOME"]).mkdir()
            (Path(env["CODEX_HOME"]) / "auth.json").write_text(json.dumps({
                "auth_mode": "apikey", "OPENAI_API_KEY": "fixture-only",
                "access_token": "fixture-only",
            }), encoding="utf-8")
            with patch.dict(os.environ, env):
                config = normalize({"native_catalog_path": source, "providers": [{
                    "id": "fixture", "base_url": "http://127.0.0.1:%d/v1" % upstream.server_port,
                    "protocol": "responses", "auth_mode": "api_key", "api_key": "fixture-only"}],
                    "models": [{"id": "fixture/gpt-6-astra", "provider": "fixture",
                                "upstream_id": "gpt-6-astra", "reasoning_levels": ["low", "max"]}]})
                save(config, root / "emp.json")
                catalog = root / "catalog.json"
                write_catalog(config, catalog)
                # This controlled upstream explicitly supports Fast. Generic
                # external entries correctly have no inherited native tiers.
                advertised = json.loads(catalog.read_text(encoding="utf-8"))
                fixture = next(item for item in advertised["models"] if item["slug"] == "fixture/gpt-6-astra")
                fixture["service_tiers"] = [{"id": "priority", "name": "Fast", "description": "Test tier"}]
                fixture["additional_speed_tiers"] = ["fast"]
                catalog.write_text(json.dumps(advertised), encoding="utf-8")
                state = AppState(root / "emp.json")
                server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
                threading.Thread(target=server.serve_forever, daemon=True).start()
                try:
                    for tier in ("default", "priority"):
                        command = [binary, "exec", "--ignore-user-config", "--ephemeral",
                                   "--skip-git-repo-check", "--color", "never", "-m", "fixture/gpt-6-astra",
                                   "-c", 'model_provider="openai"', "-c", 'model_catalog_json=' + json.dumps(str(catalog)),
                                   "-c", 'openai_base_url="http://127.0.0.1:%d/v1"' % server.server_port,
                                   "-c", 'service_tier=' + json.dumps(tier),
                                   "-c", 'model_reasoning_effort="low"',
                                   "-c", 'web_search="disabled"',
                                   "--output-last-message", str(root / "reply.txt"), "Say hello."]
                        result = subprocess.run(command, env=env, cwd=root, input="", capture_output=True,
                                                text=True, encoding="utf-8", errors="replace", timeout=60)
                        self.assertEqual(result.returncode, 0, result.stderr[-4000:])
                        self.assertEqual((root / "reply.txt").read_text(encoding="utf-8").strip(), FIXED_REPLY)
                    self.assertEqual(upstream.seen_models, ["gpt-6-astra", "gpt-6-astra"])
                    self.assertEqual(upstream.seen_tiers, [None, "priority"])
                finally:
                    server.shutdown()
                    server.server_close()
