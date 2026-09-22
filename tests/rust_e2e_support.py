"""Isolated real-process and loopback fixtures for Python/Rust endpoint tests."""
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import threading
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

import zstandard
from tests.test_server import _integration_test_config

ROOT = Path(__file__).resolve().parents[1]
GENERATED_ID = re.compile(r"^(resp|msg|rs|fc)_[0-9a-f]{32}$")


def normalized_ids(value):
    """Keep ID relationships and all other fields; normalize only random UUIDs."""
    identities = {}

    def visit(item):
        if isinstance(item, str) and GENERATED_ID.fullmatch(item):
            if item not in identities:
                identities[item] = f"{item.split('_', 1)[0]}_generated_{len(identities)}"
            return identities[item]
        if isinstance(item, list):
            return [visit(child) for child in item]
        if isinstance(item, dict):
            return {key: visit(child) for key, child in item.items()}
        return item

    return visit(value)


class Upstream:
    def __init__(self):
        self.requests = queue.Queue()
        self.reply = (200, "application/json", b"{}", {})

        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                if self.headers.get("Content-Encoding") == "zstd":
                    raw = zstandard.ZstdDecompressor().decompress(raw)
                owner.requests.put((self.path, dict(self.headers), json.loads(raw)))
                status, content_type, body, headers = owner.reply
                self.send_response(status)
                self.send_header("Content-Type", content_type)
                self.send_header("Content-Length", str(len(body)))
                self.send_header("Connection", "close")
                for key, value in headers.items():
                    self.send_header(key, value)
                self.end_headers()
                try:
                    # Split SSE frames on the network; event boundaries must survive.
                    for start in range(0, len(body), 73):
                        self.wfile.write(body[start:start + 73])
                        self.wfile.flush()
                except (BrokenPipeError, ConnectionResetError):
                    pass

            def log_message(self, *_args):
                pass

        self.server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
        self.thread = threading.Thread(target=self.server.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server.server_port}/v1"

    def close(self):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=5)

    def configure(self, payload, status=200, content_type="application/json", headers=None):
        assert self.requests.empty(), "unconsumed upstream requests"
        body = payload if isinstance(payload, bytes) else json.dumps(payload).encode()
        self.reply = (status, content_type, body, headers or {})


class EmpProcess:
    def __init__(self, command, upstream, root):
        root.mkdir()
        home = root / "codex"
        home.mkdir()
        (home / "auth.json").write_text(json.dumps({
            "tokens": {"access_token": "e2e-native-token", "account_id": "e2e-owner"}
        }))
        config = _integration_test_config(root)
        config["providers"] = [
            {"id": name, "base_url": upstream.base_url, "protocol": protocol,
             "auth_mode": "forward" if name == "native" else "api_key",
             **({} if name == "native" else {"api_key": "e2e-provider-token"})}
            for name, protocol in [
                ("test", "chat_completions"), ("anthropic", "anthropic_messages"),
                ("responses", "responses"), ("native", "responses")
            ]
        ]
        config["models"] = [
            {"id": f"{name}/model", "provider": name, "upstream_id": "upstream-model",
             "context_window": 256000, "enabled": True}
            for name in ("test", "anthropic", "responses", "native")
        ]
        (root / "native.json").write_text('{"models":[]}')
        self.codex_config = home / "config.toml"
        self.codex_config.write_text('# user settings\n[features]\nweb_search = true\n')
        self.config_path = root / "config.json"
        self.config_path.write_text(json.dumps(config))
        self.start(command, self.config_path, home)

    @classmethod
    def from_config(cls, command, config_path, home, *, environment_overrides=None, port=0):
        """Use an existing consumer fixture without replacing its configuration."""
        instance = cls.__new__(cls)
        instance.config_path = config_path
        instance.codex_config = home / "config.toml"
        instance.start(command, config_path, home, environment_overrides=environment_overrides, port=port)
        return instance

    def start(self, command, config_path, home, *, environment_overrides=None, port=0):
        environment = dict(os.environ)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
                    "all_proxy", "EASY_MULTI_PROVIDER_MASTER_KEY_FILE"):
            environment.pop(key, None)
        environment.update(CODEX_HOME=str(home), PYTHONUNBUFFERED="1",
                           NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
        environment.update(environment_overrides or {})
        self.environment = environment
        self.process = subprocess.Popen(
            command + ["serve", "--config", str(config_path),
                       "--host", "127.0.0.1", "--port", str(port)],
            cwd=ROOT, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True,
        )
        lines = queue.Queue()

        def read_output():
            for line in self.process.stdout:
                lines.put(line)
            lines.put(None)

        self.reader = threading.Thread(target=read_output, daemon=True)
        self.reader.start()
        startup = []
        try:
            while True:
                line = lines.get(timeout=30)
                if line is None:
                    raise AssertionError("EMP exited before readiness: " + "".join(startup[-12:]))
                startup.append(line)
                if line.startswith("Open in browser: "):
                    url = urlsplit(line.split(": ", 1)[1].strip())
                    self.port = url.port
                    status, headers, _ = self.request("GET", url.path + "?" + url.query)
                    assert status == 303, status
                    self.cookie = headers["set-cookie"].split(";", 1)[0]
                    break
        except BaseException:
            self.close()
            raise

    def request(self, method, path, body=None, *, auth=True, headers=None):
        outgoing = dict(headers or {})
        if auth and path.startswith("/v1/"):
            outgoing.setdefault("Authorization", "Bearer e2e-native-token")
        if auth and hasattr(self, "cookie"):
            outgoing["Cookie"] = self.cookie
        if body is not None and not isinstance(body, bytes):
            body = json.dumps(body).encode()
            outgoing["Content-Type"] = "application/json"
        connection = HTTPConnection("127.0.0.1", self.port, timeout=8)
        try:
            connection.request(method, path, body, outgoing)
            response = connection.getresponse()
            if "text/event-stream" in response.getheader("Content-Type", ""):
                frames = []
                terminal = False
                while True:
                    line = response.readline()
                    if not line:
                        break
                    frames.append(line)
                    if line.startswith(b"data: "):
                        event = json.loads(line[6:])
                        terminal = event.get("type") in {
                            "response.completed", "response.incomplete", "response.failed"}
                    if terminal and not line.strip():
                        break
                raw = b"".join(frames)
            else:
                raw = response.read()
            return response.status, dict((k.lower(), v) for k, v in response.getheaders()), raw
        finally:
            connection.close()

    def close(self):
        if self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.reader.join(timeout=5)
        self.process.stdout.close()
