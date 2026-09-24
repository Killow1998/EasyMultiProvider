#!/usr/bin/env python3
"""Compare real Python/Rust EMP usage-history scans and management responses.

This harness seeds an isolated SQLite ledger, asks each running EMP process to
scan the same synthetic Codex rollout through POST /api/usage/scan, and reads
the resulting data through the authenticated management API. It does not use
real Codex history, credentials, or user databases.
"""

from __future__ import annotations

import argparse
from datetime import datetime, timezone
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import shutil
import sqlite3
import sys
import tempfile
import time
from urllib.parse import urlencode


SCHEMA = 1
DEFAULT_ROLLOUT_RECORDS = 100_000
DEFAULT_LEDGER_ROWS = 120_000
ELAPSED_RATIO_LIMIT = 1.10
POLL_INTERVAL_SECONDS = 0.05
SCAN_TIMEOUT_SECONDS = 300.0
FORWARDING_LIMIT_MS = 1_000.0
MODEL_ID = "benchmark/model"
UPSTREAM_MODEL = "benchmark-upstream-model"
PROVIDER_ID = "benchmark"
PROVIDER_KEY = "usage-scan-fixture-key"
FORWARD_TEXT = "EMP_USAGE_SCAN_FORWARDING_OK"

RATES = {
    "input_cost_per_token": "0.000002",
    "output_cost_per_token": "0.000008",
    "cache_read_input_token_cost": "0.0000002",
}


class BenchmarkError(RuntimeError):
    pass


def _failure_detail(exc: Exception) -> str:
    if isinstance(exc, BenchmarkError):
        return str(exc)
    message = str(exc)
    if len(message) > 1_000:
        message = message[:200] + " … [truncated] … " + message[-800:]
    return f"{type(exc).__name__}: {message}"


def validate_parameters(rollout_records: int, ledger_rows: int) -> None:
    if not 1 <= rollout_records <= 1_000_000:
        raise BenchmarkError("rollout_records_must_be_1_to_1000000")
    if not 0 <= ledger_rows <= 1_000_000:
        raise BenchmarkError("ledger_rows_must_be_0_to_1000000")


def synthetic_ledger_row(index: int, now: int) -> tuple:
    categories = ("external", "native", "subscription", "unknown")
    owners = {
        "external": PROVIDER_ID,
        "native": "account:fixture-native-owner",
        "subscription": "account:fixture-subscription-owner",
        "unknown": "unconfirmed:fixture-owner",
    }
    category = categories[index % len(categories)]
    input_tokens = 1_000 + index % 997
    output_tokens = 80 + index % 71
    issue = "inconsistent_usage" if index % 997 == 0 else None
    # RATES are dollars per token: 0.000002/0.000008 USD = 2,000/8,000 nanos.
    cost = None if issue else input_tokens * 2_000 + output_tokens * 8_000
    return (
        f"seed-usage-{index:09d}",
        float(now - 86400 - (index % (7 * 86400))),
        category,
        owners[category],
        UPSTREAM_MODEL,
        "default",
        "responses",
        input_tokens,
        output_tokens,
        0,
        0,
        0,
        min(20, output_tokens),
        cost,
        issue,
        UPSTREAM_MODEL,
        float(now - 60),
        "fixture-price-revision",
        "{}",
        "realtime",
        "",
        "",
        "",
    )


