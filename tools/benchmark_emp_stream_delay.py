#!/usr/bin/env python3
"""Measure scheduled fake-upstream to downstream HTTP SSE event delay.

This runner uses the existing runtime benchmark's process/bootstrap fixture,
then sends identical streamed Responses requests through real Python and Rust
EMP processes. Reports contain event types, timing, versions, and safe error
codes only; no request content, response content, or credentials are written.
"""

from __future__ import annotations

import argparse
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
import base64
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time

try:
    from . import benchmark_emp_runtime as benchmark
except ImportError:  # Running as `python tools/benchmark_emp_stream_delay.py`.
    tools_dir = str(Path(__file__).resolve().parent)
    if tools_dir not in sys.path:
        sys.path.insert(0, tools_dir)
    import benchmark_emp_runtime as benchmark


TEXT_SENTINEL = "SCHEDULED_TEXT_EVENT"
REASONING_SENTINEL = "SCHEDULED_REASONING_EVENT"
TOOL_ARGUMENTS = '{"query":"scheduled-fixture"}'
EVENT_SPACING_MS = 6
DEFAULT_ITERATIONS = 30
DEFAULT_WARMUP = 3
DEFAULT_P95_LIMIT_MS = 5.0


def scheduled_events() -> list[dict]:
    response_id = "resp_scheduled_fixture"
    call_id = "call_scheduled_fixture"
    function_id = "fc_scheduled_fixture"
    message_id = "msg_scheduled_fixture"
    function = {
        "id": function_id,
        "type": "function_call",
        "status": "completed",
        "call_id": call_id,
        "name": "scheduled_tool",
        "arguments": TOOL_ARGUMENTS,
    }
    message = {
        "id": message_id,
        "type": "message",
        "status": "completed",
        "role": "assistant",
        "content": [{"type": "output_text", "text": TEXT_SENTINEL, "annotations": []}],
    }
    values = [
        ("response.created", {"response": {
            "id": response_id, "object": "response", "status": "in_progress",
            "model": benchmark.UPSTREAM_MODEL, "output": [],
        }}),
        ("response.reasoning_summary_text.delta", {
            "item_id": "reasoning_scheduled_fixture", "delta": REASONING_SENTINEL,
        }),
        ("response.output_item.added", {"output_index": 0, "item": {
            **function, "status": "in_progress", "arguments": "",
        }}),
        ("response.function_call_arguments.delta", {
            "item_id": function_id, "output_index": 0, "call_id": call_id,
            "delta": TOOL_ARGUMENTS,
        }),
        ("response.function_call_arguments.done", {
            "item_id": function_id, "output_index": 0, "call_id": call_id,
            "arguments": TOOL_ARGUMENTS,
        }),
        ("response.output_item.done", {"output_index": 0, "item": function}),
        ("response.output_item.added", {"output_index": 1, "item": {
            **message, "status": "in_progress", "content": [],
        }}),
        ("response.content_part.added", {
            "item_id": message_id, "output_index": 1, "content_index": 0,
            "part": {"type": "output_text", "text": "", "annotations": []},
        }),
        ("response.output_text.delta", {
            "item_id": message_id, "output_index": 1, "content_index": 0,
            "delta": TEXT_SENTINEL,
        }),
        ("response.output_text.done", {
            "item_id": message_id, "output_index": 1, "content_index": 0,
            "text": TEXT_SENTINEL,
        }),
        ("response.content_part.done", {
            "item_id": message_id, "output_index": 1, "content_index": 0,
            "part": message["content"][0],
        }),
        ("response.output_item.done", {"output_index": 1, "item": message}),
        ("response.completed", {"response": {
            "id": response_id, "object": "response", "status": "completed",
            "model": benchmark.UPSTREAM_MODEL, "output": [function, message],
            "usage": {
                "input_tokens": 8,
                "input_tokens_details": {"cached_tokens": 0},
                "output_tokens": 3,
                "output_tokens_details": {"reasoning_tokens": 1},
                "total_tokens": 11,
            },
        }}),
    ]
    return [
        {"type": kind, "sequence_number": sequence, **payload}
        for sequence, (kind, payload) in enumerate(values)
    ]


