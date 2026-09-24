#!/usr/bin/env python3
"""Measure management latency with the production idle-WebSocket limit occupied.

The harness compares the locked Python service and a supplied Rust EMP binary.
It opens real loopback WebSockets, keeps them idle while requesting the
authenticated management config endpoint, and refuses to report a measurement
unless both runtimes pass the same admission, response, and release checks.
"""

from __future__ import annotations

import argparse
from concurrent.futures import ThreadPoolExecutor, as_completed
import base64
import hashlib
import json
import os
from pathlib import Path
import platform
import select
import socket
import subprocess
import sys
import tempfile
import time

try:
    from . import benchmark_emp_runtime as benchmark
except ImportError:  # Running as `python tools/benchmark_emp_idle_websockets.py`.
    tools_dir = str(Path(__file__).resolve().parent)
    if tools_dir not in sys.path:
        sys.path.insert(0, tools_dir)
    import benchmark_emp_runtime as benchmark


DEFAULT_CONNECTIONS = 224
DEFAULT_HANDSHAKE_CONCURRENCY = 64
DEFAULT_ITERATIONS = 250
DEFAULT_WARMUP = 20
MANAGEMENT_PATH = "/api/config"
RESOURCE_RELEASE_TIMEOUT_SECONDS = 5.0
RESOURCE_RELEASE_SLACK = 8
MAX_P95_MS = 100.0
WEBSOCKET_KEY = "dGhlIHNhbXBsZSBub25jZQ=="


def validate_parameters(connections: int, handshake_concurrency: int,
                        iterations: int, warmup: int) -> None:
    if not 1 <= connections <= DEFAULT_CONNECTIONS:
        raise benchmark.BenchmarkError("connections_must_be_1_to_224")
    if not 1 <= handshake_concurrency <= connections:
        raise benchmark.BenchmarkError("invalid_handshake_concurrency")
    if iterations < 1 or warmup < 0:
        raise benchmark.BenchmarkError("invalid_iteration_count")


def percentile_summary(samples_ms: list[float], elapsed_seconds: float,
                        errors: int) -> dict:
    summary = benchmark.latency_summary(samples_ms, elapsed_seconds, errors)
    summary["max_ms"] = max(samples_ms) if samples_ms else None
    return summary


def resource_release_ok(before: dict, after: dict) -> bool:
    """Check server-side descriptors and threads return near their baseline."""
    checked = False
    for key in ("descriptors", "threads"):
        initial, final = before.get(key), after.get(key)
        if initial is not None and final is not None:
            checked = True
            if final > initial + RESOURCE_RELEASE_SLACK:
                return False
    return checked


