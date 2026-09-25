#!/usr/bin/env python3
"""Compare process-level HTTP SSE cancellation and resource release.

The runner disconnects real downstream clients at three boundaries: before
upstream response headers, after SSE headers but before the first event, and
after an output delta. A local fake Responses server observes whether EMP
closes its upstream socket. Reports contain safe event types and resource
metrics only; request bodies, response text, and credentials are never saved.
"""

from __future__ import annotations

import argparse
import base64
from dataclasses import dataclass, field
import http.client
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
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
except ImportError:  # Running as `python tools/benchmark_emp_cancellation.py`.
    tools_dir = str(Path(__file__).resolve().parent)
    if tools_dir not in sys.path:
        sys.path.insert(0, tools_dir)
    import benchmark_emp_runtime as benchmark


SCENARIOS = ("before_headers", "before_first_event", "after_delta")
RELEASE_TIMEOUT_SECONDS = 5.0
FAKE_UPSTREAM_IDLE_TIMEOUT_SECONDS = 60.0
RETRY_QUIET_SECONDS = 0.10
POLL_INTERVAL_SECONDS = 0.025
RSS_SLACK_BYTES = 16 * 1024 * 1024
THREAD_SLACK = 2
DESCRIPTOR_SLACK = 4
RUST_RELEASE_P99_LIMIT_MS = 1000.0
PYTHON_REFERENCE_LIMITATION_ERRORS = frozenset({
    "upstream_socket_not_closed",
    "all_release_conditions_not_observed_within_timeout",
})
REPLY_TEXT = benchmark.REPLY_TEXT


def validate_parameters(iterations: int, warmup: int, release_timeout: float) -> None:
    if not 1 <= iterations <= 1000 or not 0 <= warmup <= 100:
        raise benchmark.BenchmarkError("invalid_iteration_count")
    if release_timeout <= 0 or release_timeout > RELEASE_TIMEOUT_SECONDS:
        raise benchmark.BenchmarkError("invalid_release_timeout")


def resource_release_ok(before: dict, after: dict) -> bool:
    """Require RSS, thread count, and descriptors to return near baseline."""
    rss_before, rss_after = before.get("rss_bytes"), after.get("rss_bytes")
    threads_before, threads_after = before.get("threads"), after.get("threads")
    fds_before, fds_after = before.get("descriptors"), after.get("descriptors")
    if any(value is None for value in (
        rss_before, rss_after, threads_before, threads_after, fds_before, fds_after,
    )):
        return False
    return (
        rss_after <= rss_before + RSS_SLACK_BYTES
        and threads_after <= threads_before + THREAD_SLACK
        and fds_after <= fds_before + DESCRIPTOR_SLACK
    )


def percentile_report(samples_ms: list[float]) -> dict:
    return {
        "count": len(samples_ms),
        "p50_ms": benchmark.percentile(samples_ms, 0.50),
        "p95_ms": benchmark.percentile(samples_ms, 0.95),
        "p99_ms": benchmark.percentile(samples_ms, 0.99),
    }


def classify_runtime_errors(runtime: str, prefix: str, values: list[str]) -> tuple[list[str], list[str]]:
    """Keep Python's known no-cancel baseline visible without gating Rust."""
    fatal, reference_only = [], []
    for error in values:
        rendered = prefix + error
        if runtime == "python" and error in PYTHON_REFERENCE_LIMITATION_ERRORS:
            reference_only.append(rendered)
        else:
            fatal.append(rendered)
    return fatal, reference_only


def aggregate_plan_requests(plans: dict, existing_markers: set[str] | None = None) -> dict:
    """Count attempts by preflight/warmup/measurement without serializing markers."""
    existing_markers = existing_markers or set()
    phases = {
        "preflight": {"plan_count": 0, "request_attempts": 0, "extra_attempts": 0},
        "warmup": {
            "plan_count": 0, "request_attempts": 0, "extra_attempts": 0,
            "by_scenario": {},
        },
        "measurement": {
            "plan_count": 0, "request_attempts": 0, "extra_attempts": 0,
            "by_scenario": {},
        },
    }
    for marker, plan in plans.items():
        if marker in existing_markers:
            continue
        prefix = marker.split("-", 1)[0]
        phase = {"measure": "measurement"}.get(prefix, prefix)
        if phase not in phases:
            continue
        requests = plan.snapshot()["requests"]
        record = phases[phase]
        record["plan_count"] += 1
        record["request_attempts"] += requests
        record["extra_attempts"] += max(0, requests - 1)
        if phase != "preflight":
            scenario = next((value for value in SCENARIOS if value in marker), "unknown")
            by_scenario = record["by_scenario"].setdefault(
                scenario,
                {"plan_count": 0, "request_attempts": 0, "extra_attempts": 0},
            )
            by_scenario["plan_count"] += 1
            by_scenario["request_attempts"] += requests
            by_scenario["extra_attempts"] += max(0, requests - 1)
    return phases