def _fixture_module(python_root: Path):
    fixture_path = python_root / "tests" / "test_usage_history.py"
    if not fixture_path.is_file():
        raise BenchmarkError("official_usage_history_fixture_not_found")
    spec = importlib.util.spec_from_file_location("emp_official_usage_history_fixture", fixture_path)
    if spec is None or spec.loader is None:
        raise BenchmarkError("official_usage_history_fixture_unloadable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def _json_line(stream, value: dict) -> None:
    stream.write(json.dumps(value, separators=(",", ":"), ensure_ascii=False))
    stream.write("\n")


def write_rollout(path: Path, record_count: int, start_epoch: int, fixture) -> int:
    """Write records with the exact helpers used by the Python history tests."""
    path.parent.mkdir(parents=True, exist_ok=True)
    rows = fixture.header(MODEL_ID)
    with path.open("w", encoding="utf-8", newline="\n") as output:
        for row in rows:
            _json_line(output, row)
        for index in range(record_count):
            row = fixture.usage(
                count=100 + index,
                second=(index % 59) + 1,
                output=20 + (index % 7),
                reasoning=5 + (index % 5),
            )
            stamp = start_epoch + (index % 256) * 60
            row["timestamp"] = datetime.fromtimestamp(stamp, timezone.utc).isoformat().replace("+00:00", "Z")
            _json_line(output, row)
    return path.stat().st_size


def _seed_usage_database(path: Path, price_path: Path, row_count: int, now: int) -> None:
    """Ask the official Python ledger to create its schema, then batch seed rows."""
    from easy_multi_provider.diagnostic_journal import NullJournal
    from easy_multi_provider.usage_ledger import UsageLedger
    from easy_multi_provider.usage_pricing import PriceCatalog

    prices = PriceCatalog(price_path, NullJournal())
    ledger = UsageLedger(path, prices, NullJournal())
    connection = ledger._connect()
    sql = "INSERT INTO usage_events VALUES (" + ",".join("?" for _ in range(23)) + ")"
    rows = []
    for index in range(row_count):
        rows.append(synthetic_ledger_row(index, now))
        if len(rows) == 2_000:
            connection.executemany(sql, rows)
            rows.clear()
    if rows:
        connection.executemany(sql, rows)
    connection.commit()
    connection.close()


def seed_fixture(seed_root: Path, ledger_rows: int, now: int) -> tuple[Path, Path]:
    state = seed_root / "state"
    state.mkdir(parents=True, exist_ok=True)
    price_path = state / "api_prices.json"
    price_path.write_text(json.dumps({
        "fetched_at": now - 60,
        "prices": {UPSTREAM_MODEL: RATES},
    }, separators=(",", ":")), encoding="utf-8")
    usage_path = state / "usage.sqlite3"
    _seed_usage_database(usage_path, price_path, ledger_rows, now)
    return usage_path, price_path


def canonical_usage(payload: dict) -> dict:
    """Ignore only wall-clock scan timestamps; preserve response row order."""
    value = json.loads(json.dumps(payload))
    history = value.get("history")
    if isinstance(history, dict):
        history["last_scan_at"] = history.get("last_scan_at") is not None
    return value


def checkpoint_snapshot(database: Path, codex_home: Path, rollout: Path) -> dict:
    connection = sqlite3.connect(database, timeout=10)
    try:
        rows = connection.execute("SELECT path, checkpoint FROM usage_files ORDER BY path").fetchall()
        history_count = connection.execute(
            "SELECT COUNT(*) FROM usage_events WHERE origin='history'"
        ).fetchone()[0]
    finally:
        connection.close()
    checkpoints = canonical_checkpoints(rows, codex_home)
    return {
        "history_rows": int(history_count),
        "checkpoints": checkpoints,
        "expected_rollout_size": rollout.stat().st_size,
    }


def canonical_checkpoints(rows: list[tuple[str, str]], codex_home: Path) -> list[dict]:
    checkpoints = []
    for stored_path, raw in rows:
        value = json.loads(raw)
        identity = value.get("identity")
        if isinstance(identity, list) and len(identity) == 3:
            value["identity"] = ["device", "inode", identity[2]]
        checkpoints.append({
            "path": Path(stored_path).resolve().relative_to(codex_home.resolve()).as_posix(),
            "checkpoint": value,
        })
    return checkpoints


def canonical_scan_status(history: dict) -> dict:
    return {
        "running": bool(history.get("running")),
        "queued": bool(history.get("queued", False)),
        "files": history.get("files"),
        "updated": history.get("updated"),
        "errors": history.get("errors"),
        "last_scan_at": history.get("last_scan_at") is not None,
    }


def _request_json(backend, method: str, path: str, body=None):
    status, headers, raw = backend.request(method, path, body)
    try:
        payload = json.loads(raw) if raw else None
    except (UnicodeDecodeError, json.JSONDecodeError):
        payload = {"raw_sha256": hashlib.sha256(raw).hexdigest()}
    return status, headers, payload


def _usage_url(start: int, end: int, category: str = "all") -> str:
    return "/api/usage?" + urlencode({"start": str(start), "end": str(end), "category": category})


def _wait_baseline(backend, timeout: float = 30.0) -> dict:
    deadline = time.monotonic() + timeout
    previous = None
    while time.monotonic() < deadline:
        # This is a status poll, not an accounting query. Keep its SQL window
        # empty so a large ledger is not repeatedly aggregated during the scan.
        status, _, payload = _request_json(backend, "GET", _usage_url(1, 2))
        if status != 200:
            raise BenchmarkError("baseline_usage_query_failed_" + str(status))
        history = payload.get("history", {})
        if history.get("last_scan_at") is not None and not history.get("running", False):
            return payload
        previous = history
        time.sleep(POLL_INTERVAL_SECONDS)
    raise BenchmarkError("initial_history_scan_timeout_" + json.dumps(previous, sort_keys=True))


def _forwarding_smoke(backend, upstream) -> dict:
    upstream.configure({
        "id": "usage-scan-forwarding-smoke",
        "object": "response",
        "status": "completed",
        "model": UPSTREAM_MODEL,
        "output": [{"type": "message", "role": "assistant", "content": [
            {"type": "output_text", "text": FORWARD_TEXT}
        ]}],
        "usage": {"input_tokens": 9, "output_tokens": 4},
    })
    started = time.perf_counter()
    status, _, raw = backend.request("POST", "/v1/responses", {
        "model": MODEL_ID, "input": "small scan concurrency smoke", "stream": False,
    })
    elapsed_ms = (time.perf_counter() - started) * 1000
    request = upstream.requests.get(timeout=5)
    if not upstream.requests.empty():
        raise BenchmarkError("unexpected_extra_upstream_request")
    try:
        body = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        body = {}
    text = "".join(
        part.get("text", "")
        for item in body.get("output", []) if isinstance(item, dict)
        for part in item.get("content", []) if isinstance(part, dict)
    )
    semantic = {
        "status": status,
        "response_status": body.get("status"),
        "model": body.get("model"),
        "text": text,
        "upstream_path": request[0],
        "upstream_model": request[2].get("model") if isinstance(request[2], dict) else None,
    }
    expected = {
        "status": 200,
        "response_status": "completed",
        "model": UPSTREAM_MODEL,
        "text": FORWARD_TEXT,
        "upstream_path": "/v1/responses",
        "upstream_model": UPSTREAM_MODEL,
    }
    return {
        "elapsed_ms": elapsed_ms,
        "semantic": semantic,
        "passed": semantic == expected and elapsed_ms <= FORWARDING_LIMIT_MS,
        "elapsed_limit_ms": FORWARDING_LIMIT_MS,
    }


def _query_semantics(backend, start: int, end: int) -> dict:
    paths = {
        "all": _usage_url(start, end),
        "external": _usage_url(start, end, "external"),
        "native": _usage_url(start, end, "native"),
        "unknown": _usage_url(start, end, "unknown"),
        "recent_external": _usage_url(end - 3600, end, "external"),
    }
    results = {}
    for name, path in paths.items():
        status, _, payload = _request_json(backend, "GET", path)
        if status != 200:
            raise BenchmarkError(f"usage_filter_{name}_failed_{status}")
        results[name] = canonical_usage(payload)
    invalid_status, _, invalid_body = _request_json(
        backend, "GET", _usage_url(start, end, "not-a-category")
    )
    results["invalid_filter"] = {"status": invalid_status, "body": invalid_body}
    if invalid_status != 400:
        raise BenchmarkError("invalid_usage_filter_did_not_return_400")
    return results


def validate_total_requests(payload: dict, expected: int) -> None:
    actual = payload.get("totals", {}).get("requests")
    if actual != expected:
        raise BenchmarkError(f"usage_total_requests_mismatch_{actual}_{expected}")


def run_runtime(
    command: list[str],
    root: Path,
    seed_usage: Path,
    price_file: Path,
    rollout_source: Path,
    upstream,
    rollout_records: int,
    ledger_rows: int,
    start: int,
    end: int,
    resource_sampler_type,
    emp_process_type,
):
    home = root / "codex"
    (home / "sessions").mkdir(parents=True, exist_ok=True)
    state = root / "state"
    state.mkdir(parents=True, exist_ok=True)
    shutil.copy2(seed_usage, state / "usage.sqlite3")
    shutil.copy2(price_file, state / "api_prices.json")
    native_catalog = root / "native.json"
    native_catalog.write_text('{"models":[]}', encoding="utf-8")
    config = {
        "native_catalog_path": str(native_catalog),
        "providers": [{
            "id": PROVIDER_ID,
            "name": "Usage Scan Fixture",
            "base_url": upstream.base_url,
            "protocol": "responses",
            "auth_mode": "api_key",
            "api_key": PROVIDER_KEY,
        }],
        "models": [{
            "id": MODEL_ID,
            "provider": PROVIDER_ID,
            "upstream_id": UPSTREAM_MODEL,
            "enabled": True,
            "context_window": 256_000,
        }],
        "accounts": [],
    }
    config_path = root / "config.json"
    config_path.write_text(json.dumps(config), encoding="utf-8")

    backend = emp_process_type.from_config(command, config_path, home)
    sampler = None
    sampler_started = False
    try:
        baseline = _wait_baseline(backend)
        previous_scan = baseline["history"].get("last_scan_at")
        rollout = home / "sessions" / "rollout.jsonl"
        shutil.copyfile(rollout_source, rollout)
        # Stable mtime makes checkpoint parity independent of fixture copy order.
        fixed_ns = 1_790_000_000_000_000_000
        os.utime(rollout, ns=(fixed_ns, fixed_ns))

        sampler = resource_sampler_type(backend.process.pid, interval=0.01)
        sampler.start()
        sampler_started = True
        sampler.reset_peak()
        started = time.perf_counter()
        scan_status, _, scan_payload = _request_json(backend, "POST", "/api/usage/scan", {})
        if scan_status != 202:
            raise BenchmarkError(f"usage_scan_post_failed_{scan_status}")

        smoke = None
        last_history = None
        poll_count = 0
        deadline = time.monotonic() + SCAN_TIMEOUT_SECONDS
        while time.monotonic() < deadline:
            status, _, payload = _request_json(backend, "GET", _usage_url(1, 2))
            if status != 200:
                raise BenchmarkError("usage_scan_poll_failed_" + str(status))
            history = payload.get("history", {})
            last_history = history
            poll_count += 1
            if history.get("running") and smoke is None:
                smoke = _forwarding_smoke(backend, upstream)
            completed = (
                not history.get("running", False)
                and history.get("last_scan_at") is not None
                and history.get("last_scan_at") != previous_scan
            )
            if completed:
                break
            time.sleep(POLL_INTERVAL_SECONDS)
        elapsed_seconds = time.perf_counter() - started
        if not last_history or last_history.get("running") or last_history.get("last_scan_at") == previous_scan:
            safe_status = canonical_scan_status(last_history or {})
            raise BenchmarkError(
                "usage_scan_timeout_" + json.dumps(
                    {"poll_count": poll_count, "last_status": safe_status}, sort_keys=True
                )
            )
        scan_peak_rss = sampler.case_peak()

        query_started = time.perf_counter()
        query_status, _, final_payload = _request_json(backend, "GET", _usage_url(start, end))
        if query_status != 200:
            raise BenchmarkError("usage_scan_final_query_failed_" + str(query_status))
        query_results = _query_semantics(backend, start, end)
        query_elapsed_seconds = time.perf_counter() - query_started
        workload_peak_rss = sampler.overall_peak()

        history = final_payload.get("history", {})
        if history.get("running") or history.get("errors") != 0 or history.get("updated") != 1:
            raise BenchmarkError("usage_scan_status_incomplete_" + json.dumps(history, sort_keys=True))
        if smoke is None:
            raise BenchmarkError("forwarding_smoke_did_not_overlap_history_scan")
        if not smoke["passed"]:
            raise BenchmarkError("forwarding_smoke_failed")

        checkpoint = checkpoint_snapshot(state / "usage.sqlite3", home, rollout)
        if checkpoint["history_rows"] != rollout_records:
            raise BenchmarkError(
                f"history_row_count_mismatch_{checkpoint['history_rows']}_{rollout_records}"
            )
        if len(checkpoint["checkpoints"]) != 1:
            raise BenchmarkError("history_checkpoint_count_mismatch")
        saved = checkpoint["checkpoints"][0]["checkpoint"]
        if saved.get("offset") != rollout.stat().st_size or saved.get("size") != rollout.stat().st_size:
            raise BenchmarkError("history_checkpoint_did_not_reach_eof")

        validate_total_requests(query_results["all"], ledger_rows + rollout_records + 1)
        return {
            "scan_post_status": scan_status,
            "scan_post_payload": scan_payload,
            "scan_status": canonical_scan_status(history),
            "elapsed_seconds": elapsed_seconds,
            "query_elapsed_seconds": query_elapsed_seconds,
            "scan_peak_rss_bytes": scan_peak_rss,
            "workload_peak_rss_bytes": workload_peak_rss,
            "usage": query_results,
            "checkpoint": checkpoint,
            "forwarding_smoke": smoke,
        }
    finally:
        if sampler is not None and sampler_started:
            sampler.stop()
        backend.close()


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, required=True,
                        help="explicit official Python EMP v0.11.10 checkout")
    parser.add_argument("--python", type=Path, default=Path(sys.executable))
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--rollout-records", type=int, default=DEFAULT_ROLLOUT_RECORDS)
    parser.add_argument("--ledger-rows", type=int, default=DEFAULT_LEDGER_ROWS)
    parser.add_argument("--elapsed-ratio-limit", type=float, default=ELAPSED_RATIO_LIMIT)
    return parser


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    try:
        validate_parameters(args.rollout_records, args.ledger_rows)
        python_root = args.python_root.resolve(strict=True)
        rust_binary = args.rust_binary.resolve(strict=True)
        if not python_root.is_dir() or not rust_binary.is_file():
            raise BenchmarkError("runtime_path_invalid")
        if args.python.resolve() != Path(sys.executable).resolve():
            raise BenchmarkError("python_must_match_harness_interpreter")
        if args.elapsed_ratio_limit <= 0:
            raise BenchmarkError("elapsed_ratio_limit_must_be_positive")

        os.environ["EMP_PYTHON_ORACLE_ROOT"] = str(python_root)
        os.environ["EMP_RUST_BINARY"] = str(rust_binary)
        repo_root = Path(__file__).resolve().parents[1]
        if str(repo_root) not in sys.path:
            sys.path.insert(0, str(repo_root))
        if str(python_root) not in sys.path:
            sys.path.insert(0, str(python_root))
        support_path = repo_root / "tests" / "rust_e2e_support.py"
        support_spec = importlib.util.spec_from_file_location(
            "emp_usage_scan_rust_e2e_support", support_path
        )
        if support_spec is None or support_spec.loader is None:
            raise BenchmarkError("rust_e2e_support_not_loadable")
        support = importlib.util.module_from_spec(support_spec)
        sys.modules[support_spec.name] = support
        support_spec.loader.exec_module(support)
        EmpProcess, Upstream = support.EmpProcess, support.Upstream
        PYTHON_RUNTIME = support.PYTHON_RUNTIME
        if not PYTHON_RUNTIME.formal_oracle or PYTHON_RUNTIME.root.resolve() != python_root:
            raise BenchmarkError("formal_python_oracle_not_selected")
        if Path(PYTHON_RUNTIME.command("-m", "easy_multi_provider")[0]).resolve() != args.python.resolve():
            raise BenchmarkError("python_command_does_not_match_current_interpreter")
        if not python_root.joinpath("easy_multi_provider", "__init__.py").is_file():
            raise BenchmarkError("python_package_not_found")
        from tools.benchmark_emp_runtime import ResourceSampler

        # Import the version-locked fixture modules only after the formal root is
        # selected by the process helper.
        fixture = _fixture_module(python_root)

        now = int(time.time())
        start, end = now - 45 * 86400, now + 3600
        fixture_report = None
        with tempfile.TemporaryDirectory(prefix="emp-usage-scan-benchmark-") as temporary:
            work = Path(temporary)
            seed_root = work / "seed"
            seed_usage, price_file = seed_fixture(seed_root, args.ledger_rows, now)
            rollout_source = work / "rollout.jsonl"
            rollout_start = now - 4 * 3600
            rollout_bytes = write_rollout(rollout_source, args.rollout_records, rollout_start, fixture)
            fixture_report = {
                "usage_database_sha256": hashlib.sha256(seed_usage.read_bytes()).hexdigest(),
                "usage_database_bytes": seed_usage.stat().st_size,
                "rollout_sha256": hashlib.sha256(rollout_source.read_bytes()).hexdigest(),
                "rollout_bytes": rollout_bytes,
            }
            upstream = Upstream()
            try:
                results = {}
                for name, command in (
                    ("python", [str(args.python.absolute()), "-m", "easy_multi_provider"]),
                    ("rust", [str(rust_binary)]),
                ):
                    results[name] = run_runtime(
                        command, work / name, seed_usage, price_file,
                        rollout_source, upstream, args.rollout_records, args.ledger_rows, start, end,
                        ResourceSampler, EmpProcess,
                    )
            finally:
                upstream.close()

        semantic_equal = (
            results["python"]["usage"] == results["rust"]["usage"]
            and results["python"]["checkpoint"] == results["rust"]["checkpoint"]
            and results["python"]["scan_status"] == results["rust"]["scan_status"]
            and results["python"]["forwarding_smoke"]["semantic"]
            == results["rust"]["forwarding_smoke"]["semantic"]
            and results["python"]["forwarding_smoke"]["passed"]
            and results["rust"]["forwarding_smoke"]["passed"]
        )
        elapsed_ratio = None
        elapsed_pass = None
        if semantic_equal:
            elapsed_ratio = results["rust"]["elapsed_seconds"] / max(results["python"]["elapsed_seconds"], 1e-9)
            elapsed_pass = elapsed_ratio <= args.elapsed_ratio_limit
        errors = []
        if not semantic_equal:
            errors.append("python_rust_usage_semantic_mismatch")
        if elapsed_pass is False:
            errors.append("rust_scan_elapsed_limit_exceeded")

        report = {
            "schema": SCHEMA,
            "status": "measured" if not errors else "failed",
            "errors": errors,
            "comparison_enabled": semantic_equal,
            "parameters": {
                "rollout_records": args.rollout_records,
                "ledger_rows": args.ledger_rows,
                "elapsed_ratio_limit": args.elapsed_ratio_limit,
                "elapsed_ratio": elapsed_ratio,
                "elapsed_pass": elapsed_pass,
                "forwarding_smoke_limit_ms": FORWARDING_LIMIT_MS,
                "forwarding_smoke_elapsed_ratio": (
                    results["rust"]["forwarding_smoke"]["elapsed_ms"]
                    / max(results["python"]["forwarding_smoke"]["elapsed_ms"], 1e-9)
                ),
                "scan_status_route": "POST /api/usage/scan",
                "query_filters": ["all", "external", "native", "unknown", "recent_external"],
                "pagination": "not exposed by the usage API; complete ordered periods are compared",
            },
            "fixture": {
                **fixture_report,
            },
            "results": results,
            "versions": {
                "python_root": str(python_root),
                "python_emp": "0.11.10",
                "rust_binary": str(rust_binary),
            },
        }
        args.output.parent.mkdir(parents=True, exist_ok=True)
        temporary_output = args.output.with_suffix(args.output.suffix + ".tmp")
        temporary_output.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        os.replace(temporary_output, args.output)
        summary = {
            "status": report["status"],
            "errors": report["errors"],
            "comparison_enabled": semantic_equal,
            "elapsed_ratio": elapsed_ratio,
            "elapsed_pass": elapsed_pass,
            "runs": {
                name: {
                    "elapsed_seconds": result["elapsed_seconds"],
                    "query_elapsed_seconds": result["query_elapsed_seconds"],
                    "scan_peak_rss_bytes": result["scan_peak_rss_bytes"],
                    "workload_peak_rss_bytes": result["workload_peak_rss_bytes"],
                    "scan_status": result["scan_status"],
                    "history_rows": result["checkpoint"]["history_rows"],
                    "forwarding_smoke": result["forwarding_smoke"],
                }
                for name, result in results.items()
            },
            "report": str(args.output),
        }
        print(json.dumps(summary, indent=2, sort_keys=True))
        return 0 if report["status"] == "measured" else 1
    except Exception as exc:
        failure = {
            "schema": SCHEMA,
            "status": "error",
            "errors": [_failure_detail(exc)],
        }
        try:
            args.output.parent.mkdir(parents=True, exist_ok=True)
            args.output.write_text(json.dumps(failure, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        except OSError:
            pass
        print(json.dumps(failure, indent=2, sort_keys=True), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
