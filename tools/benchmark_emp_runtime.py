#!/usr/bin/env python3
"""Compare Python and Rust EMP local overhead with a zero-delay fake Responses API.

This is a measurement harness, not a release gate. It refuses to produce timing
comparisons unless public endpoint and Responses semantics match first.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import base64
import gzip
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import json
import os
from pathlib import Path
import platform
import queue
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit

try:
    import psutil
except ImportError:  # pragma: no cover - reported as a harness prerequisite
    psutil = None


REPLY_TEXT = "EMP_BENCHMARK_RESPONSE_OK"
FIXTURE_CALLER_KEY = "benchmark-fixture-caller-key"
FIXTURE_PROVIDER_KEY = "benchmark-fixture-provider-key"
UPSTREAM_MODEL = "benchmark-upstream-model"
BENCHMARK_CONTEXT_WINDOW = 100_000_000
DEFAULT_PYTHON_ROOT = Path(__file__).resolve().parents[2] / "EasyMultiProvider"
EXTERNAL_CREDENTIAL_ENV = (
    "EASY_MULTI_PROVIDER_MASTER_KEY",
    "OPENAI_API_KEY",
    "CODEX_API_KEY",
    "ANTHROPIC_API_KEY",
    "GEMINI_API_KEY",
    "AZURE_OPENAI_API_KEY",
    "AWS_ACCESS_KEY_ID",
    "AWS_SECRET_ACCESS_KEY",
)


class BenchmarkError(RuntimeError):
    """A setup, parity, or measurement error safe to report without payloads."""

    def __init__(self, code: str):
        safe_code = code if code and all(char.isalnum() or char == "_" for char in code) else "benchmark_error"
        self.code = safe_code
        super().__init__(safe_code)


def percentile(values: list[float], quantile: float) -> float | None:
    """Return a linearly interpolated percentile in the inclusive 0..1 range."""
    if not values:
        return None
    if not 0 <= quantile <= 1:
        raise ValueError("quantile must be between zero and one")
    ordered = sorted(values)
    position = (len(ordered) - 1) * quantile
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    fraction = position - lower
    return ordered[lower] + (ordered[upper] - ordered[lower]) * fraction


def latency_summary(samples_ms: list[float], elapsed_seconds: float, errors: int) -> dict:
    return {
        "count": len(samples_ms),
        "errors": errors,
        "throughput_requests_per_second": (
            len(samples_ms) / elapsed_seconds if elapsed_seconds > 0 else None
        ),
        "p50_ms": percentile(samples_ms, 0.50),
        "p95_ms": percentile(samples_ms, 0.95),
        "p99_ms": percentile(samples_ms, 0.99),
    }


def semantic_diff(left, right, path="$", limit=32) -> list[str]:
    """Return differing JSON paths only; never include request or response text."""
    differences = []
    if type(left) is not type(right):
        return [path]
    if isinstance(left, dict):
        for key in sorted(set(left) | set(right)):
            if key not in left or key not in right:
                differences.append(f"{path}.{key}")
            else:
                differences.extend(semantic_diff(left[key], right[key], f"{path}.{key}", limit))
            if len(differences) >= limit:
                return differences[:limit]
        return differences
    if isinstance(left, list):
        if len(left) != len(right):
            differences.append(f"{path}.length")
        for index, (a, b) in enumerate(zip(left, right)):
            differences.extend(semantic_diff(a, b, f"{path}[{index}]", limit))
            if len(differences) >= limit:
                return differences[:limit]
        return differences
    return [] if left == right else [path]


def _response_value(model: str) -> dict:
    message = {
        "id": "msg_emp_benchmark",
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": REPLY_TEXT, "annotations": []}],
    }
    return {
        "id": "resp_emp_benchmark",
        "object": "response",
        "created_at": 1,
        "status": "completed",
        "model": model,
        "output": [message],
        "usage": {
            "input_tokens": 8,
            "input_tokens_details": {"cached_tokens": 0},
            "output_tokens": 2,
            "output_tokens_details": {"reasoning_tokens": 0},
            "total_tokens": 10,
        },
    }


def _encode_request_payload(payload: bytes, content_encoding: str) -> bytes:
    if content_encoding == "identity":
        return payload
    if content_encoding == "gzip":
        return gzip.compress(payload, mtime=0)
    if content_encoding == "zstd":
        try:
            import zstandard
        except ImportError:
            raise BenchmarkError("zstandard_compressor_required") from None
        return zstandard.ZstdCompressor(level=3).compress(payload)
    raise BenchmarkError("unsupported_benchmark_content_encoding")


def _request_headers(*, cookie: str = "", payload_present: bool,
                     content_encoding: str = "identity") -> dict[str, str]:
    headers = {"Connection": "close"}
    if cookie:
        headers["Cookie"] = cookie
    if payload_present:
        headers["Content-Type"] = "application/json"
        headers["Authorization"] = "Bearer " + FIXTURE_CALLER_KEY
        if content_encoding != "identity":
            if content_encoding not in {"gzip", "zstd"}:
                raise BenchmarkError("unsupported_benchmark_content_encoding")
            headers["Content-Encoding"] = content_encoding
    elif content_encoding != "identity":
        raise BenchmarkError("content_encoding_requires_payload")
    return headers


def _large_history_payload(logical_size: int, stream: bool) -> bytes:
    history = [
        {"role": "user", "content": [{"type": "input_text", "text": "h" * logical_size}]},
        {"role": "assistant", "content": [{"type": "output_text", "text": "prior assistant turn"}]},
        {"role": "user", "content": [{"type": "input_text", "text": "latest turn"}]},
    ]
    return json.dumps({
        "model": "benchmark/model", "input": history, "stream": stream,
    }, separators=(",", ":")).encode()


def _sse_payload(model: str) -> bytes:
    response = _response_value(model)
    message = response["output"][0]
    events = [
        ("response.created", {"response": {**response, "status": "in_progress", "output": []}}),
        ("response.output_item.added", {"output_index": 0, "item": {
            **message, "status": "in_progress", "content": []
        }}),
        ("response.content_part.added", {
            "item_id": message["id"], "output_index": 0, "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []},
        }),
        ("response.output_text.delta", {
            "item_id": message["id"], "output_index": 0, "content_index": 0,
            "delta": REPLY_TEXT,
        }),
        ("response.output_text.done", {
            "item_id": message["id"], "output_index": 0, "content_index": 0,
            "text": REPLY_TEXT,
        }),
        ("response.content_part.done", {
            "item_id": message["id"], "output_index": 0, "content_index": 0,
            "part": message["content"][0],
        }),
        ("response.output_item.done", {"output_index": 0, "item": message}),
        ("response.completed", {"response": response}),
    ]
    chunks = []
    for sequence, (kind, value) in enumerate(events):
        value = {"type": kind, "sequence_number": sequence, **value}
        chunks.append(("event: %s\ndata: %s\n\n" % (
            kind, json.dumps(value, separators=(",", ":"))
        )).encode())
    return b"".join(chunks)


def _fake_upstream_status(path: str, authorization: str | None, body) -> int:
    if path != "/v1/responses":
        return 404
    if authorization != "Bearer " + FIXTURE_PROVIDER_KEY:
        return 401
    if not isinstance(body, dict) or body.get("model") != UPSTREAM_MODEL:
        return 400
    return 200


class _FakeResponsesHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        self.server.request_count_lock.acquire()
        self.server.request_count += 1
        self.server.request_count_lock.release()
        length = int(self.headers.get("Content-Length", "0"))
        raw = self.rfile.read(length)
        try:
            body = json.loads(raw)
        except (UnicodeDecodeError, json.JSONDecodeError):
            with self.server.error_lock:
                self.server.error_count += 1
            self.send_error(400)
            return
        status = _fake_upstream_status(
            self.path, self.headers.get("Authorization"), body
        )
        if status != 200:
            with self.server.error_lock:
                self.server.error_count += 1
            self.send_error(status)
            return
        with self.server.model_counts_lock:
            self.server.model_counts[body.get("model", "")] = (
                self.server.model_counts.get(body.get("model", ""), 0) + 1
            )
        if body.get("stream") is True:
            payload = _sse_payload(UPSTREAM_MODEL)
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Content-Length", str(len(payload)))
            self.send_header("Connection", "close")
            self.end_headers()
            try:
                for start in range(0, len(payload), 97):
                    self.wfile.write(payload[start:start + 97])
                    self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                pass
            return
        payload = json.dumps(_response_value(UPSTREAM_MODEL), separators=(",", ":")).encode()
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(payload)))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.write(payload)

    def log_message(self, *_args):
        pass


class FakeResponsesUpstream(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 128

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _FakeResponsesHandler)
        self.request_count = 0
        self.error_count = 0
        self.error_lock = threading.Lock()
        self.model_counts = {}
        self.request_count_lock = threading.Lock()
        self.model_counts_lock = threading.Lock()
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server_port}/v1"

    def reset_counts(self):
        with self.request_count_lock, self.error_lock, self.model_counts_lock:
            self.request_count = 0
            self.error_count = 0
            self.model_counts.clear()

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=3)


def _request(port: int, method: str, path: str, *, cookie: str = "", payload: bytes | None = None,
             timeout: float = 10.0, operation: str = "http_request",
             content_encoding: str = "identity") -> dict:
    headers = _request_headers(
        cookie=cookie, payload_present=payload is not None,
        content_encoding=content_encoding,
    )
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    started = time.perf_counter()
    try:
        connection.request(method, path, body=payload, headers=headers)
        response = connection.getresponse()
        first_event_ms = None
        event_types = []
        text_parts = []
        terminal = None
        if "text/event-stream" in response.getheader("Content-Type", ""):
            while True:
                line = response.readline()
                if not line:
                    break
                if line.startswith(b"data: "):
                    try:
                        event = json.loads(line[6:])
                    except json.JSONDecodeError:
                        continue
                    if first_event_ms is None:
                        first_event_ms = (time.perf_counter() - started) * 1000
                    kind = event.get("type")
                    if isinstance(kind, str):
                        event_types.append(kind)
                    if kind == "response.output_text.delta":
                        text_parts.append(event.get("delta", ""))
                    if kind in {"response.completed", "response.failed", "response.incomplete"}:
                        terminal = kind
                        break
            return {
                "status": response.status,
                "headers": {key.lower(): value for key, value in response.getheaders()},
                "body": b"",
                "elapsed_ms": (time.perf_counter() - started) * 1000,
                "first_event_ms": first_event_ms,
                "event_types": event_types,
                "text": "".join(text_parts),
                "terminal": terminal,
            }
        body = response.read()
        return {
            "status": response.status,
            "headers": {key.lower(): value for key, value in response.getheaders()},
            "body": body,
            "elapsed_ms": (time.perf_counter() - started) * 1000,
            "first_event_ms": None,
            "event_types": [],
            "text": "",
            "terminal": None,
        }
    except TimeoutError:
        raise BenchmarkError(operation + "_timeout") from None
    finally:
        connection.close()


def _management_semantic(result: dict, kind: str) -> dict:
    semantic = {"status": result["status"]}
    if result["status"] != 200:
        return semantic
    value = json.loads(result["body"])
    if kind == "healthz":
        semantic["status_value"] = value.get("status")
    elif kind == "config":
        semantic["port"] = value.get("port")
        semantic["host"] = value.get("host")
        semantic["model_ids"] = sorted(
            item.get("id") for item in value.get("models", []) if isinstance(item, dict)
        )
        semantic["provider_ids"] = sorted(
            item.get("id") for item in value.get("providers", []) if isinstance(item, dict)
        )
    elif kind == "models":
        semantic["model_slugs"] = sorted(
            item.get("id") for item in value.get("data", []) if isinstance(item, dict)
        )
    return semantic


def _responses_semantic(result: dict, stream: bool) -> dict:
    semantic = {"status": result["status"]}
    if result["status"] != 200:
        return semantic
    if stream:
        semantic.update({
            "event_types": result["event_types"],
            "terminal": result["terminal"],
            "text": result["text"],
        })
    else:
        value = json.loads(result["body"])
        output = value.get("output", [])
        semantic.update({
            "object": value.get("object"),
            "status_value": value.get("status"),
            "model": value.get("model"),
            "output_types": [item.get("type") for item in output if isinstance(item, dict)],
            "text": "".join(
                part.get("text", "")
                for item in output if isinstance(item, dict)
                for part in item.get("content", []) if isinstance(part, dict)
                and part.get("type") == "output_text"
            ),
            "total_tokens": value.get("usage", {}).get("total_tokens"),
        })
    return semantic


EXPECTED_SSE_EVENT_TYPES = [
    "response.created",
    "response.output_item.added",
    "response.content_part.added",
    "response.output_text.delta",
    "response.output_text.done",
    "response.content_part.done",
    "response.output_item.done",
    "response.completed",
]


def _valid_workload_result(kind: str, result: dict) -> bool:
    if result["status"] != 200:
        return False
    if not kind.startswith("responses_"):
        return True
    if kind == "responses_sse":
        try:
            return (
                result["event_types"] == EXPECTED_SSE_EVENT_TYPES
                and result["terminal"] == "response.completed"
                and result["text"] == REPLY_TEXT
            )
        except (KeyError, TypeError):
            return False
    try:
        semantic = _responses_semantic(result, stream=False)
    except (AttributeError, UnicodeDecodeError, json.JSONDecodeError, TypeError):
        return False
    return semantic == {
        "status": 200,
        "object": "response",
        "status_value": "completed",
        "model": UPSTREAM_MODEL,
        "output_types": ["message"],
        "text": REPLY_TEXT,
        "total_tokens": 10,
    }


def _bootstrap_target(runtime_kind: str, startup_url: str) -> str:
    if runtime_kind == "python":
        return "/"
    parsed = urlsplit(startup_url)
    target = parsed.path or "/"
    return target + ("?" + parsed.query if parsed.query else "")


def safe_error_name(error: Exception) -> str:
    """Return a diagnostic category without serializing arbitrary exception text."""
    if isinstance(error, BenchmarkError):
        return error.code
    return type(error).__name__


def _startup_log_code(line: str) -> str | None:
    lowered = line.lower()
    exception_class_codes = {
        "ModuleNotFoundError": "module_not_found",
        "ImportError": "import_error",
        "FileNotFoundError": "file_missing",
        "PermissionError": "permission_denied",
        "ConfigError": "configuration_error",
        "ValueError": "value_error",
        "RuntimeError": "runtime_error",
        "OSError": "os_error",
        "TypeError": "type_error",
        "KeyError": "key_error",
        "AssertionError": "assertion_error",
    }
    exception_class = line.strip().split(":", 1)[0]
    if exception_class in exception_class_codes:
        if exception_class == "ModuleNotFoundError" and "No module named '" in line:
            missing = line.split("No module named '", 1)[1].split("'", 1)[0]
            components = missing.split(".")
            if components and all(component.isidentifier() for component in components):
                return "python_module_not_found_" + "_".join(components)
        return "python_" + exception_class_codes[exception_class]
    patterns = (
        ("configuration", "configuration_error"),
        ("master key", "master_key_error"),
        ("fernet", "master_key_error"),
        ("permission denied", "permission_denied"),
        ("address already in use", "address_in_use"),
        ("no such file", "file_missing"),
        ("panic", "process_panic"),
        ("traceback", "python_traceback"),
    )
    return next((code for fragment, code in patterns if fragment in lowered), None)


def _tree_metrics(pid: int) -> dict:
    if psutil is None:
        raise BenchmarkError("psutil is required to sample process-tree resources")
    try:
        root = psutil.Process(pid)
        processes = [root, *root.children(recursive=True)]
    except psutil.Error:
        processes = []
    rss = cpu = threads = descriptors = 0
    descriptor_available = True
    for process in processes:
        try:
            memory = process.memory_info()
            rss += memory.rss
            times = process.cpu_times()
            cpu += times.user + times.system
            threads += process.num_threads()
            if hasattr(process, "num_fds"):
                descriptors += process.num_fds()
            elif hasattr(process, "num_handles"):
                descriptors += process.num_handles()
            else:
                descriptor_available = False
        except psutil.Error:
            continue
    return {
        "rss_bytes": rss if processes else None,
        "cpu_seconds": cpu if processes else None,
        "threads": threads if processes else None,
        "descriptors": descriptors if processes and descriptor_available else None,
    }


class ResourceSampler:
    def __init__(self, pid: int, interval: float = 0.025):
        self.pid = pid
        self.interval = interval
        self.stop_event = threading.Event()
        self._peak_lock = threading.Lock()
        self._overall_peak_rss_bytes = 0
        self._case_peak_rss_bytes = 0
        self.thread = threading.Thread(target=self._run, daemon=True)

    def _run(self):
        while not self.stop_event.is_set():
            values = _tree_metrics(self.pid)
            self._record_rss(values["rss_bytes"])
            self.stop_event.wait(self.interval)

    def _record_rss(self, rss_bytes: int | None):
        if rss_bytes is None:
            return
        with self._peak_lock:
            self._overall_peak_rss_bytes = max(self._overall_peak_rss_bytes, rss_bytes)
            self._case_peak_rss_bytes = max(self._case_peak_rss_bytes, rss_bytes)

    def reset_peak(self, baseline_rss_bytes: int | None = None):
        if baseline_rss_bytes is None:
            baseline_rss_bytes = _tree_metrics(self.pid)["rss_bytes"] or 0
        with self._peak_lock:
            self._case_peak_rss_bytes = baseline_rss_bytes

    def case_peak(self) -> int:
        current_rss = _tree_metrics(self.pid)["rss_bytes"]
        self._record_rss(current_rss)
        with self._peak_lock:
            return self._case_peak_rss_bytes

    def overall_peak(self) -> int:
        current_rss = _tree_metrics(self.pid)["rss_bytes"]
        self._record_rss(current_rss)
        with self._peak_lock:
            return self._overall_peak_rss_bytes

    def start(self):
        self.thread.start()

    def stop(self):
        self.stop_event.set()
        self.thread.join(timeout=2)


class EmpService:
    def __init__(self, command: list[str], cwd: Path, config_path: Path, home: Path,
                 env: dict, runtime_kind: str):
        self.command = command
        self.cwd = cwd
        self.config_path = config_path
        self.home = home
        self.env = env
        self.runtime_kind = runtime_kind
        self.process = None
        self.lines = queue.Queue()
        self.reader = None
        self.port = None
        self.cookie = ""
        self.startup_log_codes = []
        self.exit_code = None

    def __enter__(self):
        self.home.mkdir(parents=True, exist_ok=True)
        auth_path = self.home / "auth.json"
        auth_path.write_text(json.dumps({
            "tokens": {
                "access_token": FIXTURE_CALLER_KEY,
                "account_id": "benchmark-fixture-account",
            }
        }), encoding="utf-8")
        os.chmod(auth_path, 0o600)
        self.process = subprocess.Popen(
            self.command + [
                "serve", "--config", str(self.config_path), "--host", "127.0.0.1", "--port", "0"
            ],
            cwd=self.cwd,
            env=self.env,
            stdout=subprocess.PIPE,
            stderr=subprocess.STDOUT,
            text=True,
            encoding="utf-8",
            errors="replace",
            bufsize=1,
        )

        def read_lines():
            for line in self.process.stdout:
                code = _startup_log_code(line)
                if code is not None:
                    self.startup_log_codes.append(code)
                self.lines.put(line)
            self.lines.put(None)

        self.reader = threading.Thread(target=read_lines, daemon=True)
        self.reader.start()
        deadline = time.monotonic() + 30
        startup_error = "startup_timeout"
        while time.monotonic() < deadline:
            try:
                line = self.lines.get(timeout=min(1, max(0.01, deadline - time.monotonic())))
            except queue.Empty:
                if self.process.poll() is not None:
                    startup_error = self._safe_process_exit_code()
                    break
                continue
            if line is None:
                startup_error = self._safe_process_exit_code()
                break
            if line.startswith("Open in browser: "):
                startup_text = line.split(": ", 1)[1].strip()
                startup_url = urlsplit(startup_text)
                self.port = startup_url.port
                self.startup_path = _bootstrap_target(self.runtime_kind, startup_text)
                break
        if self.port is None:
            self.close()
            raise BenchmarkError(startup_error)
        root = _request(
            self.port, "GET", self.startup_path,
            operation=self.runtime_kind + "_bootstrap",
        )
        set_cookie = root["headers"].get("set-cookie", "")
        self.cookie = set_cookie.split(";", 1)[0]
        expected_status = 200 if self.runtime_kind == "python" else 303
        if root["status"] != expected_status or not self.cookie:
            self.close()
            raise BenchmarkError("management_bootstrap_failed")
        return self

    def _safe_process_exit_code(self) -> str:
        self.exit_code = self.process.poll()
        parts = [self.runtime_kind, "process_exited"]
        if self.exit_code is not None:
            code = "negative" + str(abs(self.exit_code)) if self.exit_code < 0 else str(self.exit_code)
            parts.extend(("exit", code))
        if self.startup_log_codes:
            parts.append(self.startup_log_codes[-1])
        return "_".join(parts)

    def __exit__(self, *_exc):
        self.close()

    def close(self):
        if self.process is not None and self.process.poll() is None:
            self.process.terminate()
            try:
                self.process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        if self.reader is not None:
            self.reader.join(timeout=2)


def _python_version(python: Path, root: Path, env: dict) -> str:
    script = "import easy_multi_provider; print(easy_multi_provider.__version__)"
    result = subprocess.run(
        [str(python), "-c", script], cwd=root, env=env, check=True,
        capture_output=True, text=True, timeout=15,
    )
    return result.stdout.strip()


def _normalized_config(python: Path, python_root: Path, env: dict, upstream_url: str,
                       work_root: Path) -> bytes:
    native_catalog = work_root / "native-catalog.json"
    native_catalog.write_text('{"models":[]}', encoding="utf-8")
    raw = {
        "host": "127.0.0.1",
        "port": 4200,
        "native_catalog_path": str(native_catalog),
        "account_store_path": str(work_root / "account-store"),
        "secret_store_path": str(work_root / "secret-store"),
        "providers": [{
            "id": "benchmark",
            "name": "Benchmark fixture",
            "base_url": upstream_url,
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": FIXTURE_PROVIDER_KEY,
        }],
        "models": [{
            "id": "benchmark/model",
            "provider": "benchmark",
            "upstream_id": UPSTREAM_MODEL,
            "enabled": True,
            "reasoning_levels": ["low"],
            "context_window": BENCHMARK_CONTEXT_WINDOW,
            "output_limit": 4096,
        }],
        "accounts": [],
    }
    script = (
        "import json,sys; from easy_multi_provider.config import normalize; "
        "print(json.dumps(normalize(json.load(sys.stdin)), separators=(',', ':')))"
    )
    result = subprocess.run(
        [str(python), "-c", script], cwd=python_root, env=env,
        input=json.dumps(raw), check=True, capture_output=True, text=True, timeout=15,
    )
    return result.stdout.strip().encode()


def _service_environment(base_env: dict, home: Path, master_key: Path, python_root: Path) -> dict:
    env = dict(base_env)
    for key in EXTERNAL_CREDENTIAL_ENV:
        env.pop(key, None)
    env.update({
        "CODEX_HOME": str(home),
        "EASY_MULTI_PROVIDER_MASTER_KEY_FILE": str(master_key),
        "HTTP_PROXY": "http://127.0.0.1:1",
        "HTTPS_PROXY": "http://127.0.0.1:1",
        "ALL_PROXY": "http://127.0.0.1:1",
        "NO_PROXY": "127.0.0.1,localhost,::1",
        "no_proxy": "127.0.0.1,localhost,::1",
        "PYTHONUNBUFFERED": "1",
    })
    for key in ("http_proxy", "https_proxy", "all_proxy"):
        env.pop(key, None)
    env["PYTHONPATH"] = str(python_root)
    return env


def _preflight(service: EmpService, upstream: FakeResponsesUpstream) -> tuple[dict, list[str]]:
    upstream.reset_counts()
    outcomes = {}
    errors = []
    for kind, method, path in (
        ("healthz", "GET", "/healthz"),
        ("config", "GET", "/api/config"),
        ("models", "GET", "/v1/models"),
    ):
        result = _request(
            service.port, method, path, cookie=service.cookie,
            operation="preflight_" + kind,
        )
        outcomes[kind] = _management_semantic(result, kind)
        if not _valid_workload_result(kind, result):
            errors.append(kind + "_semantic")
    small = b"semantic-preflight-1024"
    for kind, stream in (("responses_nonstream", False), ("responses_sse", True)):
        payload = json.dumps({
            "model": "benchmark/model", "input": small.decode(), "stream": stream,
        }, separators=(",", ":")).encode()
        result = _request(
            service.port, "POST", "/v1/responses", payload=payload,
            operation="preflight_" + kind,
        )
        outcomes[kind] = _responses_semantic(result, stream)
        if not _valid_workload_result(kind, result):
            errors.append(kind + "_semantic")
    count = upstream.request_count
    if count != 2:
        errors.append("preflight_upstream_request_count")
    if upstream.error_count:
        errors.append("preflight_fake_upstream_error")
    return outcomes, errors


def _workload_request(service: EmpService, kind: str, body: bytes | None = None, *,
                      content_encoding: str = "identity", timeout: float = 10.0) -> dict:
    if kind == "healthz":
        return _request(service.port, "GET", "/healthz", operation="healthz")
    if kind == "config":
        return _request(
            service.port, "GET", "/api/config", cookie=service.cookie,
            operation="config",
        )
    if kind == "models":
        return _request(service.port, "GET", "/v1/models", operation="models")
    return _request(
        service.port, "POST", "/v1/responses", payload=body, operation=kind,
        content_encoding=content_encoding, timeout=timeout,
    )


def _measure_case(service: EmpService, upstream: FakeResponsesUpstream, kind: str,
                  payload: bytes | None, iterations: int, warmup: int,
                  concurrency: int, *, logical_history_bytes: int | None = None,
                  logical_payload_bytes: int | None = None,
                  content_encoding: str = "identity") -> dict:
    def perform(_index):
        try:
            is_large_history = (
                logical_history_bytes is not None
                and logical_history_bytes >= 16 * 1024 * 1024
            )
            timeout = 180.0 if is_large_history else 10.0
            return _workload_request(
                service, kind, payload, content_encoding=content_encoding,
                timeout=timeout,
            ), None
        except (BenchmarkError, OSError, http.client.HTTPException, ValueError) as exc:
            if isinstance(exc, BenchmarkError):
                return None, exc.code
            return None, type(exc).__name__

    warmup_count = min(warmup, max(1, iterations))
    warmup_errors = 0
    warmup_content_mismatches = 0
    if concurrency == 1:
        for index in range(warmup_count):
            result, error_kind = perform(index)
            if error_kind or result is None or result["status"] != 200:
                warmup_errors += 1
            elif not _valid_workload_result(kind, result):
                warmup_errors += 1
                warmup_content_mismatches += 1
    else:
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            for result, error_kind in pool.map(perform, range(warmup_count)):
                if error_kind or result is None or result["status"] != 200:
                    warmup_errors += 1
                elif not _valid_workload_result(kind, result):
                    warmup_errors += 1
                    warmup_content_mismatches += 1

    case_start_resources = _tree_metrics(service.process.pid)
    sampler = getattr(service, "sampler", None)
    if sampler is not None:
        sampler.reset_peak(case_start_resources["rss_bytes"])
    upstream_before = upstream.request_count
    cpu_before = case_start_resources["cpu_seconds"]
    started = time.perf_counter()
    results = []
    error_kinds = {}
    if warmup_content_mismatches:
        error_kinds["content_mismatch"] = warmup_content_mismatches
    if concurrency == 1:
        for index in range(iterations):
            results.append(perform(index))
    else:
        with ThreadPoolExecutor(max_workers=concurrency) as pool:
            futures = [pool.submit(perform, index) for index in range(iterations)]
            for future in as_completed(futures):
                results.append(future.result())
    elapsed = time.perf_counter() - started
    case_end_resources = _tree_metrics(service.process.pid)
    cpu_after = case_end_resources["cpu_seconds"]
    upstream_delta = upstream.request_count - upstream_before
    complete = []
    first_events = []
    errors = warmup_errors
    for result, error_kind in results:
        if result is None:
            errors += 1
            error_kinds[error_kind] = error_kinds.get(error_kind, 0) + 1
            continue
        if result["status"] != 200:
            errors += 1
            error_kinds["http_status_%d" % result["status"]] = (
                error_kinds.get("http_status_%d" % result["status"], 0) + 1
            )
        complete.append(result["elapsed_ms"])
        if result["first_event_ms"] is not None:
            first_events.append(result["first_event_ms"])
        if (result["status"] == 200 and kind.startswith("responses_")
                and not _valid_workload_result(kind, result)):
            errors += 1
            error_kinds["content_mismatch"] = error_kinds.get("content_mismatch", 0) + 1
    expected_upstream = iterations if kind.startswith("responses_") else 0
    return {
        **latency_summary(complete, elapsed, errors),
        "first_event_p50_ms": percentile(first_events, 0.50),
        "first_event_p95_ms": percentile(first_events, 0.95),
        "first_event_p99_ms": percentile(first_events, 0.99),
        "cpu_seconds": (
            cpu_after - cpu_before if cpu_before is not None and cpu_after is not None else None
        ),
        "upstream_requests": upstream_delta,
        "expected_upstream_requests": expected_upstream,
        "upstream_count_matches": upstream_delta == expected_upstream,
        "error_kinds": error_kinds,
        "warmup_errors": warmup_errors,
        "request_payload_bytes": len(payload) if payload is not None else 0,
        "wire_payload_bytes": len(payload) if payload is not None else 0,
        "logical_payload_bytes": logical_payload_bytes if logical_payload_bytes is not None else (
            len(payload) if payload is not None and content_encoding == "identity" else None
        ),
        "logical_history_bytes": logical_history_bytes,
        "content_encoding": content_encoding,
        "case_start_rss_bytes": case_start_resources["rss_bytes"],
        "case_peak_rss_bytes": sampler.case_peak() if sampler is not None else None,
    }


def _run_measurements(service: EmpService, upstream: FakeResponsesUpstream,
                     idle_metrics: dict, iterations: int, warmup: int,
                     concurrencies: list[int], payload_sizes: list[int],
                     large_history_sizes_mib: list[int],
                     large_iterations: int) -> dict:
    results = {}
    for kind in ("healthz", "config", "models"):
        results[kind] = _measure_case(
            service, upstream, kind, None, iterations, warmup, 1
        )
    for concurrency in concurrencies:
        for size in payload_sizes:
            input_value = "h" * size
            history = [
                {"role": "user", "content": [{"type": "input_text", "text": input_value}]},
                {"role": "assistant", "content": [{"type": "output_text", "text": "prior assistant turn"}]},
                {"role": "user", "content": [{"type": "input_text", "text": "latest turn"}]},
            ]
            for stream in (False, True):
                kind = "responses_sse" if stream else "responses_nonstream"
                payload = json.dumps({
                    "model": "benchmark/model", "input": history, "stream": stream,
                }, separators=(",", ":")).encode()
                case = f"{kind}_history_{size}_bytes_concurrency_{concurrency}"
                results[case] = _measure_case(
                    service, upstream, kind, payload, iterations, warmup, concurrency,
                    logical_history_bytes=size,
                )
    for size_mib in large_history_sizes_mib:
        logical_size = size_mib * 1024 * 1024
        for stream in (False, True):
            kind = "responses_sse" if stream else "responses_nonstream"
            logical_payload = _large_history_payload(logical_size, stream)
            for content_encoding in ("identity", "gzip", "zstd"):
                # Encoding is deliberately outside _measure_case's timed region.
                wire_payload = _encode_request_payload(logical_payload, content_encoding)
                case = (
                    f"{kind}_large_history_{size_mib}mib_{content_encoding}_concurrency_1"
                )
                results[case] = _measure_case(
                    service, upstream, kind, wire_payload, large_iterations, warmup, 1,
                    logical_history_bytes=logical_size,
                    logical_payload_bytes=len(logical_payload),
                    content_encoding=content_encoding,
                )
    return {
        "idle_resources": idle_metrics,
        "peak_rss_bytes": service.sampler.overall_peak(),
        "final_resources": _tree_metrics(service.process.pid),
        "cases": results,
    }


def _environment() -> dict:
    cpu_model = None
    try:
        for line in Path("/proc/cpuinfo").read_text(errors="replace").splitlines():
            if line.lower().startswith("model name"):
                cpu_model = line.split(":", 1)[1].strip()
                break
    except OSError:
        pass
    total_memory = psutil.virtual_memory().total if psutil is not None else None
    return {
        "platform": platform.platform(),
        "system": platform.system(),
        "machine": platform.machine(),
        "processor": platform.processor() or cpu_model,
        "cpu_model": cpu_model,
        "logical_cpu_count": os.cpu_count(),
        "total_memory_bytes": total_memory,
        "python_harness": platform.python_version(),
    }


def _serve_once(name: str, command: list[str], cwd: Path, config_bytes: bytes,
                work_root: Path, key_file: Path, base_env: dict,
                python_root: Path, upstream: FakeResponsesUpstream,
                iterations: int, warmup: int, concurrencies: list[int],
                payload_sizes: list[int], large_history_sizes_mib: list[int],
                large_iterations: int,
                measure: bool) -> tuple[dict, list[str]]:
    config_path = work_root / (name + ".json")
    config_path.write_bytes(config_bytes)
    service_env = _service_environment(base_env, work_root / (name + "-home"), key_file, python_root)
    service = EmpService(
        command, cwd, config_path, work_root / (name + "-home"), service_env, name
    )
    errors = []
    with service:
        health = _request(
            service.port, "GET", "/healthz", operation=name + "_healthz"
        )
        if health["status"] != 200:
            raise BenchmarkError(name + "_health_bootstrap_failed")
        api = _request(
            service.port, "GET", "/api/config", cookie=service.cookie,
            operation=name + "_config",
        )
        if api["status"] != 200:
            raise BenchmarkError(name + "_management_bootstrap_failed")
        service.sampler = ResourceSampler(service.process.pid)
        service.sampler.start()
        if measure:
            for kind in ("healthz", "config", "models"):
                warmed = _workload_request(service, kind)
                if warmed["status"] != 200:
                    errors.append(name + "_idle_warmup_" + kind)
            for stream, kind in (
                (False, "responses_nonstream"),
                (True, "responses_sse"),
            ):
                payload = json.dumps({
                    "model": "benchmark/model",
                    "input": "idle-warmup",
                    "stream": stream,
                }, separators=(",", ":")).encode()
                warmed = _workload_request(service, kind, payload)
                if not _valid_workload_result(kind, warmed):
                    errors.append(name + "_idle_warmup_" + kind)
        time.sleep(0.15)
        idle = _tree_metrics(service.process.pid)
        result = {}
        if not measure:
            result["semantics"], parity_errors = _preflight(service, upstream)
            errors.extend(parity_errors)
            result["upstream"] = {
                "requests": upstream.request_count,
                "errors": upstream.error_count,
            }
        else:
            result = _run_measurements(
                service, upstream, idle, iterations, warmup,
                concurrencies, payload_sizes, large_history_sizes_mib,
                large_iterations,
            )
        service.sampler.stop()
        if measure:
            result["idle_resources"] = idle
            result["peak_rss_bytes"] = service.sampler.overall_peak()
    return result, errors


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--iterations", type=int, default=30)
    parser.add_argument("--warmup", type=int, default=5)
    parser.add_argument("--concurrency", type=int, nargs="+", default=[1, 16])
    parser.add_argument("--payload-sizes", type=int, nargs="+", default=[1024, 1024 * 1024])
    parser.add_argument("--large-history-sizes", type=int, nargs="+", default=[], metavar="MIB")
    parser.add_argument("--large-iterations", type=int, default=1)
    parser.add_argument("--expected-version", default="0.11.10")
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    return parser.parse_args(argv)


def _python_launcher(configured: Path | None, python_root: Path) -> Path:
    candidate = configured or (python_root / ".venv" / "bin" / "python")
    candidate = candidate.expanduser()
    if not candidate.is_absolute():
        candidate = Path.cwd() / candidate
    candidate = Path(os.path.abspath(candidate))
    if not candidate.is_file():
        raise BenchmarkError("python_executable_missing")
    return candidate


def _clean_version(label: str, value: str) -> str:
    value = value.strip()
    expected = "EMP " + label
    if value != expected:
        raise BenchmarkError(label + "_version_mismatch")
    return value


def _validate_parameters(iterations: int, warmup: int, concurrencies: list[int],
                         payload_sizes: list[int], large_history_sizes_mib: list[int],
                         large_iterations: int):
    if iterations < 1 or warmup < 0:
        raise BenchmarkError("invalid_iteration_count")
    if not concurrencies or any(value < 1 for value in concurrencies):
        raise BenchmarkError("invalid_concurrency")
    if not payload_sizes or any(value < 1 or value > 1024 * 1024 for value in payload_sizes):
        raise BenchmarkError("payload_sizes_must_be_1_to_1048576")
    if any(value < 16 or value > 128 for value in large_history_sizes_mib):
        raise BenchmarkError("large_history_sizes_mib_must_be_16_to_128")
    if len(set(large_history_sizes_mib)) != len(large_history_sizes_mib):
        raise BenchmarkError("large_history_sizes_mib_must_be_unique")
    if not 1 <= large_iterations <= 3:
        raise BenchmarkError("large_iterations_must_be_1_to_3")


def run(args) -> tuple[dict, int]:
    if psutil is None:
        raise BenchmarkError("psutil_required")
    python_root = args.python_root.resolve(strict=True)
    python = _python_launcher(args.python, python_root)
    rust_binary = args.rust_binary.resolve(strict=True)
    _validate_parameters(
        args.iterations, args.warmup, args.concurrency, args.payload_sizes,
        args.large_history_sizes, args.large_iterations,
    )

    python_env = dict(os.environ)
    python_env["PYTHONPATH"] = str(python_root)
    python_version = _python_version(python, python_root, python_env)
    if python_version != args.expected_version:
        raise BenchmarkError("python_version_mismatch")
    rust_version_result = subprocess.run(
        [str(rust_binary), "--version"], check=True, capture_output=True,
        text=True, timeout=10,
    )
    rust_version = _clean_version(args.expected_version, rust_version_result.stdout)

    report = {
        "schema": 1,
        "status": "incomparable",
        "comparison_enabled": False,
        "environment": _environment(),
        "versions": {
            "python_emp": python_version,
            "python_executable": str(python),
            "python_root": str(python_root),
            "rust_emp": rust_version,
            "rust_binary": str(rust_binary),
        },
        "parameters": {
            "iterations": args.iterations,
            "warmup": args.warmup,
            "concurrency": args.concurrency,
            "payload_sizes_bytes": args.payload_sizes,
            "large_history_sizes_mib": args.large_history_sizes,
            "large_iterations": args.large_iterations,
            "upstream_delay_ms": 0,
            "measurement_order": args.order,
        },
        "preflight": {},
        "measurements": None,
        "errors": [],
    }

    with tempfile.TemporaryDirectory(prefix="emp-benchmark-") as directory:
        work_root = Path(directory)
        key_file = work_root / "master.key"
        key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
        os.chmod(key_file, 0o600)
        base_env = dict(os.environ)
        for key in EXTERNAL_CREDENTIAL_ENV:
            base_env.pop(key, None)
        base_env["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(key_file)
        fake = FakeResponsesUpstream()
        try:
            config_bytes = _normalized_config(
                python, python_root, python_env, fake.base_url, work_root
            )
            py_result, py_errors = _serve_once(
                "python", [str(python), "-m", "easy_multi_provider"],
                python_root, config_bytes, work_root, key_file, base_env,
                python_root, fake, args.iterations, args.warmup,
                args.concurrency, args.payload_sizes, args.large_history_sizes,
                args.large_iterations, measure=False,
            )
            report["preflight"]["python"] = py_result
            rust_result, rust_errors = _serve_once(
                "rust", [str(rust_binary)], Path.cwd(), config_bytes,
                work_root, key_file, base_env, python_root, fake,
                args.iterations, args.warmup, args.concurrency,
                args.payload_sizes, args.large_history_sizes, args.large_iterations,
                measure=False,
            )
            report["preflight"]["rust"] = rust_result
            differences = semantic_diff(
                py_result.get("semantics"), rust_result.get("semantics")
            )
            if py_errors or rust_errors:
                report["errors"] = ["python_preflight_failed" if py_errors else None,
                                     "rust_preflight_failed" if rust_errors else None]
                report["errors"] = [value for value in report["errors"] if value]
            if differences:
                report["errors"].append("semantic_mismatch")
                report["semantic_difference_paths"] = differences
            if report["errors"]:
                return report, 2

            report["comparison_enabled"] = True
            report["status"] = "measured"
            fake.reset_counts()
            measurements = {}
            measurement_targets = [
                ("python", [str(python), "-m", "easy_multi_provider"], python_root),
                ("rust", [str(rust_binary)], Path.cwd()),
            ]
            if args.order == "rust-first":
                measurement_targets.reverse()
            for name, command, cwd in measurement_targets:
                result, errors = _serve_once(
                    name, command, cwd, config_bytes, work_root, key_file,
                    base_env, python_root, fake, args.iterations, args.warmup,
                    args.concurrency, args.payload_sizes, args.large_history_sizes,
                    args.large_iterations, measure=True,
                )
                measurements[name] = result
                if errors:
                    report["errors"].extend([name + "_measurement_error"])
                for case, values in result["cases"].items():
                    if values["errors"] or not values["upstream_count_matches"]:
                        report["errors"].append(name + "_invalid_case_" + case)
            report["measurements"] = measurements
            report["upstream"] = {
                "delay_ms": 0,
                "requests": fake.request_count,
                "model_counts": dict(fake.model_counts),
                "errors": fake.error_count,
            }
            if fake.error_count:
                report["errors"].append("fake_upstream_rejections")
            if report["errors"]:
                report["status"] = "invalid_measurement"
                report["comparison_enabled"] = False
                return report, 2
            return report, 0
        finally:
            fake.close()


def main(argv=None) -> int:
    args = _parse_args(argv)
    try:
        report, status = run(args)
    except Exception as exc:
        report = {
            "schema": 1,
            "status": "incomparable",
            "comparison_enabled": False,
            "environment": _environment(),
            "measurements": None,
            "errors": [safe_error_name(exc)],
        }
        status = 2
    payload = json.dumps(report, sort_keys=True, indent=2)
    if args.output == Path("-"):
        print(payload)
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(payload + "\n", encoding="utf-8")
    return status


if __name__ == "__main__":
    raise SystemExit(main())