def phase_observed(scenario: str, downstream: dict, upstream_headers_sent: bool) -> bool:
    if scenario == "before_headers":
        return (
            downstream.get("status") is None
            and not downstream.get("event_types")
            and not upstream_headers_sent
        )
    if scenario == "before_first_event":
        return (
            downstream.get("status") is None
            and downstream.get("content_type") is None
            and not downstream.get("event_types")
            and upstream_headers_sent
        )
    if scenario == "after_delta":
        return (
            downstream.get("status") == 200
            and downstream.get("content_type") == "text/event-stream"
            and downstream.get("output_delta_seen") is True
            and upstream_headers_sent
            and downstream.get("event_types") == [
                kind for kind, _wire in _sse_prefix_events()
            ]
        )
    return False


def _sse_events() -> list[tuple[str, bytes]]:
    """Return the complete standard Responses SSE fixture."""
    events = []
    for block in benchmark._sse_payload(benchmark.UPSTREAM_MODEL).split(b"\n\n"):
        if not block:
            continue
        raw = next((line[6:] for line in block.splitlines() if line.startswith(b"data: ")), None)
        if raw is None:
            continue
        value = json.loads(raw)
        events.append((value["type"], block + b"\n\n"))
    return events


def _sse_prefix_events() -> list[tuple[str, bytes]]:
    events = []
    for event in _sse_events():
        events.append(event)
        if event[0] == "response.output_text.delta":
            return events
    raise benchmark.BenchmarkError("fixture_output_delta_missing")


@dataclass
class UpstreamObservation:
    scenario: str
    marker: str = field(repr=False)
    request_count: int = 0
    rejected_count: int = 0
    status: int | None = None
    headers_sent: bool = False
    output_delta_sent: bool = False
    peer_closed: bool = False
    peer_close_kind: str | None = None
    peer_close_elapsed_ms: float | None = None
    unexpected_request_shape: bool = False
    lock: threading.Lock = field(default_factory=threading.Lock, repr=False)
    request_seen: threading.Event = field(default_factory=threading.Event, repr=False)
    headers_ready: threading.Event = field(default_factory=threading.Event, repr=False)
    delta_ready: threading.Event = field(default_factory=threading.Event, repr=False)
    peer_closed_event: threading.Event = field(default_factory=threading.Event, repr=False)
    started_ns: int = field(default_factory=time.monotonic_ns, repr=False)

    def snapshot(self) -> dict:
        with self.lock:
            return {
                "scenario": self.scenario,
                "requests": self.request_count,
                "rejected_requests": self.rejected_count,
                "status": self.status,
                "headers_sent": self.headers_sent,
                "output_delta_sent": self.output_delta_sent,
                "peer_closed": self.peer_closed,
                "peer_close_kind": self.peer_close_kind,
                "peer_close_elapsed_ms": self.peer_close_elapsed_ms,
                "unexpected_request_shape": self.unexpected_request_shape,
            }


