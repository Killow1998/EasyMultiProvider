#!/usr/bin/env python3
"""Compare isolated Python/Rust startup, shutdown and short forward churn.

This runner launches real EMP processes. It is deliberately separate from the
request benchmark: each cycle starts the process, verifies the public ready
path and caller-visible forwarding result, then exercises one shutdown path.
"""

from __future__ import annotations

import argparse
import base64
from collections import defaultdict
import importlib.util
import json
import os
from pathlib import Path
import signal
import subprocess
import tempfile
import threading
import time
from urllib.parse import urlsplit
import queue


ROOT = Path(__file__).resolve().parents[1]
RUNTIME_SCRIPT = ROOT / "tools" / "benchmark_emp_runtime.py"
RUNTIME_SPEC = importlib.util.spec_from_file_location(
    "_emp_benchmark_runtime_for_startup_churn", RUNTIME_SCRIPT
)
if RUNTIME_SPEC is None or RUNTIME_SPEC.loader is None:  # pragma: no cover
    raise RuntimeError("runtime_benchmark_helpers_unavailable")
RUNTIME = importlib.util.module_from_spec(RUNTIME_SPEC)
RUNTIME_SPEC.loader.exec_module(RUNTIME)


class ChurnError(RuntimeError):
    """A safe benchmark failure code without process output or payloads."""

    def __init__(self, code: str):
        safe = code if code and all(char.isalnum() or char == "_" for char in code) else "churn_error"
        self.code = safe
        super().__init__(safe)


TRANSITIONS = {
    "new": {"spawned", "failed"},
    "spawned": {"listening", "failed"},
    "listening": {"ready", "failed"},
    "ready": {"forwarded", "shutdown_requested", "failed"},
    "forwarded": {"shutdown_requested", "failed"},
    "shutdown_requested": {"exited", "failed"},
    "exited": set(),
    "failed": set(),
}
RESOURCE_SLOPE_ALLOWANCES = {
    "rss_bytes": 4 * 1024 * 1024,
    "threads": 0.1,
    "descriptors": 0.1,
    "artifact_bytes": 1024 * 1024,
}


class CycleState:
    def __init__(self):
        self.value = "new"
        self.events = ["new"]

    def transition(self, state: str) -> None:
        if state not in TRANSITIONS.get(self.value, set()):
            raise ChurnError("invalid_cycle_transition")
        self.value = state
        self.events.append(state)


def percentile(values: list[float], quantile: float) -> float | None:
    return RUNTIME.percentile(values, quantile)


def strict_json_equal(left, right) -> bool:
    """Compare JSON values with object order ignored and array order retained."""
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return (left.keys() == right.keys() and
                all(strict_json_equal(left[key], right[key]) for key in left))
    if isinstance(left, list):
        return len(left) == len(right) and all(
            strict_json_equal(a, b) for a, b in zip(left, right)
        )
    return left == right


def linear_slope(values: list[float | int | None]) -> float | None:
    """Least-squares slope per cycle, ignoring no observations silently."""
    if len(values) < 2 or any(value is None for value in values):
        return None
    ys = [float(value) for value in values]
    xs = list(range(len(ys)))
    mean_x = sum(xs) / len(xs)
    mean_y = sum(ys) / len(ys)
    denominator = sum((x - mean_x) ** 2 for x in xs)
    if denominator == 0:
        return None
    return sum((x - mean_x) * (y - mean_y) for x, y in zip(xs, ys)) / denominator


def post_warmup_slopes(cycles: list[dict], warmup_cycles: int) -> dict:
    measured = cycles[warmup_cycles:]
    fields = ("rss_bytes", "threads", "descriptors", "artifact_bytes")
    return {
        field: linear_slope([cycle.get("resources", {}).get(field) if field != "artifact_bytes"
                             else cycle.get(field) for cycle in measured])
        for field in fields
    }


