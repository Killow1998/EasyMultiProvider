#!/usr/bin/env python3
"""Compare isolated quota-history management responses from Python and Rust EMP."""

from __future__ import annotations

import argparse
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
from urllib.parse import quote


SCHEMA = 1
SAMPLE_INTERVAL_SECONDS = 300
RETENTION_SAMPLES = 15 * 24 * 60 * 60 // SAMPLE_INTERVAL_SECONDS
DEFAULT_SAMPLES = 3_000
RANGE_NAMES = ("1h", "1d", "1w", "all")
RANGE_SECONDS = {"1h": 3_600, "1d": 86_400, "1w": 604_800, "all": 1_296_000}
ACCOUNT_ID = "@native"


class BenchmarkError(RuntimeError):
    pass


def failure_detail(exc: Exception) -> str:
    if isinstance(exc, BenchmarkError):
        return str(exc)
    message = str(exc)
    if len(message) > 1_000:
        message = message[:200] + " … [truncated] … " + message[-800:]
    return f"{type(exc).__name__}: {message}"


def validate_sample_count(sample_count: int) -> None:
    if not 1 <= sample_count <= RETENTION_SAMPLES:
        raise BenchmarkError(f"sample_count_must_be_1_to_{RETENTION_SAMPLES}")


def plan_type_for(index: int, sample_count: int) -> str:
    first = sample_count // 3
    second = 2 * sample_count // 3
    if index < first:
        return "plus"
    if index < second:
        return "ProLite"
    return "pro"


def fixture_snapshot(index: int, observed_at: int, sample_count: int, snapshot_factory) -> dict:
    """Extend the existing Python quota-history fixture with supported buckets."""
    primary_used = (index * 7) % 101
    secondary_used = (index * 13) % 101
    snapshot = snapshot_factory(
        used_primary=primary_used,
        used_secondary=secondary_used,
        plan_type=plan_type_for(index, sample_count),
    )
    codex = snapshot["rate_limits"]
    # Keep the official test fixture's 5h and 7d quota windows, with stable
    # per-sample reset timestamps so every range has deterministic point data.
    codex["primary"]["windowDurationMins"] = 300
    codex["primary"]["resetsAt"] = observed_at + 300 * 60
    codex["secondary"]["windowDurationMins"] = 10_080
    codex["secondary"]["resetsAt"] = observed_at + 10_080 * 60
    other_used = (index * 17) % 101
    snapshot["rate_limits_by_limit_id"] = {
        "codex": codex,
        # The formal quota parser fixture accepts multiple limit IDs; this
        # third persisted series ensures the history view scans that shape.
        "other": {
            "primary": {
                "usedPercent": other_used,
                "windowDurationMins": 60,
                "resetsAt": observed_at + 60 * 60,
            }
        },
    }
    # These safe quota fields are present in Python's parsed API response, but
    # QuotaHistoryStore intentionally persists only rate-limit windows and plan
    # type. Tests and the report must not claim the balance/reset IDs survive.
    snapshot["credits"] = {
        "has_credits": True,
        "unlimited": False,
        "balance": str(42 + index % 17),
        "individual_limit": {
            "limit": "100",
            "used": str(index % 100),
            "remaining_percent": 100 - index % 100,
            "resets_at": observed_at + 60 * 60,
        },
        "reset_credits": {
            "available_count": index % 3,
            "credits": [{
                "status": "available",
                "expires_at": observed_at + 7 * 86400,
            }],
        },
    }
    return snapshot


def quota_history_path(root: Path) -> Path:
    return root / "state" / "quota_history.sqlite3"


def seed_quota_history(database: Path, sample_count: int, now: int, snapshot_factory) -> dict:
    """Seed by calling the official QuotaHistoryStore.append_snapshot API."""
    validate_sample_count(sample_count)
    database.parent.mkdir(parents=True, exist_ok=True)
    from easy_multi_provider.quota_history import QuotaHistoryStore

    store = QuotaHistoryStore(database)
    latest = now - now % SAMPLE_INTERVAL_SECONDS
    first = latest - (sample_count - 1) * SAMPLE_INTERVAL_SECONDS
    inserted_rows = 0
    for index in range(sample_count):
        observed_at = first + index * SAMPLE_INTERVAL_SECONDS
        inserted_rows += store.append_snapshot(
            ACCOUNT_ID,
            fixture_snapshot(index, observed_at, sample_count, snapshot_factory),
            observed_at=observed_at,
        )
    with sqlite3.connect(database) as connection:
        stored_rows = connection.execute(
            "SELECT COUNT(*) FROM quota_samples WHERE account_key = ?", (ACCOUNT_ID,)
        ).fetchone()[0]
        columns = [row[1] for row in connection.execute("PRAGMA table_info(quota_samples)")]
    return {
        "sample_buckets": sample_count,
        "inserted_window_rows": inserted_rows,
        "stored_rows": int(stored_rows),
        "columns": columns,
        "first_observed_at": first,
        "last_observed_at": latest,
        "credit_fields_supplied": True,
        "credit_metadata_persisted": False,
    }