class _CancellationUpstreamHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        try:
            raw = self.rfile.read(int(self.headers.get("Content-Length", "0")))
            body = json.loads(raw)
        except (ValueError, UnicodeDecodeError, json.JSONDecodeError):
            self.server.record_error()
            self.send_error(400)
            return
        marker = body.get("input") if isinstance(body, dict) else None
        plan = self.server.plan_for_marker(marker)
        if plan is None:
            self.server.record_unplanned_request()
            self.send_error(503)
            return
        with plan.lock:
            plan.request_count += 1
            if plan.request_count > 1:
                plan.rejected_count += 1
        plan.request_seen.set()
        status = benchmark._fake_upstream_status(
            self.path, self.headers.get("Authorization"), body
        )
        valid = status == 200 and body.get("stream") is True
        if not valid:
            with plan.lock:
                plan.unexpected_request_shape = True
                plan.status = status if status != 200 else 400
            self.server.record_error()
            self.send_error(status if status != 200 else 400)
            return
        if plan.scenario == "before_headers":
            self._wait_for_disconnect(plan)
            return

        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Cache-Control", "no-cache")
        self.send_header("Connection", "close")
        all_events = _sse_events()
        complete_body = b"".join(wire for _kind, wire in all_events)
        if plan.scenario == "complete":
            self.send_header("Content-Length", str(len(complete_body)))
        self.end_headers()
        self.wfile.flush()
        with plan.lock:
            plan.status = 200
            plan.headers_sent = True
        plan.headers_ready.set()

        if plan.scenario == "complete":
            try:
                self.wfile.write(complete_body)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError, OSError):
                self._mark_peer_closed(plan, "write_error")
            return

        if plan.scenario == "before_first_event":
            self._wait_for_disconnect(plan)
            return

        try:
            for event_type, wire in _sse_prefix_events():
                self.wfile.write(wire)
                self.wfile.flush()
                if event_type == "response.output_text.delta":
                    with plan.lock:
                        plan.output_delta_sent = True
                    plan.delta_ready.set()
                    self._wait_for_disconnect(plan)
                    return
        except (BrokenPipeError, ConnectionResetError, OSError):
            self._mark_peer_closed(plan, "write_error")

    def _wait_for_disconnect(self, plan: UpstreamObservation):
        # Keep the fake peer open beyond the 5s observation gate. Timing out
        # here would synthesize an upstream transport failure and may trigger
        # the runtime's ordinary retry path during this cancellation test.
        self.connection.settimeout(FAKE_UPSTREAM_IDLE_TIMEOUT_SECONDS)
        try:
            while True:
                incoming = self.connection.recv(1)
                if incoming == b"":
                    self._mark_peer_closed(plan, "eof")
                    return
                # No application data is expected after the HTTP request body.
                # Ignore any buffered bytes and continue until EOF/reset.
        except (ConnectionResetError, BrokenPipeError):
            self._mark_peer_closed(plan, "reset")
        except TimeoutError:
            return
        except OSError:
            self._mark_peer_closed(plan, "socket_error")

    @staticmethod
    def _mark_peer_closed(plan: UpstreamObservation, kind: str):
        with plan.lock:
            plan.peer_closed = True
            plan.peer_close_kind = kind
            plan.peer_close_elapsed_ms = (time.monotonic_ns() - plan.started_ns) / 1_000_000
        plan.peer_closed_event.set()

    def log_message(self, *_args):
        pass


class CancellationFakeUpstream(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 64

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _CancellationUpstreamHandler)
        self.plans: dict[str, UpstreamObservation] = {}
        self.unplanned_requests = 0
        self.error_count = 0
        self.lock = threading.Lock()
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self):
        return f"http://127.0.0.1:{self.server_port}/v1"

    def begin(self, scenario: str, marker: str) -> UpstreamObservation:
        if scenario not in (*SCENARIOS, "complete"):
            raise benchmark.BenchmarkError("invalid_cancellation_scenario")
        plan = UpstreamObservation(scenario, marker)
        with self.lock:
            if marker in self.plans:
                raise benchmark.BenchmarkError("duplicate_fixture_marker")
            self.plans[marker] = plan
        return plan

    def plan_for_marker(self, marker: str | None) -> UpstreamObservation | None:
        with self.lock:
            return self.plans.get(marker) if isinstance(marker, str) else None

    def record_unplanned_request(self):
        with self.lock:
            self.unplanned_requests += 1

    def record_error(self):
        with self.lock:
            self.error_count += 1

    def snapshot_counts(self) -> dict:
        with self.lock:
            return {
                "unplanned_requests": self.unplanned_requests,
                "errors": self.error_count,
                "planned_requests": sum(plan.snapshot()["requests"] for plan in self.plans.values()),
            }

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=3)