def readiness_summary(cycles: list[dict]) -> dict:
    values = [cycle["spawn_to_ready_ms"] for cycle in cycles
              if isinstance(cycle.get("spawn_to_ready_ms"), (int, float))]
    return {"count": len(values), "p50_ms": percentile(values, 0.50),
            "p95_ms": percentile(values, 0.95), "p99_ms": percentile(values, 0.99)}


def readiness_gate(python_cycles: list[dict], rust_cycles: list[dict], allowance_ms: float = 100.0) -> bool:
    python_p95 = readiness_summary(python_cycles)["p95_ms"]
    rust_p95 = readiness_summary(rust_cycles)["p95_ms"]
    return python_p95 is not None and rust_p95 is not None and rust_p95 <= python_p95 + allowance_ms


def validate_parameters(cycles: int, duration_seconds: float, warmup_cycles: int,
                        startup_timeout: float, shutdown_timeout: float) -> None:
    if cycles < 1:
        raise ChurnError("cycles_must_be_positive")
    if duration_seconds < 0 or (duration_seconds > 0 and duration_seconds < 1):
        raise ChurnError("invalid_duration_seconds")
    if warmup_cycles < 0 or warmup_cycles >= cycles:
        raise ChurnError("warmup_cycles_must_be_less_than_cycles")
    if not 1 <= startup_timeout <= 120:
        raise ChurnError("startup_timeout_out_of_range")
    if not 1 <= shutdown_timeout <= 60:
        raise ChurnError("shutdown_timeout_out_of_range")


def _startup_path(runtime_kind: str, announced_url: str) -> tuple[int, str]:
    parsed = urlsplit(announced_url)
    if parsed.port is None:
        raise ChurnError("startup_url_missing_port")
    return parsed.port, RUNTIME._bootstrap_target(runtime_kind, announced_url)


def _resource_identities(pid: int) -> list[tuple[int, float]]:
    try:
        root = RUNTIME.psutil.Process(pid)
        processes = [root, *root.children(recursive=True)]
        return [(proc.pid, proc.create_time()) for proc in processes]
    except RUNTIME.psutil.Error:
        return []


def process_tree_residue(identities: list[tuple[int, float]]) -> list[int]:
    """Find surviving original PIDs while ignoring PID reuse."""
    remaining = []
    for pid, created in identities:
        try:
            process = RUNTIME.psutil.Process(pid)
            if abs(process.create_time() - created) < 0.01 and process.is_running():
                remaining.append(pid)
        except RUNTIME.psutil.Error:
            continue
    return remaining


def wait_for_process_tree_exit(identities: list[tuple[int, float]],
                               timeout_seconds: float = 2.0) -> list[int]:
    deadline = time.monotonic() + timeout_seconds
    remaining = process_tree_residue(identities)
    while remaining and time.monotonic() < deadline:
        time.sleep(0.05)
        remaining = process_tree_residue(identities)
    return remaining


def shutdown_summary(cycles: list[dict]) -> dict:
    elapsed = [cycle["shutdown_elapsed_ms"] for cycle in cycles
               if isinstance(cycle.get("shutdown_elapsed_ms"), (int, float))]
    return {
        "count": len(elapsed),
        "p50_ms": percentile(elapsed, 0.50),
        "p95_ms": percentile(elapsed, 0.95),
        "p99_ms": percentile(elapsed, 0.99),
        "all_exit_zero": bool(cycles) and all(cycle.get("exit_code") == 0 for cycle in cycles),
        "no_process_residue": bool(cycles) and all(
            not cycle.get("process_tree_residue") for cycle in cycles
        ),
    }