def quota_history_query_path(account_id: str, range_name: str) -> str:
    if range_name not in RANGE_NAMES:
        raise BenchmarkError("unsupported_quota_history_range")
    return f"/api/accounts/{quote(account_id, safe='')}/quota-history?range={range_name}"


def quota_history_queries(account_id: str = ACCOUNT_ID) -> dict[str, str]:
    return {name: quota_history_query_path(account_id, name) for name in RANGE_NAMES}


def strict_json_equal(left, right) -> bool:
    """Object key order is irrelevant; array order and JSON number types are strict."""
    if type(left) is not type(right):
        return False
    if isinstance(left, dict):
        return left.keys() == right.keys() and all(
            strict_json_equal(left[key], right[key]) for key in left
        )
    if isinstance(left, list):
        return len(left) == len(right) and all(
            strict_json_equal(a, b) for a, b in zip(left, right)
        )
    return left == right


def quota_response_for_comparison(
    payload: dict,
    range_name: str,
    account_id: str = ACCOUNT_ID,
    *,
    current_epoch: int | None = None,
) -> dict:
    expected_keys = {
        "account_id", "range", "start_at", "end_at", "sample_interval_seconds",
        "retention_days", "series", "plans",
    }
    if set(payload) != expected_keys:
        raise BenchmarkError("quota_history_response_schema_mismatch")
    if payload["account_id"] != account_id or payload["range"] != range_name:
        raise BenchmarkError("quota_history_response_scope_mismatch")
    if type(payload["start_at"]) is not int or type(payload["end_at"]) is not int:
        raise BenchmarkError("quota_history_response_bounds_not_integer")
    if payload["end_at"] - payload["start_at"] != RANGE_SECONDS[range_name]:
        raise BenchmarkError("quota_history_response_range_bounds_mismatch")
    if current_epoch is not None and abs(payload["end_at"] - current_epoch) > 30:
        raise BenchmarkError("quota_history_response_end_not_current")
    if payload["sample_interval_seconds"] != SAMPLE_INTERVAL_SECONDS or payload["retention_days"] != 15:
        raise BenchmarkError("quota_history_response_retention_mismatch")
    if not isinstance(payload["series"], list) or not isinstance(payload["plans"], list):
        raise BenchmarkError("quota_history_response_lists_missing")
    value = json.loads(json.dumps(payload))
    # The API computes these two envelope bounds from process-local system_now.
    # Compare their validated range relationship while keeping every stored
    # observed_at/reset/plan timestamp type-sensitive and exact.
    value["start_at"] = "now-minus-range"
    value["end_at"] = "now"
    return value


def _load_official_fixtures(python_root: Path):
    fixture_path = python_root / "tests" / "test_quota_history.py"
    if not fixture_path.is_file():
        raise BenchmarkError("official_quota_history_fixture_not_found")
    spec = importlib.util.spec_from_file_location("emp_official_quota_history_fixture", fixture_path)
    if spec is None or spec.loader is None:
        raise BenchmarkError("official_quota_history_fixture_unloadable")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def build_parser() -> argparse.ArgumentParser:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, required=True,
                        help="official Python EMP v0.11.10 checkout")
    parser.add_argument("--python", type=Path, default=Path(sys.executable),
                        help="the venv interpreter used to run Python EMP")
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--fixture-dir", type=Path, required=True,
                        help="isolated directory where quota_history.sqlite3 and manifest are written")
    parser.add_argument("--output", type=Path, default=None)
    parser.add_argument("--samples", type=int, default=DEFAULT_SAMPLES)
    parser.add_argument("--now", type=int, default=None,
                        help="fixed UNIX time for reproducible 5-minute fixture buckets")
    return parser