def downstream_disconnect(port: int, cookie: str, plan: UpstreamObservation,
                          timeout: float = RELEASE_TIMEOUT_SECONDS) -> dict:
    """Issue a real HTTP stream request and close at the selected observation point."""
    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=timeout)
    payload = json.dumps({
        "model": "benchmark/model",
        "input": plan.marker,
        "stream": True,
    }, separators=(",", ":")).encode()
    headers = benchmark._request_headers(cookie=cookie, payload_present=True)
    outcome = {"status": None, "content_type": None, "event_types": [], "output_delta_seen": False}
    try:
        connection.request("POST", "/v1/responses", body=payload, headers=headers)
        if plan.scenario == "before_headers":
            if not plan.request_seen.wait(timeout):
                raise benchmark.BenchmarkError("upstream_request_not_observed")
            return outcome
        if plan.scenario == "before_first_event":
            if not plan.headers_ready.wait(timeout):
                raise benchmark.BenchmarkError("upstream_headers_not_observed")
            return outcome
        if plan.scenario != "after_delta":
            raise benchmark.BenchmarkError("invalid_cancellation_scenario")
        response = connection.getresponse()
        outcome["status"] = response.status
        outcome["content_type"] = response.getheader("Content-Type", "").split(";", 1)[0].lower()
        if response.status != 200 or outcome["content_type"] != "text/event-stream":
            return outcome
        data_lines: list[bytes] = []
        while True:
            line = response.readline()
            if not line:
                break
            if line in (b"\n", b"\r\n"):
                if data_lines:
                    event = json.loads(b"\n".join(data_lines))
                    kind = event.get("type")
                    if isinstance(kind, str):
                        outcome["event_types"].append(kind)
                    data_lines = []
                    if kind == "response.output_text.delta":
                        outcome["output_delta_seen"] = True
                        break
            elif line.startswith(b"data:"):
                data_lines.append(line[5:].lstrip().rstrip(b"\r\n"))
        if not outcome["output_delta_seen"]:
            raise benchmark.BenchmarkError("downstream_output_delta_missing")
        if not plan.delta_ready.wait(timeout):
            raise benchmark.BenchmarkError("upstream_output_delta_not_observed")
        return outcome
    except (OSError, TimeoutError, http.client.HTTPException):
        raise benchmark.BenchmarkError("downstream_disconnect_io_error") from None
    finally:
        connection.close()
        outcome["client_closed_ns"] = time.monotonic_ns()


def _read_limit_snapshot(service) -> dict:
    result = benchmark._request(
        service.port, "GET", "/api/request-limits", cookie=service.cookie,
        operation="request_limits_snapshot", timeout=3.0,
    )
    if result["status"] != 200:
        raise benchmark.BenchmarkError("request_limits_status")
    try:
        snapshot = json.loads(result["body"])
        reserved = snapshot["reserved_bytes"]
    except (ValueError, KeyError, TypeError):
        raise benchmark.BenchmarkError("request_limits_shape") from None
    if not isinstance(reserved, int) or reserved < 0:
        raise benchmark.BenchmarkError("request_limits_reserved_bytes")
    return {"reserved_bytes": reserved}


def _management_semantic(result: dict) -> dict:
    try:
        body = json.loads(result["body"])
        return {"status": result["status"], "reserved_bytes": body.get("reserved_bytes")}
    except (ValueError, TypeError):
        return {"status": result["status"], "shape": "invalid"}


def _wait_for_release(service, plan: UpstreamObservation, before_resources: dict,
                      baseline_reserved: int, timeout: float,
                      disconnected_ns: int) -> tuple[float | None, dict, dict, dict]:
    deadline = time.monotonic() + timeout
    latest_resources = benchmark._tree_metrics(service.process.pid)
    latest_limits = {"reserved_bytes": None}
    released_ns = None
    latest_conditions = {
        "upstream_socket_closed": False,
        "request_reservation_returned": False,
        "process_resources_returned": False,
    }
    while time.monotonic() <= deadline:
        try:
            latest_limits = _read_limit_snapshot(service)
        except benchmark.BenchmarkError:
            latest_limits = {"reserved_bytes": None}
        latest_resources = benchmark._tree_metrics(service.process.pid)
        latest_conditions = {
            "upstream_socket_closed": plan.peer_closed_event.is_set(),
            "request_reservation_returned": (
                latest_limits["reserved_bytes"] == baseline_reserved
            ),
            "process_resources_returned": resource_release_ok(
                before_resources, latest_resources
            ),
        }
        if all(latest_conditions.values()):
            released_ns = time.monotonic_ns()
            break
        time.sleep(POLL_INTERVAL_SECONDS)
    release_ms = (
        max(0.0, (released_ns - disconnected_ns) / 1_000_000)
        if released_ns is not None else None
    )
    return (
        release_ms,
        latest_resources,
        latest_limits,
        latest_conditions,
    )