SCHEDULED_EVENTS = scheduled_events()
EXPECTED_TYPES = [event["type"] for event in SCHEDULED_EVENTS]


def validate_parameters(iterations: int, warmup: int, p95_limit_ms: float) -> None:
    if iterations < 1 or warmup < 0:
        raise benchmark.BenchmarkError("invalid_iteration_count")
    if p95_limit_ms <= 0:
        raise benchmark.BenchmarkError("invalid_p95_limit")


def _event_semantics(events: list[dict]) -> dict:
    types = [event.get("type") for event in events]
    sequences = [event.get("sequence_number") for event in events]
    reasoning = "".join(
        event.get("delta", "") for event in events
        if event.get("type") == "response.reasoning_summary_text.delta"
    )
    text = "".join(
        event.get("delta", "") for event in events
        if event.get("type") == "response.output_text.delta"
    )
    tool_arguments = "".join(
        event.get("delta", "") for event in events
        if event.get("type") == "response.function_call_arguments.delta"
    )
    terminal = events[-1] if events else {}
    response = terminal.get("response", {}) if isinstance(terminal, dict) else {}
    output = response.get("output", []) if isinstance(response, dict) else []
    output_types = [item.get("type") for item in output if isinstance(item, dict)]
    return {
        "event_types": types,
        "sequence_numbers": sequences,
        "ordered": types == EXPECTED_TYPES and sequences == list(range(len(EXPECTED_TYPES))),
        "reasoning_present": reasoning == REASONING_SENTINEL,
        "text_present": text == TEXT_SENTINEL,
        "tool_arguments_present": tool_arguments == TOOL_ARGUMENTS,
        "terminal": response.get("status") if isinstance(response, dict) else None,
        "output_types": output_types,
        "completed": (
            terminal.get("type") == "response.completed"
            and response.get("status") == "completed"
        ),
        "output_pair_present": output_types == ["function_call", "message"],
    }


def _latency_summary(samples_ms: list[float]) -> dict:
    return benchmark.latency_summary(samples_ms, 0.0, 0)


class _ScheduledHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        server = self.server
        with server._lock:
            server.request_count += 1
        try:
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError, json.JSONDecodeError):
            server._record_error()
            self.send_error(400)
            return
        status = benchmark._fake_upstream_status(
            self.path, self.headers.get("Authorization"), body
        )
        if status != 200 or body.get("stream") is not True:
            server._record_error()
            self.send_error(status if status != 200 else 400)
            return
        with server._lock:
            server.model_counts[body.get("model", "")] = (
                server.model_counts.get(body.get("model", ""), 0) + 1
            )
        events = scheduled_events()
        frames = [
            ("event: " + event["type"] + "\ndata: "
             + json.dumps(event, separators=(",", ":")) + "\n\n").encode()
            for event in events
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(sum(map(len, frames))))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.flush()
        start_ns = time.monotonic_ns()
        for event, frame in zip(events, frames, strict=True):
            due_ns = start_ns + event["sequence_number"] * EVENT_SPACING_MS * 1_000_000
            delay_ns = due_ns - time.monotonic_ns()
            if delay_ns > 0:
                time.sleep(delay_ns / 1_000_000_000)
            sent_ns = time.monotonic_ns()
            with server._lock:
                # Publish the timestamp before writing. A fast downstream can
                # observe the frame as soon as flush returns on this thread.
                server.due_times[event["sequence_number"]] = due_ns
                server.send_times[event["sequence_number"]] = sent_ns
            try:
                self.wfile.write(frame)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                return

    def log_message(self, *_args):
        pass


class ScheduledResponsesUpstream(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _ScheduledHandler)
        self._lock = threading.Lock()
        self.request_count = 0
        self.error_count = 0
        self.model_counts = {}
        self.due_times = {}
        self.send_times = {}
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server_port}/v1"

    def reset_counts(self):
        with self._lock:
            self.request_count = 0
            self.error_count = 0
            self.model_counts.clear()
            self.due_times.clear()
            self.send_times.clear()

    def _record_error(self):
        with self._lock:
            self.error_count += 1

    def snapshot(self):
        with self._lock:
            return {
                "requests": self.request_count,
                "errors": self.error_count,
                "models": dict(self.model_counts),
                "due_times": dict(self.due_times),
                "send_times": dict(self.send_times),
            }

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=3)