def _load_process_support(repo_root: Path, python_root: Path, rust_binary: Path):
    os.environ["EMP_PYTHON_ORACLE_ROOT"] = str(python_root)
    os.environ["EMP_RUST_BINARY"] = str(rust_binary)
    support_path = repo_root / "tests" / "rust_e2e_support.py"
    spec = importlib.util.spec_from_file_location("emp_quota_history_e2e_support", support_path)
    if spec is None or spec.loader is None:
        raise BenchmarkError("rust_e2e_support_unloadable")
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    runtime = module.PYTHON_RUNTIME
    if not runtime.formal_oracle or runtime.root.resolve() != python_root:
        raise BenchmarkError("formal_python_oracle_not_selected")
    if runtime.root.joinpath("easy_multi_provider", "__init__.py").is_file() is False:
        raise BenchmarkError("formal_python_package_not_found")
    return module


def _runtime_root(root: Path, fixture_database: Path, now: int, config_factory):
    root.mkdir(parents=True, exist_ok=False)
    home = root / "codex"
    home.mkdir()
    state = root / "state"
    state.mkdir()
    shutil.copy2(fixture_database, quota_history_path(root))
    price_rates = {
        "fixture-model": {
            "input_cost_per_token": "0.000001",
            "output_cost_per_token": "0.000001",
        }
    }
    (state / "api_prices.json").write_text(json.dumps({
        "fetched_at": float(now - 60), "prices": price_rates,
    }, separators=(",", ":")), encoding="utf-8")
    native_catalog = root / "native.json"
    native_catalog.write_text('{"models":[]}', encoding="utf-8")
    config = config_factory(root)
    config["native_catalog_path"] = str(native_catalog)
    config_path = root / "config.json"
    config_path.write_text(json.dumps(config), encoding="utf-8")
    return config_path, home


def _read_json_response(backend, path: str):
    status, _, raw = backend.request("GET", path)
    try:
        payload = json.loads(raw)
    except (UnicodeDecodeError, json.JSONDecodeError):
        payload = {"raw_sha256": hashlib.sha256(raw).hexdigest()}
    return status, payload


def _run_runtime(
    command: list[str],
    root: Path,
    fixture_database: Path,
    now: int,
    config_factory,
    process_type,
    resource_sampler_type,
    service_activity: dict | None = None,
):
    config_path, home = _runtime_root(root, fixture_database, now, config_factory)
    backend = process_type.from_config(command, config_path, home)
    if service_activity is not None:
        service_activity["services_started"] = True
    sampler = None
    sampler_started = False
    try:
        sampler = resource_sampler_type(backend.process.pid, interval=0.02)
        sampler.start()
        sampler_started = True
        queries = {}
        for range_name, path in quota_history_queries().items():
            started = time.perf_counter()
            status, payload = _read_json_response(backend, path)
            elapsed = time.perf_counter() - started
            if status != 200:
                raise BenchmarkError(f"quota_history_{range_name}_status_{status}")
            comparable = quota_response_for_comparison(
                payload, range_name, current_epoch=int(time.time())
            )
            queries[range_name] = {
                "elapsed_seconds": elapsed,
                "response": comparable,
            }

        invalid_path = quota_history_query_path(ACCOUNT_ID, "1d").replace("range=1d", "range=2d")
        invalid_status, invalid_payload = _read_json_response(backend, invalid_path)
        if invalid_status != 400:
            raise BenchmarkError(f"quota_history_invalid_range_status_{invalid_status}")
        result = {
            "queries": queries,
            "invalid_range": {"status": invalid_status, "body": invalid_payload},
            "peak_rss_bytes": sampler.overall_peak(),
        }
        return result
    finally:
        if sampler is not None and sampler_started:
            sampler.stop()
        backend.close()


def strict_result_match(python_result: dict, rust_result: dict) -> bool:
    python_queries = {
        name: value["response"] for name, value in python_result["queries"].items()
    }
    rust_queries = {
        name: value["response"] for name, value in rust_result["queries"].items()
    }
    return (
        strict_json_equal(python_queries, rust_queries)
        and strict_json_equal(
            python_result["invalid_range"], rust_result["invalid_range"]
        )
    )