def _run_one(service, upstream: CancellationFakeUpstream, scenario: str,
             timeout: float, marker: str) -> dict:
    plan = upstream.begin(scenario, marker)
    try:
        before_limits = _read_limit_snapshot(service)
    except benchmark.BenchmarkError as exc:
        return {"scenario": scenario, "errors": [exc.code], "release_ms": None}
    time.sleep(POLL_INTERVAL_SECONDS)
    before_resources = benchmark._tree_metrics(service.process.pid)
    try:
        downstream = downstream_disconnect(service.port, service.cookie, plan, timeout)
    except benchmark.BenchmarkError as exc:
        downstream = {"status": None, "content_type": None, "event_types": [], "output_delta_seen": False}
        disconnect_error = exc.code
    else:
        disconnect_error = None
    disconnected_ns = downstream.get("client_closed_ns", time.monotonic_ns())
    release_ms, after_resources, after_limits, release_conditions = _wait_for_release(
        service, plan, before_resources, before_limits["reserved_bytes"], timeout,
        disconnected_ns,
    )
    time.sleep(RETRY_QUIET_SECONDS)
    upstream_result = plan.snapshot()
    counts = upstream.snapshot_counts()
    errors = []
    if disconnect_error:
        errors.append(disconnect_error)
    if not release_conditions["upstream_socket_closed"]:
        errors.append("upstream_socket_not_closed")
    if upstream_result["requests"] != 1 or upstream_result["rejected_requests"]:
        errors.append("unexpected_upstream_retry")
    if upstream_result["unexpected_request_shape"] or counts["errors"]:
        errors.append("fake_upstream_rejected_request")
    if not release_conditions["request_reservation_returned"]:
        errors.append("request_reservation_not_released")
    if before_limits["reserved_bytes"] != 0:
        errors.append("request_reservation_baseline_nonzero")
    if not release_conditions["process_resources_returned"]:
        errors.append("process_resources_not_released")
    if release_ms is None:
        errors.append("all_release_conditions_not_observed_within_timeout")
    if not phase_observed(scenario, downstream, upstream_result["headers_sent"]):
        errors.append("downstream_disconnect_phase_mismatch")
    return {
        "scenario": scenario,
        "observation": {
            "status": downstream["status"],
            "content_type": downstream["content_type"],
            "event_types": downstream["event_types"],
            "output_delta_seen": downstream["output_delta_seen"],
            "upstream_headers_sent": upstream_result["headers_sent"],
        },
        "upstream": upstream_result,
        "request_limits": {
            "reserved_before": before_limits["reserved_bytes"],
            "reserved_after": after_limits.get("reserved_bytes"),
        },
        "resources": {
            "before": before_resources,
            "after": after_resources,
            "returned_near_baseline": resource_release_ok(before_resources, after_resources),
        },
        "release_conditions_within_timeout": release_conditions,
        "disconnect_to_release_ms": release_ms,
        "errors": errors,
    }


def _summarize_scenario(results: list[dict]) -> dict:
    samples = [item["disconnect_to_release_ms"] for item in results
               if item.get("disconnect_to_release_ms") is not None]
    return {
        "iterations": len(results),
        "release_latency": percentile_report(samples),
        "upstream_requests": sum(item.get("upstream", {}).get("requests", 0) for item in results),
        "upstream_retries": sum(item.get("upstream", {}).get("rejected_requests", 0) for item in results),
        "peer_close_count": sum(bool(item.get("upstream", {}).get("peer_closed")) for item in results),
        "request_reservation_zero_count": sum(
            item.get("request_limits", {}).get("reserved_after") == 0 for item in results
        ),
        "resources_released_count": sum(
            bool(item.get("resources", {}).get("returned_near_baseline")) for item in results
        ),
        "release_condition_counts": {
            key: sum(bool(item.get("release_conditions_within_timeout", {}).get(key))
                     for item in results)
            for key in (
                "upstream_socket_closed", "request_reservation_returned",
                "process_resources_returned",
            )
        },
        "errors": [error for item in results for error in item.get("errors", [])],
        "outcomes": [item.get("observation") for item in results],
    }


