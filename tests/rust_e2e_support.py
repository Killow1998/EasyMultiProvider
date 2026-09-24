"""Isolated real-process and loopback fixtures for Python/Rust endpoint tests."""
import json
import os
from pathlib import Path
import queue
import re
import subprocess
import sys
import threading
from dataclasses import dataclass
from http.client import HTTPConnection
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from urllib.parse import urlsplit

import zstandard

ROOT = Path(__file__).resolve().parents[1]
GENERATED_ID = re.compile(r"^(resp|msg|rs|fc)_[0-9a-f]{32}$")


@dataclass(frozen=True)
class PythonRuntime:
    """One explicit Python implementation, environment, and working root."""

    root: Path
    formal_oracle: bool

    @classmethod
    def from_environment(cls):
        if "EMP_PYTHON_ORACLE_ROOT" not in os.environ:
            root = ROOT.resolve()
            runtime = cls(root, False)
        else:
            try:
                root = Path(os.environ["EMP_PYTHON_ORACLE_ROOT"]).expanduser().resolve(strict=True)
            except OSError as exc:
                raise AssertionError("EMP_PYTHON_ORACLE_ROOT is not a valid directory") from exc
            if not root.is_dir():
                raise AssertionError("EMP_PYTHON_ORACLE_ROOT is not a directory")
            runtime = cls(root, True)
            environment = runtime.environment()
            probe = subprocess.run(
                [
                    sys.executable,
                    "-c",
                    "import easy_multi_provider; "
                    "print(easy_multi_provider.__version__); "
                    "print(easy_multi_provider.__file__)",
                ],
                cwd=root,
                env=environment,
                capture_output=True,
                text=True,
                timeout=5,
            )
            if probe.returncode != 0:
                raise AssertionError(
                    "EMP_PYTHON_ORACLE_ROOT could not import easy_multi_provider: "
                    + probe.stderr
                )
            output = probe.stdout.splitlines()
            if len(output) != 2 or output[0] != "0.11.10":
                version = output[0] if output else "unavailable"
                raise AssertionError(
                    "EMP_PYTHON_ORACLE_ROOT must be Python EMP 0.11.10; got "
                    + version
                )
            package_init = (root / "easy_multi_provider" / "__init__.py").resolve()
            if Path(output[1]).resolve() != package_init:
                raise AssertionError("EMP_PYTHON_ORACLE_ROOT imported a different package")

        root_text = str(runtime.root)
        while root_text in sys.path:
            sys.path.remove(root_text)
        sys.path.insert(0, root_text)
        return runtime

    def command(self, *arguments):
        return [sys.executable, *arguments]

    def environment(self, base=None):
        environment = dict(os.environ if base is None else base)
        environment["PYTHONPATH"] = str(self.root)
        environment["PYTHONDONTWRITEBYTECODE"] = "1"
        return environment

    def cwd(self, override=None):
        return self.root if override is None else Path(override)


PYTHON_RUNTIME = PythonRuntime.from_environment()


def rust_environment(base=None):
    environment = dict(os.environ if base is None else base)
    environment.pop("PYTHONPATH", None)
    return environment