def main(argv: list[str] | None = None) -> int:
    args = build_parser().parse_args(argv)
    service_activity = {"services_started": False}
    try:
        validate_sample_count(args.samples)
        python_root = args.python_root.resolve(strict=True)
        if not python_root.is_dir():
            raise BenchmarkError("python_root_not_directory")
        rust_binary = args.rust_binary.resolve(strict=True)
        if not rust_binary.is_file():
            raise BenchmarkError("rust_binary_not_file")
        if args.python.resolve() != Path(sys.executable).resolve():
            raise BenchmarkError("python_must_match_harness_interpreter")
        sys.path.insert(0, str(python_root))
        import easy_multi_provider
        if easy_multi_provider.__version__ != "0.11.10":
            raise BenchmarkError("official_python_emp_must_be_0.11.10")
        fixtures = _load_official_fixtures(python_root)
        fixture_dir = args.fixture_dir.resolve()
        fixture_dir.mkdir(parents=True, exist_ok=True)
        now = int(time.time()) if args.now is None else args.now
        database = quota_history_path(fixture_dir)
        if database.exists():
            raise BenchmarkError("fixture_database_already_exists")
        result = seed_quota_history(database, args.samples, now, fixtures.snapshot)
        manifest = {
            "schema": SCHEMA,
            "python_emp": easy_multi_provider.__version__,
            "database": str(database),
            "database_sha256": hashlib.sha256(database.read_bytes()).hexdigest(),
            "now": now,
            "ranges": list(RANGE_NAMES),
            "query_paths": quota_history_queries(),
            "retention_samples": RETENTION_SAMPLES,
            **result,
        }
        (fixture_dir / "fixture.json").write_text(
            json.dumps(manifest, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        repo_root = Path(__file__).resolve().parents[1]
        if str(repo_root) not in sys.path:
            sys.path.append(str(repo_root))
        support = _load_process_support(repo_root, python_root, rust_binary)
        from tools.benchmark_emp_runtime import ResourceSampler
        if Path(support.PYTHON_RUNTIME.command("-m", "easy_multi_provider")[0]).resolve() != args.python.resolve():
            raise BenchmarkError("python_command_does_not_match_current_interpreter")

        python_command = [str(args.python.absolute()), "-m", "easy_multi_provider"]
        with tempfile.TemporaryDirectory(prefix="emp-quota-history-processes-") as temporary:
            root = Path(temporary)
            python_result = _run_runtime(
                python_command,
                root / "python",
                database,
                now,
                support._integration_test_config,
                support.EmpProcess,
                ResourceSampler,
                service_activity,
            )
            rust_result = _run_runtime(
                [str(rust_binary)],
                root / "rust",
                database,
                now,
                support._integration_test_config,
                support.EmpProcess,
                ResourceSampler,
                service_activity,
            )
        semantics_equal = strict_result_match(python_result, rust_result)
        errors = [] if semantics_equal else ["python_rust_quota_history_response_mismatch"]
        python_elapsed = sum(query["elapsed_seconds"] for query in python_result["queries"].values())
        rust_elapsed = sum(query["elapsed_seconds"] for query in rust_result["queries"].values())
        output_path = (args.output or fixture_dir / "report.json").resolve()
        report = {
            "schema": SCHEMA,
            "status": "measured" if semantics_equal else "failed",
            "services_started": True,
            "errors": errors,
            "comparison_enabled": semantics_equal,
            "semantics_equal": semantics_equal,
            "parameters": {
                "samples": args.samples,
                "ranges": list(RANGE_NAMES),
                "invalid_range_status": 400,
                "credit_metadata_persisted": False,
            },
            "fixture": manifest,
            "results": {"python": python_result, "rust": rust_result},
            "query_elapsed_seconds": {
                "python": python_elapsed,
                "rust": rust_elapsed,
                "rust_python_ratio": rust_elapsed / max(python_elapsed, 1e-9),
            },
            "versions": {
                "python_root": str(python_root),
                "python_emp": easy_multi_provider.__version__,
                "rust_binary": str(rust_binary),
            },
        }
        output_path.parent.mkdir(parents=True, exist_ok=True)
        output_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")
        print(json.dumps({
            "status": report["status"],
            "services_started": True,
            "errors": errors,
            "comparison_enabled": semantics_equal,
            "semantics_equal": semantics_equal,
            "samples": args.samples,
            "stored_rows": result["stored_rows"],
            "query_elapsed_seconds": report["query_elapsed_seconds"],
            "credit_metadata_persisted": False,
            "report": str(output_path),
        }, indent=2, sort_keys=True))
        return 0 if semantics_equal else 1
    except Exception as exc:
        print(json.dumps({
            "status": "error",
            "error": failure_detail(exc),
            "services_started": service_activity["services_started"],
        }, indent=2, sort_keys=True), file=sys.stderr)
        return 2


if __name__ == "__main__":
    raise SystemExit(main())