def _semantic_preflight(service, upstream: CancellationFakeUpstream, marker: str) -> dict:
    plan = upstream.begin("complete", marker)
    payload = json.dumps({
        "model": "benchmark/model", "input": marker, "stream": True,
    }, separators=(",", ":")).encode()
    try:
        result = benchmark._request(
            service.port, "POST", "/v1/responses", payload=payload,
            operation="cancellation_semantic_preflight", timeout=10.0,
        )
    except (benchmark.BenchmarkError, OSError, http.client.HTTPException):
        return {"valid": False, "status": None, "event_types": [], "completed": False,
                "output_text_present": False, "upstream_requests": plan.snapshot()["requests"]}
    expected_types = [kind for kind, _wire in _sse_events()]
    observation = {
        "valid": (
            result["status"] == 200
            and result["event_types"] == expected_types
            and result["terminal"] == "response.completed"
            and result["text"] == REPLY_TEXT
            and plan.snapshot()["requests"] == 1
            and plan.snapshot()["rejected_requests"] == 0
            and not plan.snapshot()["unexpected_request_shape"]
        ),
        "status": result["status"],
        "event_types": result["event_types"],
        "completed": result["terminal"] == "response.completed",
        "output_text_present": result["text"] == REPLY_TEXT,
        "upstream_requests": plan.snapshot()["requests"],
    }
    return observation


def _run_runtime(name: str, command: list[str], cwd: Path, config: bytes,
                 work_root: Path, key_file: Path, base_env: dict,
                 python_root: Path, upstream: CancellationFakeUpstream,
                 iterations: int, warmup: int, release_timeout: float) -> dict:
    config_path = work_root / f"{name}-cancellation.json"
    config_path.write_bytes(config)
    home = work_root / f"{name}-cancellation-home"
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    service = benchmark.EmpService(command, cwd, config_path, home, env, name)
    existing_plans = set(upstream.plans)
    counts_before = upstream.snapshot_counts()
    errors = []
    reference_errors = []

    def record_errors(prefix: str, values: list[str]):
        fatal, reference_only = classify_runtime_errors(name, prefix, values)
        errors.extend(fatal)
        reference_errors.extend(reference_only)

    preflight = None
    measured: dict[str, dict] = {}
    with service:
        health = benchmark._request(
            service.port, "GET", "/healthz", operation=name + "_healthz", timeout=5.0,
        )
        if health["status"] != 200:
            errors.append("health_status")
        initial_limits = _read_limit_snapshot(service)
        if initial_limits["reserved_bytes"] != 0:
            errors.append("initial_request_reservation_nonzero")
        preflight = _semantic_preflight(service, upstream, f"preflight-{name}")
        if not preflight["valid"]:
            errors.append("responses_semantic_preflight")

        for scenario in SCENARIOS:
            for index in range(warmup):
                result = _run_one(
                    service, upstream, scenario, release_timeout,
                    f"warmup-{name}-{scenario}-{index}",
                )
                record_errors("warmup_" + scenario + "_", result.get("errors", []))
        time.sleep(0.10)

        for scenario in SCENARIOS:
            results = []
            for index in range(iterations):
                result = _run_one(
                    service, upstream, scenario, release_timeout,
                    f"measure-{name}-{scenario}-{index}",
                )
                record_errors(name + "_" + scenario + "_", result.get("errors", []))
                results.append(result)
            measured[scenario] = _summarize_scenario(results)
            if name == "rust":
                release_p99 = measured[scenario]["release_latency"]["p99_ms"]
                if release_p99 is None or release_p99 > RUST_RELEASE_P99_LIMIT_MS:
                    errors.append(name + "_" + scenario + "_release_latency_p99_gate")

        final_limits = _read_limit_snapshot(service)
        if final_limits["reserved_bytes"] != 0:
            errors.append("final_request_reservation_nonzero")
    counts_after = upstream.snapshot_counts()
    phase_request_counts = aggregate_plan_requests(upstream.plans, existing_plans)
    extra_attempts = sum(
        phase["extra_attempts"] for phase in phase_request_counts.values()
    )
    if extra_attempts:
        if name == "python":
            reference_errors.append(f"python_late_upstream_retries_{extra_attempts}")
        else:
            errors.append(f"{name}_late_upstream_retries_{extra_attempts}")
    expected_plan_counts = {
        "preflight": 1,
        "warmup": warmup * len(SCENARIOS),
        "measurement": iterations * len(SCENARIOS),
    }
    for phase, expected in expected_plan_counts.items():
        if phase_request_counts[phase]["plan_count"] != expected:
            errors.append(f"{name}_{phase}_request_plan_count_mismatch")
    return {
        "preflight": preflight,
        "scenarios": measured,
        "errors": errors,
        "reference_errors": reference_errors,
        "upstream": {
            "unplanned_requests": (
                counts_after["unplanned_requests"] - counts_before["unplanned_requests"]
            ),
            "requests": sum(
                plan.snapshot()["requests"]
                for marker, plan in upstream.plans.items()
                if marker not in existing_plans
            ),
            "errors": counts_after["errors"] - counts_before["errors"],
            "phase_request_counts": phase_request_counts,
        },
    }