def _read_sse(port: int, cookie: str, stream_marker: str) -> dict:
    import http.client

    payload = json.dumps({
        "model": "benchmark/model",
        "input": "stream-delay-" + stream_marker,
        "stream": True,
    }, separators=(",", ":")).encode()
    headers = benchmark._request_headers(cookie=cookie, payload_present=True)
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=15)
    observed = []
    try:
        connection.request("POST", "/v1/responses", body=payload, headers=headers)
        response = connection.getresponse()
        status = response.status
        if status != 200 or "text/event-stream" not in response.getheader("Content-Type", ""):
            return {"status": status, "events": observed, "error": "invalid_sse_response"}
        data_lines = []
        while True:
            line = response.readline()
            if not line:
                if data_lines:
                    raw = b"\n".join(data_lines)
                    observed.append((json.loads(raw), time.monotonic_ns()))
                break
            if line in (b"\n", b"\r\n"):
                if data_lines:
                    raw = b"\n".join(data_lines)
                    observed.append((json.loads(raw), time.monotonic_ns()))
                    data_lines = []
                    if observed[-1][0].get("type") == "response.completed":
                        break
            elif line.startswith(b"data:"):
                data_lines.append(line[5:].lstrip().rstrip(b"\r\n"))
        return {"status": status, "events": observed, "error": None}
    except (OSError, TimeoutError):
        return {"status": None, "events": observed, "error": "sse_io_error"}
    except (UnicodeDecodeError, json.JSONDecodeError):
        return {"status": 200, "events": observed, "error": "sse_json_error"}
    finally:
        connection.close()


def _one_iteration(service, upstream: ScheduledResponsesUpstream, marker: str,
                   collect_timings: bool = True) -> dict:
    upstream.reset_counts()
    result = _read_sse(service.port, service.cookie, marker)
    snapshot = upstream.snapshot()
    events = [value for value, _stamp in result["events"]]
    observed_ns = [stamp for _value, stamp in result["events"]]
    semantics = _event_semantics(events)
    timings = _event_timings(events, observed_ns, snapshot) if collect_timings else []
    valid = (
        result["status"] == 200 and result["error"] is None
        and semantics["ordered"] and semantics["reasoning_present"]
        and semantics["text_present"] and semantics["tool_arguments_present"]
        and semantics["completed"] and semantics["output_pair_present"]
        and snapshot["requests"] == 1 and snapshot["errors"] == 0
        and snapshot["models"] == {benchmark.UPSTREAM_MODEL: 1}
        and (not collect_timings or len(timings) == len(EXPECTED_TYPES))
    )
    return {
        "status": result["status"],
        "error": result["error"],
        "semantics": semantics,
        "timings": timings,
        "valid": valid,
        "upstream": {
            "requests": snapshot["requests"],
            "errors": snapshot["errors"],
            "model_counts": snapshot["models"],
        },
    }


def _event_timings(events: list[dict], observed_ns: list[int], snapshot: dict) -> list[dict]:
    if len(events) != len(observed_ns):
        return []
    timings = []
    for event, observed in zip(events, observed_ns, strict=True):
        sequence = event.get("sequence_number")
        sent = snapshot["send_times"].get(sequence)
        due = snapshot["due_times"].get(sequence)
        if sent is not None and due is not None:
            timings.append({
                "event_type": event.get("type"),
                "added_delay_ms": max(0.0, (observed - due) / 1_000_000),
                "upstream_send_to_downstream_ms": max(
                    0.0, (observed - sent) / 1_000_000
                ),
                "schedule_lateness_ms": max(0.0, (sent - due) / 1_000_000),
            })
    return timings