def resource_slope_gate(python_cycles: list[dict], rust_cycles: list[dict],
                        warmup_cycles: int) -> dict:
    python_slopes = post_warmup_slopes(python_cycles, warmup_cycles)
    rust_slopes = post_warmup_slopes(rust_cycles, warmup_cycles)
    result = {}
    for field, allowance in RESOURCE_SLOPE_ALLOWANCES.items():
        python_slope = python_slopes[field]
        rust_slope = rust_slopes[field]
        result[field] = {
            "python_slope_per_cycle": python_slope,
            "rust_slope_per_cycle": rust_slope,
            "absolute_allowance_per_cycle": allowance,
            "reference_allowance_per_cycle": allowance,
            "absolute_pass": rust_slope is not None and rust_slope <= allowance,
            "comparative_pass": (
                python_slope is not None and rust_slope is not None and
                rust_slope <= max(python_slope, 0.0) + allowance
            ),
            "passed": (
                python_slope is not None and rust_slope is not None and
                rust_slope <= allowance and
                rust_slope <= max(python_slope, 0.0) + allowance
            ),
        }
    return result


def artifact_bytes(root: Path) -> int:
    total = 0
    for path in root.rglob("*"):
        try:
            if path.is_file() and not path.is_symlink():
                total += path.stat().st_size
        except OSError:
            continue
    return total


def _make_auth(home: Path) -> None:
    home.mkdir(parents=True, exist_ok=True)
    auth_path = home / "auth.json"
    auth_path.write_text(json.dumps({"tokens": {
        "access_token": RUNTIME.FIXTURE_CALLER_KEY,
        "account_id": "startup-churn-fixture-account",
    }}), encoding="utf-8")
    os.chmod(auth_path, 0o600)


def _wait_for_announcement(runtime_kind: str, process: subprocess.Popen,
                           lines: queue.Queue, timeout_seconds: float) -> tuple[float, int, str]:
    deadline = time.monotonic() + timeout_seconds
    while time.monotonic() < deadline:
        try:
            line = lines.get(timeout=min(0.25, deadline - time.monotonic()))
        except queue.Empty:
            if process.poll() is not None:
                raise ChurnError("process_exited_before_listener")
            continue
        if line is None:
            raise ChurnError("process_output_closed_before_listener")
        if line.startswith("Open in browser: "):
            announced = time.monotonic()
            port, path = _startup_path(runtime_kind, line.split(": ", 1)[1].strip())
            return announced, port, path
    raise ChurnError("startup_timeout")


