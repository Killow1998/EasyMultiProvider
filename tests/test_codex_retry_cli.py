"""Opt-in acceptance of native retry behavior using an isolated Codex CLI."""

import json
import os
import ssl
import subprocess
import tempfile
import threading
import unittest
from http.server import ThreadingHTTPServer
from pathlib import Path
from unittest.mock import patch

from easy_multi_provider.catalog import write_catalog
from easy_multi_provider.config import normalize, save
from easy_multi_provider.server import AppState, make_handler
from tests.support import ensure_test_master_key
from tests.test_codex_cli_demo import FIXED_REPLY, _fixed_response_stream
from tests.test_native_websocket import _FakeConnection


@unittest.skipUnless(
    os.environ.get("EMP_CODEX_TEST_BINARY") and os.environ.get("EMP_CODEX_TEST_CATALOG"),
    "set EMP_CODEX_TEST_BINARY and EMP_CODEX_TEST_CATALOG for isolated official CLI test",
)
class CodexRetryCliTests(unittest.TestCase):
    def test_tls_failure_recovers_on_websocket_without_http_replay(self):
        ensure_test_master_key()

        class FixedConnection(_FakeConnection):
            def send(self, value):
                super().send(value)
                self.responses = [
                    json.loads(line[6:])
                    for line in _fixed_response_stream("gpt-6-astra").decode().splitlines()
                    if line.startswith("data: {")
                ]

        upstream = FixedConnection([])
        with tempfile.TemporaryDirectory(prefix="emp-native-retry-") as directory:
            root = Path(directory)
            isolated_home = root / "codex"
            isolated_home.mkdir()
            env = dict(os.environ, CODEX_HOME=str(isolated_home), EMP_RETRY_TEST_KEY="fixture-only",
                       OPENAI_API_KEY="fixture-only", HTTP_PROXY="http://127.0.0.1:1",
                       HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1",
                       NO_PROXY="127.0.0.1,localhost,::1")
            config = normalize({
                "native_catalog_path": str(Path(os.environ["EMP_CODEX_TEST_CATALOG"]).resolve()),
                "providers": [{"id": "native", "auth_mode": "forward", "protocol": "responses",
                               "base_url": "https://native.example/backend-api/codex"}],
                "models": [{"id": "native/gpt-6-astra", "provider": "native",
                            "upstream_id": "gpt-6-astra", "reasoning_levels": ["low"]}],
            })
            save(config, root / "emp.json")
            catalog = root / "catalog.json"
            write_catalog(config, catalog)
            state = AppState(root / "emp.json")
            server = ThreadingHTTPServer(("127.0.0.1", 0), make_handler(state))
            threading.Thread(target=server.serve_forever, daemon=True).start()
            provider = ('{name="Fixture", base_url="http://127.0.0.1:%d/v1", '
                        'wire_api="responses", env_key="EMP_RETRY_TEST_KEY", supports_websockets=true, '
                        'http_headers={"chatgpt-account-id"="account-fixture", Cookie=%s}}') % (
                            server.server_port, json.dumps("emp_session=" + state.session_token))
            command = [str(Path(os.environ["EMP_CODEX_TEST_BINARY"]).resolve()),
                       "exec", "--ignore-user-config", "--ephemeral", "--skip-git-repo-check",
                       "--color", "never", "-m", "native/gpt-6-astra",
                       "-c", 'model_provider="fixture"', "-c", "model_providers.fixture=" + provider,
                       "-c", "model_catalog_json=" + json.dumps(str(catalog)),
                       "-c", 'model_reasoning_effort="low"', "-c", 'web_search="disabled"',
                       "--output-last-message", str(root / "reply.txt"), "Say hello."]
            try:
                with patch("easy_multi_provider.native_websocket._default_connector",
                           side_effect=[ssl.SSLEOFError(8, "unexpected EOF"), upstream]) as connector, \
                        patch("easy_multi_provider.codex_dispatch.proxy",
                              side_effect=AssertionError("unexpected HTTP replay")) as http:
                    result = subprocess.run(command, env=env, cwd=root, input="", capture_output=True,
                                            text=True, encoding="utf-8", errors="replace", timeout=60)
                    self.assertEqual(result.returncode, 0, result.stderr[-4000:])
                    self.assertEqual((root / "reply.txt").read_text(encoding="utf-8").strip(), FIXED_REPLY)
                    self.assertEqual(connector.call_count, 2)
                    self.assertEqual(sum(item.get("generate") is not False for item in upstream.sent), 1)
                    self.assertEqual(state._native_websocket_cooldowns, {})
                    http.assert_not_called()
            finally:
                server.shutdown()
                server.server_close()
