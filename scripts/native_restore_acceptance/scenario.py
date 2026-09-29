"""Linux manual acceptance scenario for existing native EMP history."""

from __future__ import annotations

import argparse
import json
import os
import platform
import sys
from pathlib import Path
from typing import Any

from .app_server import AppServer
from .assertions import build_assertions, classify
from .common import fail, run_bounded_capture, sha256, sha256_file
from .constants import (
    CALL_ID,
    CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    MODEL,
    OPAQUE,
    PARENT_SUFFIX,
    PROBE_PROMPTS,
    SUMMARY,
    TOOL_ARGUMENTS,
    TOOL_OUTPUT,
)
from .fake_provider import FakeResponsesProvider
from .fixture import (
    extract_thread_id,
    fork_thread,
    make_env,
    run_turn,
    start_thread,
    write_fixture_config,
)
from .history import (
    FileSnapshot,
    append_items,
    append_large_historical_events,
    find_rollout,
    inspect_reports,
    parent_fixture_items,
)

ADDRESS_SPACE_LIMIT_BYTES = 1536 * 1024 * 1024


def state_fingerprints(
    codex_home: Path,
    state_dir: Path,
    rollouts: dict[str, Path],
) -> dict[str, Any]:
    paths: dict[str, Path] = {"config.toml": codex_home / "config.toml"}
    for name, path in rollouts.items():
        paths[f"rollout:{name}"] = path
    for base in (codex_home / ".emp-history-repair", state_dir / "history-repair"):
        if base.is_dir():
            for path in base.rglob("*"):
                if path.is_file():
                    paths[f"repair:{path.relative_to(codex_home)}"] = path
    lease_path = state_dir / "lease.json"
    if lease_path.is_file():
        paths["integration:lease.json"] = lease_path
    output = {}
    for name, path in sorted(paths.items()):
        stat = path.stat()
        output[name] = {
            "size": stat.st_size,
            "sha256": sha256_file(path),
            "mtime_ns": stat.st_mtime_ns,
        }
    return output


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", required=True, type=Path)
    parser.add_argument("--emp-bin", required=True, type=Path)
    parser.add_argument("--root", required=True, type=Path)
    parser.add_argument("--contract", choices=("existing", "created"), default="existing")
    parser.add_argument("--large-history", action="store_true")
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    if platform.system() != "Linux":
        fail("this manual acceptance harness requires Linux, /proc, and /usr/bin/prlimit")

    codex_bin = args.codex_bin.expanduser().resolve(strict=True)
    emp_bin = args.emp_bin.expanduser().resolve(strict=True)
    root = args.root.expanduser().resolve()
    for binary in (codex_bin, emp_bin):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            fail("both supplied binaries must be executable files")
    if root.exists() and (not root.is_dir() or any(root.iterdir())):
        fail("--root must name a new or empty directory")
    if args.contract == "created":
        from .created_flow import run_created_flow

        return run_created_flow(args, ADDRESS_SPACE_LIMIT_BYTES)
    os.umask(0o077)
    root.mkdir(parents=True, exist_ok=True, mode=0o700)

    home = root / "home"
    codex_home = root / "codex-home"
    temp_dir = root / "tmp"
    workspace = root / "workspace"
    state_dir = codex_home / "easy-multi-provider" / "integration"
    for directory in (home / ".config", codex_home, temp_dir, workspace, state_dir):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)

    report: dict[str, Any] = {
        "harness": "native-restore-acceptance-v3",
        "platform": platform.system(),
        "codex_binary": str(codex_bin),
        "emp_binary": str(emp_bin),
        "binary_hashes": {},
        "rss_sampling": {"source": "/proc/<pid>/status:VmHWM", "unknown_value": None},
        "fixture": {
            "history_mode": "paginated",
            "external_compaction_marker": "emp1",
            "summary_label": SUMMARY,
            "opaque_sha256": sha256(OPAQUE.encode("utf-8")),
            "matched_tool_call_id": CALL_ID,
            "tool_arguments_sha256": sha256(TOOL_ARGUMENTS.encode("utf-8")),
            "tool_output_sha256": sha256(TOOL_OUTPUT.encode("utf-8")),
            "known_local_markers": {
                "parent": PARENT_SUFFIX,
                "child": CHILD_SUFFIX,
                "grandchild": GRANDCHILD_SUFFIX,
            },
            "rollback_or_discard_fixture": False,
            "scope": "existing-history restore and real Codex resume",
        },
        "threads": {},
        "requests": [],
        "setup_turn_outcome": None,
        "fixture_setup_completed": False,
        "build_turn_outcomes": {},
        "restore": {},
        "backup_verification": {},
        "resume_thread_ids": {},
        "assertions": {},
        "passed": False,
        "baseline_failure_demonstrated": False,
        "classification": "unexpected_failure",
        "process_limits": {
            "address_space_bytes": ADDRESS_SPACE_LIMIT_BYTES,
            "mechanism": "/usr/bin/prlimit",
        },
    }
    ids: dict[str, str] = {}
    provider: FakeResponsesProvider | None = None
    original_files: dict[str, FileSnapshot] = {}
    final_code = 1

    try:
        report["binary_hashes"] = {
            "codex": {"sha256_before": sha256_file(codex_bin)},
            "emp": {"sha256_before": sha256_file(emp_bin)},
        }
        provider = FakeResponsesProvider()
        env = make_env(home, codex_home, temp_dir, "emp-acceptance-fake-key-only")
        original_config, active_config = write_fixture_config(codex_home, state_dir, provider.base_url)

        version, version_peak = run_bounded_capture(
            [str(codex_bin), "--version"],
            ADDRESS_SPACE_LIMIT_BYTES,
            env=env,
            cwd=root,
            timeout=20,
        )
        report["codex_version"] = version.stdout.strip() if version.returncode == 0 else "unknown"
        report["binary_hashes"]["codex"]["version_peak_rss_bytes"] = version_peak
        if version.returncode != 0 or report["codex_version"] != "codex-cli 0.158.0":
            fail("supplied Codex binary must report exactly codex-cli 0.158.0")

        emp_version, emp_version_peak = run_bounded_capture(
            [str(emp_bin), "--version"],
            ADDRESS_SPACE_LIMIT_BYTES,
            env=env,
            cwd=root,
            timeout=20,
        )
        report["emp_version"] = emp_version.stdout.strip() if emp_version.returncode == 0 else "unknown"
        report["binary_hashes"]["emp"]["version_peak_rss_bytes"] = emp_version_peak
        if emp_version.returncode != 0 or report["emp_version"] != "EMP 0.12.3":
            fail("supplied EMP binary must report exactly EMP 0.12.3")

        with AppServer(codex_bin, env, root / "app-server-create-parent.stderr.log") as server:
            parent_id = start_thread(server, workspace)
        report["setup_turn_outcome"] = "completed"
        report["fixture_setup_completed"] = True
        parent_rollout = find_rollout(codex_home, parent_id)
        append_items(parent_rollout, parent_fixture_items())

        build_outcomes: dict[str, str] = {}
        with AppServer(codex_bin, env, root / "app-server-build-parent.stderr.log") as server:
            server.rpc(
                "thread/resume",
                {
                    "threadId": parent_id,
                    "model": MODEL,
                    "modelProvider": "native_fake",
                    "excludeTurns": True,
                },
            )
            build_outcomes["parent"] = run_turn(server, parent_id, PARENT_SUFFIX)
            child_id = fork_thread(server, parent_id)

        with AppServer(codex_bin, env, root / "app-server-build-child.stderr.log") as server:
            server.rpc(
                "thread/resume",
                {
                    "threadId": child_id,
                    "model": MODEL,
                    "modelProvider": "native_fake",
                    "excludeTurns": True,
                },
            )
            build_outcomes["child"] = run_turn(server, child_id, CHILD_SUFFIX)

        with AppServer(codex_bin, env, root / "app-server-fork-grandchild.stderr.log") as server:
            server.rpc(
                "thread/resume",
                {
                    "threadId": child_id,
                    "model": MODEL,
                    "modelProvider": "native_fake",
                    "excludeTurns": True,
                },
            )
            grandchild_id = fork_thread(server, child_id)

        with AppServer(codex_bin, env, root / "app-server-build-grandchild.stderr.log") as server:
            server.rpc(
                "thread/resume",
                {
                    "threadId": grandchild_id,
                    "model": MODEL,
                    "modelProvider": "native_fake",
                    "excludeTurns": True,
                },
            )
            build_outcomes["grandchild"] = run_turn(server, grandchild_id, GRANDCHILD_SUFFIX)

        report["build_turn_outcomes"] = build_outcomes
        ids = {"parent": parent_id, "child": child_id, "grandchild": grandchild_id}
        paths = {key: find_rollout(codex_home, value) for key, value in ids.items()}
        if args.large_history:
            report["fixture"]["large_historical_rollout"] = append_large_historical_events(
                paths["parent"]
            )
        original_files = {
            thread_id: FileSnapshot.capture(paths[key])
            for key, thread_id in ids.items()
        }
        report["threads"] = {
            key: {
                "id": thread_id,
                "rollout_relative_path": str(paths[key].relative_to(codex_home)),
                "pre_restore_bytes": original_files[thread_id].size,
                "pre_restore_sha256": original_files[thread_id].sha256,
            }
            for key, thread_id in ids.items()
        }

        config_path = codex_home / "config.toml"
        if config_path.read_bytes() != active_config:
            fail("active fixture Codex config changed before EMP restore")

        restore, restore_peak = run_bounded_capture(
            [str(emp_bin), "restore", "--state-dir", str(state_dir), "--json"],
            ADDRESS_SPACE_LIMIT_BYTES,
            env=env,
            cwd=root,
            timeout=180,
        )
        restore_summary: dict[str, Any] = {
            "exit_code": restore.returncode,
            "stdout": restore.stdout,
            "stderr": restore.stderr,
        }
        restore_summary["peak_rss_bytes"] = restore_peak
        restore_summary["peak_rss_measurement"] = (
            "measured" if type(restore_peak) is int and restore_peak > 0 else "unknown"
        )
        restore_summary["address_space_limit_bytes"] = ADDRESS_SPACE_LIMIT_BYTES
        try:
            parsed_restore = json.loads(restore.stdout)
            configuration = parsed_restore.get("configuration")
            if isinstance(configuration, dict):
                for key in ("action", "state", "relation", "lease_status", "conflicts"):
                    if key in configuration:
                        restore_summary[key] = configuration[key]
        except json.JSONDecodeError:
            restore_summary["json_result"] = False
        report["restore"] = restore_summary

        config_after = config_path.read_bytes()
        report["restore"]["config_managed_fields_removed"] = all(
            name not in config_after.decode("utf-8")
            for name in (
                "openai_base_url =",
                "model_catalog_json =",
                "experimental_realtime_ws_base_url =",
            )
        )
        report["restore"]["provider_config_preserved"] = b"[model_providers.native_fake]" in config_after
        report["restore"]["native_config_matches_saved"] = config_after == original_config
        report["restore"]["native_original_config_bytes"] = len(original_config)
        report["backup_verification"] = inspect_reports(codex_home, state_dir, original_files)

        first_state = state_fingerprints(codex_home, state_dir, paths)
        second_restore, second_restore_peak = run_bounded_capture(
            [str(emp_bin), "restore", "--state-dir", str(state_dir), "--json"],
            ADDRESS_SPACE_LIMIT_BYTES,
            env=env,
            cwd=root,
            timeout=180,
        )
        second_state = state_fingerprints(codex_home, state_dir, paths)
        second_config_matches_saved = config_path.read_bytes() == original_config
        report["second_restore"] = {
            "exit_code": second_restore.returncode,
            "peak_rss_bytes": second_restore_peak,
            "peak_rss_measurement": (
                "measured"
                if type(second_restore_peak) is int and second_restore_peak > 0
                else "unknown"
            ),
            "output": second_restore.stdout.strip(),
            "error": second_restore.stderr.strip(),
            "config_matches_saved_native_config": second_config_matches_saved,
            "no_op": (
                second_restore.returncode == 0
                and first_state == second_state
                and second_config_matches_saved
            ),
            "state_before": first_state,
            "state_after": second_state,
        }

        provider.reject_emp1 = True
        turn_outcomes: dict[str, str] = {}
        resumed_thread_ids: dict[str, str] = {}
        with AppServer(codex_bin, env, root / "app-server-verify.stderr.log") as server:
            for key, thread_id in ids.items():
                resumed = server.rpc(
                    "thread/resume",
                    {
                        "threadId": thread_id,
                        "model": MODEL,
                        "modelProvider": "native_fake",
                        "excludeTurns": True,
                    },
                )
                resumed_thread_ids[key] = extract_thread_id(resumed)
                started = server.rpc(
                    "turn/start",
                    {
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": PROBE_PROMPTS[key]}],
                        "model": MODEL,
                        "approvalPolicy": "never",
                    },
                )
                if not isinstance(started, dict):
                    turn_outcomes[key] = "invalid_start_response"
                else:
                    turn_outcomes[key] = server.wait_turn(thread_id)
            report["resume_app_server_peak_rss_bytes"] = server.peak_rss_bytes
            report["resume_app_server_peak_rss_measurement"] = (
                "measured"
                if type(server.peak_rss_bytes) is int and server.peak_rss_bytes > 0
                else "unknown"
            )

        report["turn_outcomes"] = turn_outcomes
        report["resume_thread_ids"] = resumed_thread_ids
        with provider.lock:
            report["requests"] = list(provider.requests)

        report["assertions"] = build_assertions(report, ids)
    except Exception as exc:
        report["error"] = str(exc)
    finally:
        if provider is not None:
            try:
                provider.stop()
            except Exception as exc:
                report["cleanup_error"] = str(exc)
        for name, binary in (("codex", codex_bin), ("emp", emp_bin)):
            try:
                record = report["binary_hashes"].setdefault(name, {})
                record["sha256_after"] = sha256_file(binary)
                record["unchanged"] = record.get("sha256_before") == record["sha256_after"]
            except Exception as exc:
                report["binary_hashes"].setdefault(name, {})["hash_error"] = str(exc)
                report["binary_hashes"][name]["unchanged"] = False
        assertions = report.setdefault("assertions", {})
        assertions["codex_binary_hash_stable"] = (
            report.get("binary_hashes", {}).get("codex", {}).get("unchanged") is True
        )
        assertions["emp_binary_hash_stable"] = (
            report.get("binary_hashes", {}).get("emp", {}).get("unchanged") is True
        )
        report["passed"] = (
            "error" not in report
            and "cleanup_error" not in report
            and bool(assertions)
            and all(assertions.values())
        )
        report["classification"] = classify(report, ids)
        report["baseline_failure_demonstrated"] = (
            report["classification"] == "expected_baseline_failure"
        )
        final_code = 0 if report["passed"] else 1
        report_path = root / "result.json"
        report_path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")

    print(
        json.dumps(
            {
                "passed": report.get("passed", False),
                "classification": report.get("classification"),
                "baseline_failure_demonstrated": report.get("baseline_failure_demonstrated", False),
                "codex_version": report.get("codex_version"),
                "restore": report.get("restore", {}),
                "turn_outcomes": report.get("turn_outcomes", {}),
                "assertions_failed": [
                    key for key, value in report.get("assertions", {}).items() if not value
                ],
                "report": str(root / "result.json"),
                "error": report.get("error") or report.get("cleanup_error"),
            },
            indent=2,
        )
    )
    return final_code