def _cycle(runtime_kind: str, command: list[str], cwd: Path, config_path: Path,
           home: Path, env: dict, upstream, *, cycle_index: int,
           shutdown_mode: str, startup_timeout: float,
           shutdown_timeout: float) -> dict:
    state = CycleState()
    process = None
    reader = None
    lines: queue.Queue = queue.Queue()
    record = {"runtime": runtime_kind, "cycle": cycle_index,
              "shutdown_mode": shutdown_mode, "state_events": state.events}
    identities: list[tuple[int, float]] = []
    try:
        _make_auth(home)
        spawned_at = time.monotonic()
        process = subprocess.Popen(
            command + ["serve", "--config", str(config_path), "--host", "127.0.0.1", "--port", "0"],
            cwd=cwd, env=env, stdout=subprocess.PIPE, stderr=subprocess.STDOUT,
            text=True, encoding="utf-8", errors="replace", bufsize=1,
            start_new_session=(os.name == "posix"),
            creationflags=(getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
                           if os.name == "nt" else 0),
        )
        state.transition("spawned")

        def read_lines():
            assert process.stdout is not None
            for line in process.stdout:
                lines.put(line)
            lines.put(None)

        reader = threading.Thread(target=read_lines, daemon=True)
        reader.start()
        announced_at, port, bootstrap_path = _wait_for_announcement(
            runtime_kind, process, lines, startup_timeout
        )
        state.transition("listening")
        record["spawn_to_listening_ms"] = (announced_at - spawned_at) * 1000

        bootstrap = RUNTIME._request(port, "GET", bootstrap_path,
                                     operation=runtime_kind + "_bootstrap")
        cookie = bootstrap["headers"].get("set-cookie", "").split(";", 1)[0]
        expected_bootstrap = 200 if runtime_kind == "python" else 303
        if bootstrap["status"] != expected_bootstrap or not cookie:
            raise ChurnError("authenticated_bootstrap_failed")
        health = RUNTIME._request(port, "GET", "/healthz", operation="healthz")
        if health["status"] != 200:
            raise ChurnError("health_not_ready")
        config_result = RUNTIME._request(port, "GET", "/api/config", cookie=cookie,
                                         operation="authenticated_config")
        if config_result["status"] != 200:
            raise ChurnError("authenticated_bootstrap_failed")
        ready_at = time.monotonic()
        state.transition("ready")
        record["spawn_to_ready_ms"] = (ready_at - spawned_at) * 1000

        body = json.dumps({"model": "benchmark/model", "input": "startup-churn",
                           "stream": False}, separators=(",", ":")).encode()
        forward = RUNTIME._request(port, "POST", "/v1/responses", payload=body,
                                  operation="churn_forward")
        if not RUNTIME._valid_workload_result("responses_nonstream", forward):
            raise ChurnError("forwarding_failed")
        semantic = RUNTIME._responses_semantic(forward, stream=False)
        expected = {"status": 200, "object": "response", "status_value": "completed",
                    "model": RUNTIME.UPSTREAM_MODEL, "output_types": ["message"],
                    "text": RUNTIME.REPLY_TEXT, "total_tokens": 10}
        if not strict_json_equal(semantic, expected):
            raise ChurnError("forwarding_semantics_mismatch")
        record["forward_semantics"] = semantic
        record["forward_elapsed_ms"] = forward["elapsed_ms"]
        state.transition("forwarded")
        identities = _resource_identities(process.pid)
        record["resources"] = RUNTIME._tree_metrics(process.pid)

        shutdown_started = time.monotonic()
        state.transition("shutdown_requested")
        if shutdown_mode == "api_quit":
            quit_body = json.dumps({}, separators=(",", ":")).encode()
            quit_result = RUNTIME._request(port, "POST", "/api/quit", cookie=cookie,
                                           payload=quit_body, operation="quit")
            if quit_result["status"] != 200 or json.loads(quit_result["body"]) != {"status": "stopping"}:
                raise ChurnError("quit_contract_failed")
        elif shutdown_mode == "sigterm":
            if os.name != "posix":
                raise ChurnError("sigterm_shutdown_requires_posix")
            process.send_signal(signal.SIGTERM)
        else:
            raise ChurnError("unknown_shutdown_mode")
        exit_code = process.wait(timeout=shutdown_timeout)
        shutdown_elapsed = (time.monotonic() - shutdown_started) * 1000
        if exit_code != 0:
            raise ChurnError("shutdown_exit_not_zero")
        remaining = wait_for_process_tree_exit(identities)
        if remaining:
            raise ChurnError("process_tree_residue")
        state.transition("exited")
        record.update({"state": state.value, "exit_code": exit_code,
                       "shutdown_elapsed_ms": shutdown_elapsed,
                       "process_tree_residue": remaining})
        return record
    except subprocess.TimeoutExpired:
        state.transition("failed")
        raise ChurnError("shutdown_timeout") from None
    except ChurnError:
        if state.value not in {"failed", "exited"}:
            state.transition("failed")
        raise
    except (OSError, ValueError, KeyError, TypeError, json.JSONDecodeError) as exc:
        if state.value not in {"failed", "exited"}:
            state.transition("failed")
        raise ChurnError("cycle_" + type(exc).__name__) from None
    finally:
        if process is not None and process.poll() is None:
            try:
                if os.name == "posix":
                    os.killpg(process.pid, signal.SIGKILL)
                else:
                    process.kill()
                process.wait(timeout=5)
            except (OSError, subprocess.TimeoutExpired):
                pass
        if reader is not None:
            reader.join(timeout=2)


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=RUNTIME.DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--expected-version", default="0.12.1")
    parser.add_argument("--cycles", type=int, default=5)
    parser.add_argument("--duration-seconds", type=float, default=0)
    parser.add_argument("--warmup-cycles", type=int, default=1)
    parser.add_argument("--startup-timeout", type=float, default=30)
    parser.add_argument("--shutdown-timeout", type=float, default=8)
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    return parser.parse_args(argv)