from tests.test_server import _integration_test_config


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
        self.reply_gate = None

        owner = self

        class Handler(BaseHTTPRequestHandler):
            def do_POST(self):
                raw = self.rfile.read(int(self.headers["Content-Length"]))
                if self.headers.get("Content-Encoding") == "zstd":
                    raw = zstandard.ZstdDecompressor().decompress(raw)
                owner.requests.put((self.path, dict(self.headers), json.loads(raw)))
                self.send_fixture_reply()

            def do_GET(self):
                if not urlsplit(self.path).path.endswith("/models"):
                    self.send_error(501, "unsupported fixture request")
                    return
                owner.requests.put((self.path, dict(self.headers), None))
                self.send_fixture_reply()

            def send_fixture_reply(self):
                status, content_type, body, headers = owner.reply
                if owner.reply_gate is not None:
                    owner.reply_gate.wait(timeout=8)
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
    def from_config(cls, command, config_path, home, *, environment_overrides=None, port=0, arguments=None):
        """Use an existing consumer fixture without replacing its configuration."""
        instance = cls.__new__(cls)
        instance.config_path = config_path
        instance.codex_config = home / "config.toml"
        instance.start(command, config_path, home, environment_overrides=environment_overrides, port=port, arguments=arguments)
        return instance

    def start(self, command, config_path, home, *, environment_overrides=None, port=0, arguments=None):
        configured_rust_binary = os.environ.get("EMP_RUST_BINARY")
        command_binary = Path(command[0]).resolve()
        if configured_rust_binary and command_binary == Path(configured_rust_binary).resolve():
            self.runtime_kind = "rust"
            process_cwd = ROOT
        elif command_binary == Path(sys.executable).resolve():
            self.runtime_kind = (
                "python_oracle" if PYTHON_RUNTIME.formal_oracle else "python_snapshot"
            )
            process_cwd = PYTHON_RUNTIME.cwd()
        else:
            raise AssertionError("EmpProcess requires an explicit Python or EMP launcher")
        environment = dict(os.environ)
        for key in ("HTTP_PROXY", "HTTPS_PROXY", "ALL_PROXY", "http_proxy", "https_proxy",
                    "all_proxy", "EASY_MULTI_PROVIDER_MASTER_KEY_FILE"):
            environment.pop(key, None)
        environment.update(HTTP_PROXY="http://127.0.0.1:1", HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1")
        environment.update(CODEX_HOME=str(home), PYTHONUNBUFFERED="1",
                           NO_PROXY="127.0.0.1,localhost", no_proxy="127.0.0.1,localhost")
        environment.update(environment_overrides or {})
        if self.runtime_kind == "rust":
            environment.pop("PYTHONPATH", None)
        else:
            environment = PYTHON_RUNTIME.environment(environment)
        self.environment = environment
        self.cwd = process_cwd
        if arguments is None:
            arguments = ["serve", "--config", str(config_path), "--host", "127.0.0.1", "--port", str(port)]
        self.process = subprocess.Popen(
            command + arguments,
            cwd=process_cwd, env=environment, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
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
                    self.opened_url = line.split(": ", 1)[1].strip()
                    url = urlsplit(self.opened_url)
                    self.port = url.port
                    if self.runtime_kind == "python_oracle":
                        assert url.path in ("", "/") and not url.query, self.opened_url
                        status, headers, _ = self.request("GET", "/", auth=False)
                        assert status == 200, status
                        cookie = headers.get("set-cookie", "")
                        assert cookie.startswith("emp_session="), cookie
                        self.cookie = cookie.split(";", 1)[0]
                    else:
                        self.login_mode = "bootstrap-token"
                        assert url.path == "/" and url.query.startswith("bootstrap="), self.opened_url
                        bare_status, bare_headers, _ = self.request("GET", "/", auth=False)
                        assert bare_status == 401, bare_status
                        assert "set-cookie" not in bare_headers, bare_headers
                        target = url.path + "?" + url.query
                        status, headers, _ = self.request("GET", target, auth=False)
                        assert status == 303, status
                        cookie = headers.get("set-cookie", "")
                        assert cookie.startswith("emp_session="), cookie
                        self.cookie = cookie.split(";", 1)[0]
                        replay_status, replay_headers, _ = self.request("GET", target, auth=False)
                        assert replay_status == 401, replay_status
                        assert "set-cookie" not in replay_headers, replay_headers
                    if self.runtime_kind == "python_oracle":
                        self.login_mode = "bare-root-cookie"
                    status, _, raw = self.request("GET", "/api/config")
                    assert status == 200, raw
                    # The fixture asks the OS for a port; persisted production
                    # configuration needs the assigned nonzero port in Python.
                    # Apply this through the same UI API in both implementations.
                    if port == 0:
                        status, _, raw = self.request("GET", "/api/config")
                        assert status == 200, raw
                        config = json.loads(raw)
                        if config["port"] == 0:
                            config["port"] = self.port
                            status, _, raw = self.request("POST", "/api/config", config)
                            assert status == 200, raw
                    self.startup_output = tuple(startup)
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