def run(args) -> tuple[dict, int]:
    if benchmark.psutil is None:
        raise benchmark.BenchmarkError("psutil_required")
    validate_parameters(args.iterations, args.warmup, args.release_timeout)
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
            "scenarios": list(SCENARIOS), "iterations": args.iterations,
            "warmup": args.warmup, "release_timeout_seconds": args.release_timeout,
            "fake_upstream_idle_timeout_seconds": FAKE_UPSTREAM_IDLE_TIMEOUT_SECONDS,
            "rust_release_p99_limit_ms": RUST_RELEASE_P99_LIMIT_MS,
            "resource_slack": {
                "rss_bytes": RSS_SLACK_BYTES,
                "threads": THREAD_SLACK,
                "descriptors": DESCRIPTOR_SLACK,
            },
            "retry_quiet_seconds": RETRY_QUIET_SECONDS,
            "measurement_order": args.order,
        },
        "preflight": {}, "measurements": None, "errors": [],
    }
    with tempfile.TemporaryDirectory(prefix="emp-cancellation-benchmark-") as directory:
        work_root = Path(directory)
        key_file = work_root / "master.key"
        key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
        os.chmod(key_file, 0o600)
        base_env = dict(os.environ)
        for key in benchmark.EXTERNAL_CREDENTIAL_ENV:
            base_env.pop(key, None)
        base_env["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(key_file)
        upstream = CancellationFakeUpstream()
        try:
            config = benchmark._normalized_config(
                python, python_root, python_env, upstream.base_url, work_root,
            )
            runtimes = [
                ("python", [str(python), "-m", "easy_multi_provider"], python_root),
                ("rust", [str(rust_binary)], Path.cwd()),
            ]
            if args.order == "rust-first":
                runtimes.reverse()
            measurements = {}
            for name, command, cwd in runtimes:
                result = _run_runtime(
                    name, command, cwd, config, work_root, key_file, base_env,
                    python_root, upstream, args.iterations, args.warmup,
                    args.release_timeout,
                )
                measurements[name] = result
                report["preflight"][name] = result["preflight"]
                report["errors"].extend(result["errors"])
                if result["upstream"]["unplanned_requests"]:
                    report["errors"].append(name + "_unplanned_upstream_request")
            py_contract = {
                "preflight": measurements["python"]["preflight"],
                "scenarios": {
                    scenario: measurements["python"]["scenarios"][scenario]["outcomes"]
                    for scenario in SCENARIOS
                },
            }
            rust_contract = {
                "preflight": measurements["rust"]["preflight"],
                "scenarios": {
                    scenario: measurements["rust"]["scenarios"][scenario]["outcomes"]
                    for scenario in SCENARIOS
                },
            }
            differences = benchmark.semantic_diff(py_contract, rust_contract)
            if differences:
                report["semantic_difference_paths"] = differences
                report["errors"].append("runtime_cancellation_semantics_mismatch")
            if report["errors"]:
                report["status"] = "invalid_measurement"
                report["measurements"] = measurements
                return report, 2
            report["status"] = "measured"
            report["comparison_enabled"] = True
            report["measurements"] = measurements
            return report, 0
        finally:
            upstream.close()


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=benchmark.DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--expected-version", default="0.12.1")
    parser.add_argument("--iterations", type=int, default=5)
    parser.add_argument("--warmup", type=int, default=1)
    parser.add_argument("--release-timeout", type=float, default=RELEASE_TIMEOUT_SECONDS)
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = _parse_args(argv)
    try:
        report, exit_code = run(args)
    except Exception as exc:
        report = {
            "schema": 1, "status": "incomparable", "comparison_enabled": False,
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