def main(argv=None) -> int:
    args = _parse_args(argv)
    try:
        validate_parameters(args.cycles, args.duration_seconds, args.warmup_cycles,
                            args.startup_timeout, args.shutdown_timeout)
        if RUNTIME.psutil is None:
            raise ChurnError("psutil_required")
        python_root = args.python_root.resolve(strict=True)
        python = RUNTIME._python_launcher(args.python, python_root)
        rust_binary = args.rust_binary.resolve(strict=True)
        python_env = dict(os.environ)
        python_env["PYTHONPATH"] = str(python_root)
        python_version = RUNTIME._python_version(python, python_root, python_env)
        if python_version != args.expected_version:
            raise ChurnError("python_version_mismatch")
        rust_version = subprocess.run([str(rust_binary), "--version"], check=True,
                                      capture_output=True, text=True, timeout=10).stdout.strip()
        RUNTIME._clean_version(args.expected_version, rust_version)
        env = dict(os.environ)
        for key in RUNTIME.EXTERNAL_CREDENTIAL_ENV:
            env.pop(key, None)
        report = {"schema": 1, "status": "incomparable", "comparison_enabled": False,
                  "versions": {"python_emp": python_version, "rust_emp": rust_version},
                  "environment": RUNTIME._environment(),
                  "parameters": {"cycles": args.cycles,
                                 "duration_seconds": args.duration_seconds,
                                 "warmup_cycles": args.warmup_cycles,
                                 "shutdown_paths": (["api_quit", "sigterm"]
                                                    if os.name == "posix" else ["api_quit"])},
                  "runtimes": {}, "errors": []}
        with tempfile.TemporaryDirectory(prefix="emp-startup-churn-") as temporary:
            work_root = Path(temporary)
            key_file = work_root / "master.key"
            key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
            os.chmod(key_file, 0o600)
            fake = RUNTIME.FakeResponsesUpstream()
            commands = {
                "python": ([str(python), "-m", "easy_multi_provider"], python_root),
                "rust": ([str(rust_binary)], ROOT),
            }
            try:
                backends = ["python", "rust"]
                if args.order == "rust-first":
                    backends.reverse()
                cycles = defaultdict(list)
                signal_cycles = defaultdict(list)
                started = time.monotonic()
                completed_pairs = 0
                while (completed_pairs < args.cycles if args.duration_seconds == 0
                       else time.monotonic() - started < args.duration_seconds):
                    for runtime_kind in backends:
                        runtime_dir = work_root / runtime_kind
                        runtime_dir.mkdir(parents=True, exist_ok=True)
                        config_path = runtime_dir / "emp.json"
                        home = runtime_dir / "codex-home"
                        service_env = RUNTIME._service_environment(
                            env, home, key_file, python_root
                        )
                        config = RUNTIME._normalized_config(
                            python, python_root, python_env, fake.base_url, runtime_dir
                        )
                        config_path.write_bytes(config)
                        index = len(cycles[runtime_kind])
                        record = _cycle(
                            runtime_kind, *commands[runtime_kind], config_path, home,
                            service_env, fake, cycle_index=index,
                            shutdown_mode="api_quit", startup_timeout=args.startup_timeout,
                            shutdown_timeout=args.shutdown_timeout,
                        )
                        record["artifact_bytes"] = artifact_bytes(runtime_dir)
                        cycles[runtime_kind].append(record)
                        if os.name == "posix":
                            config_path.write_bytes(RUNTIME._normalized_config(
                                python, python_root, python_env, fake.base_url, runtime_dir
                            ))
                            signal_record = _cycle(
                                runtime_kind, *commands[runtime_kind], config_path, home,
                                service_env, fake, cycle_index=len(signal_cycles[runtime_kind]),
                                shutdown_mode="sigterm", startup_timeout=args.startup_timeout,
                                shutdown_timeout=args.shutdown_timeout,
                            )
                            signal_record["artifact_bytes"] = artifact_bytes(runtime_dir)
                            signal_cycles[runtime_kind].append(signal_record)
                    completed_pairs += 1
                report["actual_duration_seconds"] = time.monotonic() - started
                python_startup = cycles["python"] + signal_cycles["python"]
                rust_startup = cycles["rust"] + signal_cycles["rust"]
                python_semantics = {
                    "api_quit": [cycle["forward_semantics"] for cycle in cycles["python"]],
                    "sigterm": [cycle["forward_semantics"] for cycle in signal_cycles["python"]],
                }
                rust_semantics = {
                    "api_quit": [cycle["forward_semantics"] for cycle in cycles["rust"]],
                    "sigterm": [cycle["forward_semantics"] for cycle in signal_cycles["rust"]],
                }
                semantic_equal = strict_json_equal(python_semantics, rust_semantics)
                gate = readiness_gate(python_startup, rust_startup)
                report["comparison_enabled"] = semantic_equal
                report["status"] = "measured" if semantic_equal else "invalid_measurement"
                if not semantic_equal:
                    report["errors"].append("forward_semantic_mismatch")
                if not gate:
                    report["errors"].append("readiness_gate_failed")
                    report["status"] = "invalid_measurement"
                report["readiness_gate"] = {
                    "allowance_ms": 100,
                    "python": readiness_summary(python_startup),
                    "rust": readiness_summary(rust_startup),
                    "passed": gate,
                }
                shutdown_summaries = {
                    mode: {name: shutdown_summary(items)
                           for name, items in table.items()}
                    for mode, table in (("api_quit", cycles), ("sigterm", signal_cycles))
                    if os.name == "posix" or mode == "api_quit"
                }
                report["shutdown_summaries"] = shutdown_summaries
                resource_gates = resource_slope_gate(
                    cycles["python"], cycles["rust"], args.warmup_cycles
                )
                report["post_warmup_resource_slope_gates"] = resource_gates
                if not all(item["passed"] for item in resource_gates.values()):
                    report["errors"].append("post_warmup_resource_slope_gate_failed")
                    report["status"] = "invalid_measurement"
                report["runtimes"] = {
                    name: {"cycles": items,
                           "post_warmup_resource_slopes_per_cycle": post_warmup_slopes(
                               items, args.warmup_cycles
                           ),
                           "signal_shutdown_cycles": signal_cycles[name]}
                    for name, items in cycles.items()
                }
                if os.name != "posix":
                    report["platform_signal_shutdown"] = {
                        "status": "not_comparable",
                        "reason": "python_console_close_and_rust_ctrl_break_are_distinct_events",
                    }
                report["forwarding_upstream"] = {
                    "requests": fake.request_count, "errors": fake.error_count,
                }
                expected_upstream_requests = completed_pairs * (4 if os.name == "posix" else 2)
                if fake.request_count != expected_upstream_requests:
                    report["errors"].append("forwarding_upstream_count_mismatch")
                    report["status"] = "invalid_measurement"
                if fake.error_count:
                    report["errors"].append("fake_upstream_rejected_forwarding")
                    report["status"] = "invalid_measurement"
            finally:
                fake.close()
        payload = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if str(args.output) == "-":
            print(payload, end="")
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(payload, encoding="utf-8")
        return 0 if report["status"] == "measured" and not report["errors"] else 2
    except (ChurnError, RUNTIME.BenchmarkError) as exc:
        report = {"schema": 1, "status": "blocked", "comparison_enabled": False,
                  "errors": [getattr(exc, "code", "benchmark_error")]}
        payload = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if str(args.output) == "-":
            print(payload, end="")
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(payload, encoding="utf-8")
        return 2
    except Exception as exc:
        safe_type = type(exc).__name__
        report = {"schema": 1, "status": "blocked", "comparison_enabled": False,
                  "errors": ["unexpected_" + safe_type]}
        payload = json.dumps(report, indent=2, sort_keys=True) + "\n"
        if str(args.output) == "-":
            print(payload, end="")
        else:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(payload, encoding="utf-8")
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
