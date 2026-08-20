"""Opt-in end-to-end test using the real Codex CLI and a fixed demo model."""

import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
import unittest
import uuid
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path

from tests.support import ensure_test_master_key
from easy_multi_provider.catalog import write_catalog
from easy_multi_provider.config import normalize, save
from easy_multi_provider.server import AppState, make_handler


ensure_test_master_key()


FIXED_REPLY = "EASY_MULTIPROVIDER_DEMO_OK"


def _event(name, payload, sequence):
    value = dict(payload)
    value.setdefault("type", name)
    value["sequence_number"] = sequence
    return "event: %s\ndata: %s\n\n" % (name, json.dumps(value))


def _fixed_response_stream(model):
    response_id = "resp_" + uuid.uuid4().hex
    message_id = "msg_" + uuid.uuid4().hex
    created_at = int(time.time())
    item = {
        "id": message_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": FIXED_REPLY, "annotations": []}],
    }
    base = {
        "id": response_id,
        "object": "response",
        "created_at": created_at,
        "status": "in_progress",
        "error": None,
        "incomplete_details": None,
        "instructions": None,
        "model": model,
        "output": [],
        "parallel_tool_calls": True,
        "tool_choice": "auto",
        "tools": [],
        "usage": None,
    }
    events = [
        _event("response.created", {"response": base}, 0),
        _event(
            "response.output_item.added",
            {"output_index": 0, "item": dict(item, status="in_progress", content=[])},
            1,
        ),
        _event(
            "response.content_part.added",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": {"type": "output_text", "text": "", "annotations": []},
            },
            2,
        ),
        _event(
            "response.output_text.delta",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "delta": FIXED_REPLY,
                "logprobs": [],
            },
            3,
        ),
        _event(
            "response.output_text.done",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "text": FIXED_REPLY,
                "logprobs": [],
            },
            4,
        ),
        _event(
            "response.content_part.done",
            {
                "item_id": message_id,
                "output_index": 0,
                "content_index": 0,
                "part": item["content"][0],
            },
            5,
        ),
        _event("response.output_item.done", {"output_index": 0, "item": item}, 6),
    ]
    completed = dict(base)
    completed.update(
        {
            "status": "completed",
            "completed_at": created_at,
            "output": [item],
            "usage": {
                "input_tokens": 1,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 1,
                "output_tokens_details": {"reasoning_tokens": 0},
                "total_tokens": 2,
            },
        }
    )
    events.append(_event("response.completed", {"response": completed}, 7))
    return "".join(events).encode("utf-8")


class FixedResponsesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, format, *args):
        pass

    def do_POST(self):
        length = int(self.headers.get("Content-Length", "0"))
        body = json.loads(self.rfile.read(length).decode("utf-8"))
        self.server.seen_models.append(body.get("model"))
        payload = _fixed_response_stream(body.get("model", "fixed-model"))
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)


@unittest.skipUnless(
    os.environ.get("EASY_MP_RUN_CODEX_CLI") == "1",
    "set EASY_MP_RUN_CODEX_CLI=1 to run the real Codex CLI demo",
)
class CodexCliDemoTests(unittest.TestCase):
    def test_real_codex_cli_uses_temporary_demo_model(self):
        codex = shutil.which("codex")
        self.assertIsNotNone(codex, "codex CLI is not installed")
        native_catalog = Path.home() / ".codex" / "models_cache.json"
        self.assertTrue(native_catalog.exists(), "Codex native model cache is unavailable")

        fake_server = ThreadingHTTPServer(("127.0.0.1", 0), FixedResponsesHandler)
        fake_server.seen_models = []
        fake_thread = threading.Thread(target=fake_server.serve_forever, daemon=True)
        fake_thread.start()
        router_server = None
        router_thread = None
        try:
            with tempfile.TemporaryDirectory(prefix="easy-mp-codex-demo-") as directory:
                root = Path(directory)
                config_path = root / "config.json"
                catalog_path = root / "catalog.json"
                output_path = root / "last-message.txt"
                config = normalize(
                    {
                        "host": "127.0.0.1",
                        "port": 4200,
                        "native_catalog_path": str(native_catalog),
                        "providers": [
                            {
                                "id": "demo",
                                "name": "Fixed Demo",
                                "base_url": "http://127.0.0.1:%d/v1" % fake_server.server_address[1],
                                "protocol": "responses",
                                "auth_mode": "api_key",
                                "api_key": "local-demo-key",
                            }
                        ],
                        "models": [
                            {
                                "id": "demo/fixed",
                                "provider": "demo",
                                "upstream_id": "fixed-model",
                                "display_name": "Fixed Demo Model",
                                "reasoning_levels": ["medium"],
                            }
                        ],
                    }
                )
                save(config, config_path)
                write_catalog(config, catalog_path)

                state = AppState(config_path)
                base_handler = make_handler(state)

                class TrackingHandler(base_handler):
                    def _serve_responses_websocket(self):
                        self.server.websocket_upgrades += 1
                        return super()._serve_responses_websocket()

                    def _websocket_events(self, metadata, result):
                        self.server.websocket_requests += 1
                        yield from super()._websocket_events(metadata, result)

                router_server = ThreadingHTTPServer(("127.0.0.1", 0), TrackingHandler)
                router_server.websocket_upgrades = 0
                router_server.websocket_requests = 0
                router_thread = threading.Thread(target=router_server.serve_forever, daemon=True)
                router_thread.start()
                router_url = "http://127.0.0.1:%d/v1" % router_server.server_address[1]

                command = [
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
                    "demo/fixed",
                    "-c",
                    'model_provider="openai"',
                    "-c",
                    'model_catalog_json="%s"' % str(catalog_path),
                    "-c",
                    'openai_base_url="%s"' % router_url,
                    "-c",
                    'model_reasoning_effort="medium"',
                    "-c",
                    'approval_policy="never"',
                    "-c",
                    'sandbox_mode="read-only"',
                    "Return the model response without modification.",
                ]
                completed = subprocess.run(
                    command,
                    cwd=str(root),
                    text=True,
                    stdout=subprocess.PIPE,
                    stderr=subprocess.PIPE,
                    timeout=60,
                )
                self.assertEqual(
                    completed.returncode,
                    0,
                    "Codex CLI failed\nstdout:\n%s\nstderr:\n%s"
                    % (completed.stdout, completed.stderr),
                )
                self.assertEqual(output_path.read_text(encoding="utf-8").strip(), FIXED_REPLY)
                self.assertEqual(fake_server.seen_models, ["fixed-model"])
                self.assertGreater(router_server.websocket_upgrades, 0)
                self.assertGreater(router_server.websocket_requests, 0)
        finally:
            if router_server is not None:
                router_server.shutdown()
                router_server.server_close()
            fake_server.shutdown()
            fake_server.server_close()


if __name__ == "__main__":
    unittest.main()
