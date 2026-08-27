"""Opt-in real Codex acceptance for one configured external model.

The test reads the model slug and config path from environment variables. It
never embeds provider names, endpoints, or credentials in source or output.
"""

import os
import shutil
import subprocess
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

import easy_multi_provider.router as router
from easy_multi_provider.catalog import write_catalog
from easy_multi_provider.config import api_key, load, normalize, save
from easy_multi_provider.route_plan import resolve_route
from easy_multi_provider.server import AppState, make_handler
from easy_multi_provider.vault import MASTER_KEY_ENV


if os.environ.get("EASY_MP_RUN_LIVE_EXTERNAL") == "1":
    # tests/__init__.py installs an ephemeral test key. Live acceptance must
    # instead use the checkout's local key file to read the selected provider.
    os.environ.pop(MASTER_KEY_ENV, None)


@unittest.skipUnless(
    os.environ.get("EASY_MP_RUN_LIVE_EXTERNAL") == "1",
    "set EASY_MP_RUN_LIVE_EXTERNAL=1 and EASY_MP_LIVE_MODEL to run",
)
class LiveExternalModelTests(unittest.TestCase):
    def test_real_codex_tool_and_web_round_trips(self):
        codex = shutil.which("codex")
        self.assertIsNotNone(codex, "codex CLI is not installed")
        model_slug = os.environ.get("EASY_MP_LIVE_MODEL", "").strip()
        self.assertTrue(model_slug, "EASY_MP_LIVE_MODEL is required")
        source_path = Path(os.environ.get("EASY_MP_LIVE_CONFIG", "config.json"))
        source = load(source_path)
        route = resolve_route(source, model_slug)
        provider, model = route.provider_copy(), route.model_copy()

        server = None
        thread = None
        with tempfile.TemporaryDirectory(prefix="easy-mp-live-external-") as directory:
            root = Path(directory)
            config_path = root / "config.json"
            catalog_path = root / "catalog.json"
            live_key = api_key(provider)
            self.assertTrue(live_key, "source provider credential is unavailable")
            isolated_provider = dict(provider)
            isolated_provider["api_key"] = ""
            isolated_provider["api_key_file"] = ""
            isolated = normalize(
                {
                    "host": "127.0.0.1",
                    "port": 4200,
                    "native_catalog_path": str(Path.home() / ".codex" / "models_cache.json"),
                    "providers": [isolated_provider],
                    "models": [model],
                }
            )
            save(isolated, config_path)
            write_catalog(isolated, catalog_path)
            state = AppState(config_path)
            state.config["providers"][0]["api_key"] = live_key
            self.assertTrue(
                api_key(state.snapshot()["providers"][0]),
                "isolated provider credential could not be decrypted",
            )
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            thread = threading.Thread(target=server.serve_forever, daemon=True)
            thread.start()
            base_url = "http://127.0.0.1:%d/v1" % server.server_address[1]

            try:
                request_shapes = []
                real_request = router._request

                def tracking_request(provider_value, payload, incoming, stream=False, operation="", context_check=None):
                    source_items = payload.get("input")
                    source_items = source_items if isinstance(source_items, list) else []
                    tools = payload.get("tools")
                    tools = tools if isinstance(tools, list) else []
                    shape = {
                        "input_count": len(source_items),
                        "item_types": [
                            str(item.get("type") or "message")
                            for item in source_items
                            if isinstance(item, dict)
                        ],
                        "tool_count": len(tools),
                        "tool_types": [
                            str(item.get("type") or "unknown")
                            for item in tools
                            if isinstance(item, dict)
                        ],
                        "max_output_tokens": payload.get("max_output_tokens"),
                        "stream": bool(stream),
                    }
                    request_shapes.append(shape)
                    try:
                        return real_request(
                            provider_value,
                            payload,
                            incoming,
                            stream,
                            operation,
                            context_check,
                        )
                    except Exception as exc:
                        shape["error_type"] = type(exc).__name__
                        shape["error_status"] = getattr(exc, "status", None)
                        raise

                cases = (
                    (
                        "Use the exec tool to run the read-only command pwd exactly once. "
                        "Then reply with exactly EMP_EXTERNAL_TOOL_OK.",
                        "EMP_EXTERNAL_TOOL_OK",
                    ),
                    (
                        "Use the available web search tool with a harmless query for the "
                        "OpenAI Codex GitHub repository. Then reply with "
                        "exactly EMP_EXTERNAL_WEB_OK.",
                        "EMP_EXTERNAL_WEB_OK",
                    ),
                )
                with patch.object(router, "_request", side_effect=tracking_request):
                    for prompt, expected in cases:
                        output_path = root / (expected.lower() + ".txt")
                        completed = subprocess.run(
                            [
                                codex,
                                "exec",
                                "--ignore-user-config",
                                "--ephemeral",
                                "--skip-git-repo-check",
                                "--color",
                                "never",
                                "--output-last-message",
                                str(output_path),
                                "-m",
                                model_slug,
                                "-c",
                                'model_provider="openai"',
                                "-c",
                                'model_catalog_json="%s"' % catalog_path,
                                "-c",
                                'openai_base_url="%s"' % base_url,
                                "-c",
                                'approval_policy="never"',
                                "-c",
                                'sandbox_mode="read-only"',
                                prompt,
                            ],
                            cwd=str(root),
                            text=True,
                            stdout=subprocess.PIPE,
                            stderr=subprocess.PIPE,
                            timeout=240,
                        )
                        self.assertEqual(
                            completed.returncode,
                            0,
                            "live external Codex acceptance failed\nshapes:\n%s\nstdout:\n%s\nstderr:\n%s"
                            % (request_shapes, completed.stdout, completed.stderr),
                        )
                        self.assertEqual(
                            output_path.read_text(encoding="utf-8").strip(), expected
                        )
            finally:
                server.shutdown()
                server.server_close()
                thread.join(timeout=5)


if __name__ == "__main__":
    unittest.main()