def _runtime_measurement(name: str, command: list[str], cwd: Path, config: bytes,
                         work_root: Path, key_file: Path, base_env: dict,
                         python_root: Path, upstream: ScheduledResponsesUpstream,
                         iterations: int, warmup: int) -> dict:
    config_path = work_root / (name + "-stream-delay.json")
    config_path.write_bytes(config)
    home = work_root / (name + "-stream-delay-home")
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    service = benchmark.EmpService(
        command, cwd, config_path, home, env, name
    )
    semantics = None
    errors = []
    delays_by_type = {}
    schedule_lateness_by_type = {}
    all_delays = []
    all_schedule_lateness = []
    upstream_total = 0
    upstream_errors = 0
    with service:
        for index in range(warmup + iterations):
            result = _one_iteration(service, upstream, f"{name}-{index}")
            if not result["valid"]:
                errors.append("stream_semantic_or_upstream_count")
            current_semantics = result["semantics"]
            if semantics is None:
                semantics = current_semantics
            elif current_semantics != semantics:
                errors.append("stream_semantics_changed_between_iterations")
            upstream_total += result["upstream"]["requests"]
            upstream_errors += result["upstream"]["errors"]
            if index >= warmup:
                for timing in result["timings"]:
                    delay = timing["added_delay_ms"]
                    all_delays.append(delay)
                    delays_by_type.setdefault(timing["event_type"], []).append(delay)
                    lateness = timing["schedule_lateness_ms"]
                    all_schedule_lateness.append(lateness)
                    schedule_lateness_by_type.setdefault(timing["event_type"], []).append(lateness)
    expected_requests = warmup + iterations
    if upstream_total != expected_requests:
        errors.append("upstream_request_count")
    if upstream_errors:
        errors.append("upstream_rejected_request")
    return {
        "iterations": iterations,
        "warmup": warmup,
        "semantics": semantics,
        "added_delay": {
            "all_events": _latency_summary(all_delays),
            "by_event_type": {
                kind: _latency_summary(values)
                for kind, values in sorted(delays_by_type.items())
            },
        },
        "schedule_lateness": {
            "all_events": _latency_summary(all_schedule_lateness),
            "by_event_type": {
                kind: _latency_summary(values)
                for kind, values in sorted(schedule_lateness_by_type.items())
            },
        },
        "upstream": {
            "requests": upstream_total,
            "expected_requests": expected_requests,
            "errors": upstream_errors,
        },
        "errors": errors,
    }


def _runtime_preflight(name: str, command: list[str], cwd: Path, config: bytes,
                       work_root: Path, key_file: Path, base_env: dict,
                       python_root: Path, upstream: ScheduledResponsesUpstream) -> dict:
    config_path = work_root / (name + "-stream-delay-preflight.json")
    config_path.write_bytes(config)
    home = work_root / (name + "-stream-delay-preflight-home")
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    service = benchmark.EmpService(command, cwd, config_path, home, env, name)
    with service:
        result = _one_iteration(service, upstream, name + "-preflight", collect_timings=False)
    return {
        "semantics": result["semantics"],
        "valid": result["valid"],
        "status": result["status"],
        "upstream": result["upstream"],
        "error": result["error"],
    }


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=benchmark.DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--expected-version", default="0.11.10")
    parser.add_argument("--iterations", type=int, default=DEFAULT_ITERATIONS)
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    parser.add_argument("--p95-limit-ms", type=float, default=DEFAULT_P95_LIMIT_MS)
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    return parser.parse_args(argv)


def _normalized_config_with_reasoning(python: Path, python_root: Path,
                                     python_env: dict, upstream_url: str,
                                     work_root: Path) -> bytes:
    normalized = json.loads(benchmark._normalized_config(
        python, python_root, python_env, upstream_url, work_root
    ))
    models = normalized.get("models")
    if not isinstance(models, list) or not models or not isinstance(models[0], dict):
        raise benchmark.BenchmarkError("benchmark_model_missing")
    model = models[0]
    model["supports_reasoning_summaries"] = True
    script = (
        "import json,sys; from easy_multi_provider.config import normalize; "
        "print(json.dumps(normalize(json.load(sys.stdin)), separators=(',', ':')))"
    )
    result = subprocess.run(
        [str(python), "-c", script], cwd=python_root, env=python_env,
        input=json.dumps(normalized), check=True, capture_output=True,
        text=True, timeout=15,
    )
    config = json.loads(result.stdout)
    if not config.get("models") or config["models"][0].get("supports_reasoning_summaries") is not True:
        raise benchmark.BenchmarkError("reasoning_capability_not_preserved")
    return result.stdout.strip().encode()