def _websocket_handshake(port: int, cookie: str, timeout: float = 10.0):
    """Return (socket, status, safe_error); retain only successful 101 sockets."""
    connection = None
    try:
        connection = socket.create_connection(("127.0.0.1", port), timeout=timeout)
        connection.settimeout(timeout)
        request = (
            f"GET /v1/responses HTTP/1.1\r\n"
            f"Host: 127.0.0.1:{port}\r\n"
            "Upgrade: websocket\r\n"
            "Connection: Upgrade\r\n"
            "Sec-WebSocket-Version: 13\r\n"
            f"Sec-WebSocket-Key: {WEBSOCKET_KEY}\r\n"
            f"Cookie: {cookie}\r\n\r\n"
        ).encode("ascii")
        connection.sendall(request)
        response_head = bytearray()
        while b"\r\n\r\n" not in response_head:
            chunk = connection.recv(1)
            if not chunk:
                connection.close()
                return None, None, "handshake_eof"
            response_head.extend(chunk)
            if len(response_head) > 16 * 1024:
                connection.close()
                return None, None, "handshake_header_too_large"
        status_line = bytes(response_head).split(b"\r\n", 1)[0]
        parts = status_line.split()
        try:
            status = int(parts[1])
        except (IndexError, ValueError):
            connection.close()
            return None, None, "invalid_handshake_status"
        if status != 101:
            connection.close()
            return None, status, "handshake_rejected"
        headers = {}
        for line in bytes(response_head).split(b"\r\n")[1:]:
            if not line or b":" not in line:
                continue
            name, value = line.split(b":", 1)
            headers[name.strip().lower()] = value.strip()
        expected_accept = base64.b64encode(hashlib.sha1(
            (WEBSOCKET_KEY + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
        ).digest())
        connection_tokens = [
            token.strip().lower()
            for token in headers.get(b"connection", b"").split(b",")
        ]
        if (headers.get(b"sec-websocket-accept") != expected_accept
                or headers.get(b"upgrade", b"").lower() != b"websocket"
                or b"upgrade" not in connection_tokens):
            connection.close()
            return None, status, "invalid_websocket_handshake"
        return connection, status, None
    except (OSError, TimeoutError):
        if connection is not None:
            connection.close()
        return None, None, "handshake_io_error"


def _idle_socket_failures(connections: list[socket.socket], grace_seconds: float = 0.1) -> int:
    """An idle response socket must not emit a frame or close itself."""
    if not connections:
        return 0
    try:
        readable, _, _ = select.select(connections, [], [], grace_seconds)
    except (OSError, ValueError):
        return len(connections)
    # Idle clients send no response.create, so any server frame or EOF indicates
    # an unexpected admission close or unsolicited payload.
    return len(readable)


def _open_websockets(service, count: int, concurrency: int) -> tuple[list[socket.socket], dict]:
    opened: list[socket.socket] = []
    statuses: list[int] = []
    failures: list[str] = []
    with ThreadPoolExecutor(max_workers=concurrency) as executor:
        futures = [
            executor.submit(_websocket_handshake, service.port, service.cookie)
            for _ in range(count)
        ]
        for future in as_completed(futures):
            connection, status, error = future.result()
            if connection is not None:
                opened.append(connection)
            if status is not None:
                statuses.append(status)
            if error is not None:
                failures.append(error)
    unexpected_idle_frames = _idle_socket_failures(opened)
    return opened, {
        "requested": count,
        "upgraded_101": sum(status == 101 for status in statuses),
        "rejected_statuses": sorted(status for status in statuses if status != 101),
        "io_failures": len(failures),
        "unexpected_idle_frames_or_closes": unexpected_idle_frames,
        "all_accepted_and_idle": (
            len(opened) == count and len(statuses) == count
            and all(status == 101 for status in statuses)
            and not failures and unexpected_idle_frames == 0
        ),
    }


def _close_websockets(connections: list[socket.socket]) -> None:
    for connection in connections:
        try:
            connection.shutdown(socket.SHUT_RDWR)
        except OSError:
            pass
        connection.close()


def _wait_resources_released(pid: int, before: dict) -> dict:
    deadline = time.monotonic() + RESOURCE_RELEASE_TIMEOUT_SECONDS
    latest = benchmark._tree_metrics(pid)
    while time.monotonic() < deadline and not resource_release_ok(before, latest):
        time.sleep(0.05)
        latest = benchmark._tree_metrics(pid)
    return latest


def _service_for(name: str, command: list[str], cwd: Path, config_bytes: bytes,
                 work_root: Path, key_file: Path, base_env: dict,
                 python_root: Path):
    config_path = work_root / f"{name}-idle-ws.json"
    config_path.write_bytes(config_bytes)
    home = work_root / f"{name}-idle-ws-home"
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    return benchmark.EmpService(command, cwd, config_path, home, env, name)


def _exercise_runtime(name: str, command: list[str], cwd: Path, config_bytes: bytes,
                      work_root: Path, key_file: Path, base_env: dict,
                      python_root: Path, connections: int,
                      handshake_concurrency: int, iterations: int, warmup: int,
                      expected_config_semantic: dict | None,
                      measure: bool) -> tuple[dict, list[str]]:
    service = _service_for(name, command, cwd, config_bytes, work_root,
                           key_file, base_env, python_root)
    errors: list[str] = []
    with service:
        health = benchmark._request(service.port, "GET", "/healthz", operation="healthz")
        if health["status"] != 200:
            errors.append("health_status")
        before = benchmark._tree_metrics(service.process.pid)
        sampler = benchmark.ResourceSampler(service.process.pid)
        sampler.start()
        sockets, admission = _open_websockets(service, connections, handshake_concurrency)
        while_open = benchmark._tree_metrics(service.process.pid)
        management_errors = 0
        semantic_mismatches = 0
        samples: list[float] = []
        total_requests = warmup + (iterations if measure else 1)
        management_semantic = None
        measurement_started = None
        for index in range(total_requests):
            if measure and index == warmup:
                measurement_started = time.perf_counter()
            result = benchmark._request(
                service.port, "GET", MANAGEMENT_PATH, cookie=service.cookie,
                operation="idle_ws_management", timeout=10.0,
            )
            semantic = benchmark._management_semantic(result, "config")
            if management_semantic is None:
                management_semantic = semantic
            if result["status"] != 200:
                management_errors += 1
            if expected_config_semantic is not None and semantic != expected_config_semantic:
                semantic_mismatches += 1
            if measure and index >= warmup:
                samples.append(result["elapsed_ms"])
        elapsed = (
            time.perf_counter() - measurement_started
            if measurement_started is not None else 0.0
        )
        if management_errors:
            errors.append("management_status")
        if semantic_mismatches:
            errors.append("management_semantic")
        final_idle_failures = _idle_socket_failures(sockets, grace_seconds=0.0)
        admission["unexpected_idle_frames_or_closes"] = max(
            admission["unexpected_idle_frames_or_closes"], final_idle_failures
        )
        admission["all_accepted_and_idle"] = (
            admission["all_accepted_and_idle"] and final_idle_failures == 0
        )
        if not admission["all_accepted_and_idle"]:
            errors.append("websocket_admission")

        _close_websockets(sockets)
        after = _wait_resources_released(service.process.pid, before)
        sampler.stop()
        released = resource_release_ok(before, after)
        if not released:
            errors.append("websocket_resources_not_released")
        result = {
            "connections": admission,
            "management": {
                "path": MANAGEMENT_PATH,
                "warmup_requests": warmup,
                "iterations": iterations if measure else 0,
                "successful_requests": total_requests - management_errors,
                "errors": management_errors,
                "semantic_mismatches": semantic_mismatches,
                "semantic": management_semantic,
                "elapsed_seconds": elapsed,
                "latency": percentile_summary(samples, elapsed, management_errors),
            },
            "resources": {
                "before": before,
                "with_websockets": while_open,
                "after_close": after,
                "overall_peak_rss_bytes": sampler.overall_peak(),
                "returned_near_baseline": released,
            },
        }
    return result, errors


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=benchmark.DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--expected-version", default="0.11.10")
    parser.add_argument("--connections", type=int, default=DEFAULT_CONNECTIONS)
    parser.add_argument("--handshake-concurrency", type=int, default=DEFAULT_HANDSHAKE_CONCURRENCY)
    parser.add_argument("--iterations", type=int, default=DEFAULT_ITERATIONS)
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    parser.add_argument("--max-p95-ms", type=float, default=MAX_P95_MS)
    return parser.parse_args(argv)


def run(args) -> tuple[dict, int]:
    if benchmark.psutil is None:
        raise benchmark.BenchmarkError("psutil_required")
    validate_parameters(args.connections, args.handshake_concurrency,
                        args.iterations, args.warmup)
    if args.max_p95_ms <= 0:
        raise benchmark.BenchmarkError("invalid_p95_limit")
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
            "connections": args.connections,
            "handshake_concurrency": args.handshake_concurrency,
            "management_path": MANAGEMENT_PATH,
            "iterations": args.iterations,
            "warmup": args.warmup,
            "p95_limit_ms": args.max_p95_ms,
            "measurement_order": args.order,
        },
        "preflight": {},
        "measurements": None,
        "errors": [],
    }

    with tempfile.TemporaryDirectory(prefix="emp-idle-ws-benchmark-") as directory:
        work_root = Path(directory)
        key_file = work_root / "master.key"
        key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
        os.chmod(key_file, 0o600)
        base_env = dict(os.environ)
        for key in benchmark.EXTERNAL_CREDENTIAL_ENV:
            base_env.pop(key, None)
        base_env["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(key_file)
        fake = benchmark.FakeResponsesUpstream()
        try:
            config_bytes = benchmark._normalized_config(
                python, python_root, python_env, fake.base_url, work_root
            )
            runtimes = [
                ("python", [str(python), "-m", "easy_multi_provider"], python_root),
                ("rust", [str(rust_binary)], Path.cwd()),
            ]
            for name, command, cwd in runtimes:
                fake.reset_counts()
                preflight, errors = _exercise_runtime(
                    name, command, cwd, config_bytes, work_root, key_file,
                    base_env, python_root, args.connections,
                    args.handshake_concurrency, 0, 0, None, False,
                )
                report["preflight"][name] = preflight
                if errors:
                    report["errors"].extend([name + "_preflight_" + error for error in errors])
                if fake.request_count or fake.error_count:
                    report["errors"].append(name + "_unexpected_upstream_traffic")
                report["preflight"][name]["upstream"] = {
                    "requests": fake.request_count,
                    "errors": fake.error_count,
                }

            py_contract = {
                "connections": report["preflight"]["python"]["connections"],
                "management": report["preflight"]["python"]["management"]["semantic"],
                "resources_returned": report["preflight"]["python"]["resources"]["returned_near_baseline"],
            }
            rust_contract = {
                "connections": report["preflight"]["rust"]["connections"],
                "management": report["preflight"]["rust"]["management"]["semantic"],
                "resources_returned": report["preflight"]["rust"]["resources"]["returned_near_baseline"],
            }
            differences = benchmark.semantic_diff(py_contract, rust_contract)
            if differences:
                report["semantic_difference_paths"] = differences
                report["errors"].append("runtime_preflight_mismatch")
            if report["errors"]:
                return report, 2

            expected_config = report["preflight"]["python"]["management"]["semantic"]
            report["comparison_enabled"] = True
            report["status"] = "measured"
            measured = {}
            measurement_runtimes = runtimes if args.order == "python-first" else list(reversed(runtimes))
            for name, command, cwd in measurement_runtimes:
                values, errors = _exercise_runtime(
                    name, command, cwd, config_bytes, work_root, key_file,
                    base_env, python_root, args.connections,
                    args.handshake_concurrency, args.iterations, args.warmup,
                    expected_config, True,
                )
                measured[name] = values
                report["errors"].extend([name + "_measurement_" + error for error in errors])
            report["measurements"] = measured
            for name in ("python", "rust"):
                p95 = measured[name]["management"]["latency"]["p95_ms"]
                if p95 is None or p95 > args.max_p95_ms:
                    report["errors"].append(name + "_management_p95_gate")
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