def run(args) -> tuple[dict, int]:
    if benchmark.psutil is None:
        raise benchmark.BenchmarkError("psutil_required")
    validate_parameters(args.iterations, args.warmup, args.p95_limit_ms)
    python_root = args.python_root.resolve(strict=True)
    python = benchmark._python_launcher(args.python, python_root)
    rust_binary = args.rust_binary.resolve(strict=True)
    python_env = dict(os.environ)
    python_env["PYTHONPATH"] = str(python_root)
    python_version = benchmark._python_version(python, python_root, python_env)
    if python_version != args.expected_version:
        raise benchmark.BenchmarkError("python_version_mismatch")
    version_result = subprocess.run(
        [str(rust_binary), "--version"], check=True, capture_output=True,
        text=True, timeout=10,
    )
    rust_version = benchmark._clean_version(args.expected_version, version_result.stdout)

    report = {
        "schema": 1,
        "status": "incomparable",
        "comparison_enabled": False,
        "environment": benchmark._environment(),
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
            "event_spacing_ms": EVENT_SPACING_MS,
            "p95_limit_ms": args.p95_limit_ms,
            "measurement_order": args.order,
            "transport": "http_sse",
        },
        "semantics": {},
        "preflight": {},
        "measurements": None,
        "errors": [],
    }

    with tempfile.TemporaryDirectory(prefix="emp-stream-delay-") as directory:
        work_root = Path(directory)
        key_file = work_root / "master.key"
        key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
        os.chmod(key_file, 0o600)
        base_env = dict(os.environ)
        for key in benchmark.EXTERNAL_CREDENTIAL_ENV:
            base_env.pop(key, None)
        base_env["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(key_file)
        upstream = ScheduledResponsesUpstream()
        try:
            config = _normalized_config_with_reasoning(
                python, python_root, python_env, upstream.base_url, work_root
            )
            runtimes = [
                ("python", [str(python), "-m", "easy_multi_provider"], python_root),
                ("rust", [str(rust_binary)], Path.cwd()),
            ]
            if args.order == "rust-first":
                runtimes.reverse()
            for name, command, cwd in runtimes:
                result = _runtime_preflight(
                    name, command, cwd, config, work_root, key_file,
                    base_env, python_root, upstream,
                )
                report["preflight"][name] = result
                report["semantics"][name] = result["semantics"]
                if not result["valid"]:
                    report["errors"].append(name + "_semantic_preflight_failed")
            differences = benchmark.semantic_diff(
                report["semantics"]["python"], report["semantics"]["rust"]
            )
            if differences:
                report["semantic_difference_paths"] = differences
                report["errors"].append("runtime_semantic_mismatch")
            if report["errors"]:
                return report, 2

            measurements = {}
            for name, command, cwd in runtimes:
                measurements[name] = _runtime_measurement(
                    name, command, cwd, config, work_root, key_file,
                    base_env, python_root, upstream, args.iterations, args.warmup,
                )
                if measurements[name]["semantics"] != report["semantics"][name]:
                    measurements[name]["errors"].append("measurement_semantics_changed")
                report["errors"].extend(
                    [name + "_" + error for error in measurements[name]["errors"]]
                )
            report["measurements"] = measurements
            if report["errors"]:
                return report, 2
            report["comparison_enabled"] = True
            report["status"] = "measured"
            for name, values in measurements.items():
                p95 = values["added_delay"]["all_events"]["p95_ms"]
                if p95 is None or p95 > args.p95_limit_ms:
                    report["errors"].append(name + "_added_delay_p95_gate")
            if report["errors"]:
                report["status"] = "invalid_measurement"
                report["comparison_enabled"] = False
                return report, 2
            return report, 0
        finally:
            upstream.shutdown()
            upstream.server_close()
            upstream.thread.join(timeout=3)


def main(argv=None) -> int:
    args = _parse_args(argv)
    try:
        report, exit_code = run(args)
    except Exception as exc:
        report = {
            "schema": 1,
            "status": "incomparable",
            "comparison_enabled": False,
            "errors": [benchmark.safe_error_name(exc)],
        }
        exit_code = 2
    output = json.dumps(report, sort_keys=True, indent=2)
    if args.output == Path("-"):
        print(output)
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output + "\n", encoding="utf-8")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
