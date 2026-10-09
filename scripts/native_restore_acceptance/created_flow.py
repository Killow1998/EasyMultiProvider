"""End-to-end acceptance for history created through a disposable live EMP."""

from __future__ import annotations

import argparse
import json
import os
import platform
from pathlib import Path
import socket
from typing import Any

from .app_server import AppServer
from .common import fail, run_bounded_capture, sha256, sha256_file
from .created_assertions import (
    CREATED_LOCAL_MARKERS,
    created_assertions,
    expected_baseline_failure,
)
from .constants import (
    CHILD_SUFFIX,
    CREATED_FLOW_PROMPTS,
    EARLY_CHILD_SUFFIX,
    GRANDCHILD_SUFFIX,
    MODEL,
    OPAQUE,
    OPAQUE_ID,
    PARENT_SUFFIX,
    PROBE_PROMPTS,
    SUMMARY,
    TOOL_ARGUMENTS,
    TOOL_OUTPUT,
)
from .emp_server import EmpServer
from .fake_provider import FakeResponsesProvider
from .fixture import (
    EMP_MODEL_PROVIDER,
    extract_thread_id,
    fork_thread,
    make_env,
    run_turn,
    write_created_flow_config,
)
from .history import FileSnapshot, append_items, find_rollout, inspect_reports


NATIVE_TOKEN = "emp-acceptance-fake-key-only"
UPSTREAM_KEY = "emp-acceptance-upstream-key-only"
ALTERNATE_MODEL = "gpt-5-mini"


def write_emp_configuration(root: Path, upstream_url: str, port: int) -> Path:
    config_path = root / "emp-config.json"
    config = {
        "host": "127.0.0.1",
        "port": port,
        "native_catalog_path": str(root / "native-models.json"),
        "providers": [
            {
                "id": "fixture",
                "base_url": upstream_url,
                "protocol": "responses",
                "auth_mode": "api_key",
                "api_key": UPSTREAM_KEY,
            }
        ],
        "models": [
            {
                "id": MODEL,
                "provider": "fixture",
                "upstream_id": MODEL,
                "context_window": 8192,
                "output_limit": 1024,
                "enabled": True,
            },
            {
                "id": ALTERNATE_MODEL,
                "provider": "fixture",
                "upstream_id": ALTERNATE_MODEL,
                "context_window": 8192,
                "output_limit": 1024,
                "enabled": True,
            },
        ],
    }
    config_path.write_text(json.dumps(config, separators=(",", ":")) + "\n", encoding="utf-8")
    return config_path


def has_emp1(path: Path) -> list[str]:
    ids: list[str] = []

    def walk(value: Any) -> None:
        if isinstance(value, dict):
            if value.get("type") == "compaction" and str(value.get("encrypted_content", "")).startswith("emp1:"):
                ids.append(str(value.get("id", "")))
            for child in value.values():
                walk(child)
        elif isinstance(value, list):
            for child in value:
                walk(child)

    with path.open("rb") as stream:
        for line in stream:
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            walk(record)
    return ids


def read_mcp_calls(path: Path) -> list[dict[str, Any]]:
    if not path.is_file():
        return []
    calls = []
    with path.open("rb") as stream:
        for line in stream:
            try:
                value = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            if isinstance(value, dict):
                calls.append(value)
    return calls


def checkpoint_provider_observations(
    report: dict[str, Any], provider: FakeResponsesProvider, mcp_log: Path
) -> None:
    with provider.lock:
        report["requests"] = list(provider.requests)
    report["mcp_calls"] = read_mcp_calls(mcp_log)


def run_created_flow(args: argparse.Namespace, address_space_limit_bytes: int) -> int:
    codex_bin = args.codex_bin.expanduser().resolve(strict=True)
    emp_bin = args.emp_bin.expanduser().resolve(strict=True)
    root = args.root.expanduser().resolve()
    for binary in (codex_bin, emp_bin):
        if not binary.is_file() or not os.access(binary, os.X_OK):
            fail("both supplied binaries must be executable files")
    if root.exists() and (not root.is_dir() or any(root.iterdir())):
        fail("--root must name a new or empty directory")
    os.umask(0o077)
    root.mkdir(parents=True, exist_ok=True, mode=0o700)

    home = root / "home"
    codex_home = root / "codex-home"
    temp_dir = root / "tmp"
    workspace = root / "workspace"
    state_dir = codex_home / "easy-multi-provider" / "integration"
    mcp_log = root / "mcp-tool-calls.jsonl"
    for directory in (home / ".config", codex_home / "sessions", temp_dir, workspace, state_dir):
        directory.mkdir(parents=True, exist_ok=True, mode=0o700)

    report: dict[str, Any] = {
        "harness": "native-restore-acceptance-created-v1",
        "platform": platform.system(),
        "codex_binary": str(codex_bin),
        "emp_binary": str(emp_bin),
        "binary_hashes": {},
        "process_limits": {"address_space_bytes": address_space_limit_bytes, "mechanism": "/usr/bin/prlimit"},
        "rss_sampling": {"source": "/proc/<pid>/status:VmHWM", "unknown_value": None},
        "fixture": {
            "history_mode": "paginated",
            "summary_label": SUMMARY,
            "opaque_item_sha256": sha256(OPAQUE.encode()),
            "tool_arguments_sha256": sha256(TOOL_ARGUMENTS.encode()),
            "tool_result_sha256": sha256(TOOL_OUTPUT.encode()),
            "synthetic_auth": True,
            "provider_kind": "loopback fake Responses endpoint",
            "live_emp_health_checked": False,
            "external_openai_ciphertext_validation": False,
            "known_local_markers": {
                key: sorted(value) for key, value in CREATED_LOCAL_MARKERS.items()
            },
        },
        "threads": {},
        "requests": [],
        "mcp_calls": [],
        "setup_turn_outcome": None,
        "build_turn_outcomes": {},
        "pre_restore_emp1_threads": [],
        "restore": {},
        "second_restore": {},
        "backup_verification": {},
        "resume_thread_ids": {},
        "resume_turn_outcomes": {},
        "assertions": {},
        "passed": False,
        "baseline_failure_demonstrated": False,
        "classification": "unexpected_failure",
    }
    provider: FakeResponsesProvider | None = None
    emp: EmpServer | None = None
    ids: dict[str, str] = {}
    original_files: dict[str, FileSnapshot] = {}
    paths: dict[str, Path] = {}
    env = make_env(home, codex_home, temp_dir, NATIVE_TOKEN)
    final_code = 1

    try:
        report["binary_hashes"] = {
            "codex": {"sha256_before": sha256_file(codex_bin)},
            "emp": {"sha256_before": sha256_file(emp_bin)},
        }
        codex_version, codex_peak = run_bounded_capture(
            [str(codex_bin), "--version"], address_space_limit_bytes, env=env, cwd=root, timeout=20
        )
        report["codex_version"] = codex_version.stdout.strip() if codex_version.returncode == 0 else "unknown"
        report["binary_hashes"]["codex"]["version_peak_rss_bytes"] = codex_peak
        if codex_version.returncode != 0 or not report["codex_version"].startswith("codex-cli "):
            fail("unable to read the supplied Codex binary version")

        emp_version, emp_peak = run_bounded_capture(
            [str(emp_bin), "--version"], address_space_limit_bytes, env=env, cwd=root, timeout=20
        )
        report["emp_version"] = emp_version.stdout.strip() if emp_version.returncode == 0 else "unknown"
        report["binary_hashes"]["emp"]["version_peak_rss_bytes"] = emp_peak
        if emp_version.returncode != 0 or not report["emp_version"].startswith("EMP "):
            fail("unable to read the supplied EMP binary version")

        auth = {"tokens": {"access_token": NATIVE_TOKEN, "account_id": "fixture-account"}}
        auth_path = codex_home / "auth.json"
        auth_path.write_text(json.dumps(auth, separators=(",", ":")) + "\n", encoding="utf-8")
        auth_path.chmod(0o600)
        (root / "native-models.json").write_text('{"models":[]}\n', encoding="utf-8")
        provider = FakeResponsesProvider()
        with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as reservation:
            reservation.bind(("127.0.0.1", 0))
            emp_port = reservation.getsockname()[1]
        emp_config = write_emp_configuration(root, provider.base_url, emp_port)
        emp = EmpServer(
            emp_bin,
            emp_config,
            root,
            env,
            root / "emp.stderr.log",
            address_space_limit_bytes,
            emp_port,
        )
        report["fixture"]["live_emp_health_checked"] = True
        native_config, active_config = write_created_flow_config(
            codex_home,
            state_dir,
            provider.base_url,
            emp.api_base_url,
            mcp_log,
            emp.pid,
        )

        with AppServer(
            codex_bin,
            env,
            root / "app-server-create-and-fork.stderr.log",
            address_space_limit_bytes,
        ) as server:
            started = server.rpc(
                "thread/start",
                {
                    "cwd": str(workspace),
                    "model": MODEL,
                    "modelProvider": EMP_MODEL_PROVIDER,
                    "historyMode": "paginated",
                    "ephemeral": False,
                    "approvalPolicy": "never",
                    "sandbox": "read-only",
                },
            )
            if not isinstance(started, dict) or not isinstance(started.get("thread"), dict):
                fail("Codex did not return parent thread metadata")
            parent_id = started["thread"].get("id")
            if not isinstance(parent_id, str) or not parent_id:
                fail("Codex did not return parent thread id")
            report["setup_turn_outcome"] = "thread_created"

            outcomes: dict[str, str] = {}
            outcomes["tool_before_compaction"] = run_turn(
                server, parent_id, CREATED_FLOW_PROMPTS["tool_before_compaction"]
            )
            early_child_id = fork_thread(server, parent_id)
            server.rpc(
                "thread/resume",
                {
                    "threadId": early_child_id,
                    "model": MODEL,
                    "modelProvider": EMP_MODEL_PROVIDER,
                    "excludeTurns": True,
                },
            )
            outcomes["early_child"] = run_turn(
                server,
                early_child_id,
                f"{EARLY_CHILD_SUFFIX} {CREATED_FLOW_PROMPTS['early_child']}",
            )

            server.rpc(
                "thread/resume",
                {
                    "threadId": parent_id,
                    "model": MODEL,
                    "modelProvider": EMP_MODEL_PROVIDER,
                    "excludeTurns": True,
                },
            )
            outcomes["compaction_threshold"] = run_turn(
                server, parent_id, CREATED_FLOW_PROMPTS["compaction_threshold"]
            )
            outcomes["compaction_followup"] = run_turn(
                server, parent_id, CREATED_FLOW_PROMPTS["compaction_followup"]
            )
            outcomes["tool_after_compaction"] = run_turn(
                server, parent_id, CREATED_FLOW_PROMPTS["tool_after_compaction"]
            )
            outcomes["build_parent"] = run_turn(server, parent_id, PARENT_SUFFIX)

            late_child_id = fork_thread(server, parent_id)
            server.rpc(
                "thread/resume",
                {
                    "threadId": late_child_id,
                    "model": ALTERNATE_MODEL,
                    "modelProvider": EMP_MODEL_PROVIDER,
                    "excludeTurns": True,
                },
            )
            outcomes["build_child"] = run_turn(
                server,
                late_child_id,
                f"{CHILD_SUFFIX} {CREATED_FLOW_PROMPTS['late_child']}",
                model=ALTERNATE_MODEL,
            )
            grandchild_id = fork_thread(server, late_child_id)
            server.rpc(
                "thread/resume",
                {
                    "threadId": grandchild_id,
                    "model": MODEL,
                    "modelProvider": EMP_MODEL_PROVIDER,
                    "excludeTurns": True,
                },
            )
            outcomes["build_grandchild"] = run_turn(
                server,
                grandchild_id,
                f"{GRANDCHILD_SUFFIX} {CREATED_FLOW_PROMPTS['grandchild']}",
            )
            app_server_peak = server.peak_rss_bytes

        ids = {
            "parent": parent_id,
            "early_child": early_child_id,
            "child": late_child_id,
            "grandchild": grandchild_id,
        }
        report["build_turn_outcomes"] = outcomes
        report["app_server_peak_rss_bytes"] = app_server_peak
        if (codex_home / "config.toml").read_bytes() != active_config:
            fail("Codex integration settings changed before EMP restore")
        if (codex_home / "easy-multi-provider" / "integration" / "lease.json").is_file() is False:
            fail("fixture integration lease was not created")

        paths = {key: find_rollout(codex_home, thread_id) for key, thread_id in ids.items()}
        for key, path in paths.items():
            append_items(
                path,
                [
                    {
                        "type": "reasoning",
                        "id": OPAQUE_ID,
                        "summary": [],
                        "encrypted_content": OPAQUE,
                    }
                ],
            )
        original_files = {
            thread_id: FileSnapshot.capture(paths[key]) for key, thread_id in ids.items()
        }
        report["pre_restore_emp1_threads"] = sorted(
            key for key, path in paths.items() if has_emp1(path)
        )
        report["threads"] = {
            key: {
                "id": thread_id,
                "rollout_relative_path": str(paths[key].relative_to(codex_home)),
                "pre_restore_bytes": original_files[thread_id].size,
                "pre_restore_sha256": original_files[thread_id].sha256,
                "emp1_ids_before_restore": has_emp1(paths[key]),
            }
            for key, thread_id in ids.items()
        }
        checkpoint_provider_observations(report, provider, mcp_log)

        if emp is not None:
            emp.stop()
            report["emp_peak_rss_bytes"] = emp.peak_rss_bytes
            emp = None
        report["shutdown_restored_config"] = (codex_home / "config.toml").read_bytes() == native_config

        restore, restore_peak = run_bounded_capture(
            [str(emp_bin), "restore", "--state-dir", str(state_dir), "--json"],
            address_space_limit_bytes,
            env=env,
            cwd=root,
            timeout=180,
        )
        restore_summary: dict[str, Any] = {
            "exit_code": restore.returncode,
            "peak_rss_bytes": restore_peak,
            "address_space_limit_bytes": address_space_limit_bytes,
            "peak_rss_measurement": "measured" if type(restore_peak) is int and restore_peak > 0 else "unknown",
            "stdout": restore.stdout,
            "stderr": restore.stderr,
        }
        try:
            parsed_restore = json.loads(restore.stdout)
            configuration = parsed_restore.get("configuration")
            if isinstance(configuration, dict):
                for key in ("action", "state", "relation", "lease_status", "conflicts"):
                    if key in configuration:
                        restore_summary[key] = configuration[key]
        except json.JSONDecodeError:
            restore_summary["json_result"] = False
        config_after = (codex_home / "config.toml").read_bytes()
        restore_summary["native_config_matches_saved"] = config_after == native_config
        restore_summary["native_base_url_restored"] = (
            f'openai_base_url = {json.dumps(provider.base_url)}'.encode("utf-8") in config_after
        )
        restore_summary["provider_config_preserved"] = config_after == native_config
        report["restore"] = restore_summary
        report["backup_verification"] = inspect_reports(codex_home, state_dir, original_files)

        before_second = state_fingerprints(codex_home, state_dir, paths)
        second, second_peak = run_bounded_capture(
            [str(emp_bin), "restore", "--state-dir", str(state_dir), "--json"],
            address_space_limit_bytes,
            env=env,
            cwd=root,
            timeout=180,
        )
        after_second = state_fingerprints(codex_home, state_dir, paths)
        second_config_matches_native = (codex_home / "config.toml").read_bytes() == native_config
        report["second_restore"] = {
            "exit_code": second.returncode,
            "peak_rss_bytes": second_peak,
            "peak_rss_measurement": "measured" if type(second_peak) is int and second_peak > 0 else "unknown",
            "output": second.stdout.strip(),
            "error": second.stderr.strip(),
            "config_matches_saved_native_config": second_config_matches_native,
            "no_op": second.returncode == 0 and before_second == after_second and second_config_matches_native,
            "state_before": before_second,
            "state_after": after_second,
        }
        if not restore_summary["native_config_matches_saved"]:
            fail("EMP restore did not restore the exact saved native config")
        if native_config == active_config:
            fail("fixture failed to distinguish native and EMP settings")

        provider.reject_emp1 = True
        resume_outcomes: dict[str, str] = {}
        resume_thread_ids: dict[str, str] = {}
        with AppServer(
            codex_bin,
            env,
            root / "app-server-native-resume.stderr.log",
            address_space_limit_bytes,
        ) as server:
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
                resume_thread_ids[key] = extract_thread_id(resumed)
                started_turn = server.rpc(
                    "turn/start",
                    {
                        "threadId": thread_id,
                        "input": [{"type": "text", "text": PROBE_PROMPTS[key]}],
                        "model": MODEL,
                        "approvalPolicy": "never",
                    },
                )
                resume_outcomes[key] = (
                    server.wait_turn(thread_id) if isinstance(started_turn, dict) else "invalid_start_response"
                )
            report["native_resume_peak_rss_bytes"] = server.peak_rss_bytes

        report["resume_thread_ids"] = resume_thread_ids
        report["resume_turn_outcomes"] = resume_outcomes
        checkpoint_provider_observations(report, provider, mcp_log)
        requests = report["requests"]
        report["assertions"] = created_assertions(report, ids, requests, report["mcp_calls"])
    except Exception as exc:
        report["error"] = str(exc)
    finally:
        if emp is not None:
            try:
                emp.stop()
                report["emp_peak_rss_bytes"] = emp.peak_rss_bytes
            except Exception as exc:
                report["cleanup_error"] = str(exc)
        if provider is not None:
            try:
                provider.stop()
                checkpoint_provider_observations(report, provider, mcp_log)
            except Exception as exc:
                report["cleanup_error"] = str(exc)
        for name, binary in (("codex", codex_bin), ("emp", emp_bin)):
            try:
                record = report["binary_hashes"].setdefault(name, {})
                record["sha256_after"] = sha256_file(binary)
                record["unchanged"] = record.get("sha256_before") == record["sha256_after"]
            except Exception as exc:
                report["binary_hashes"].setdefault(name, {})["unchanged"] = False
                report["binary_hashes"][name]["hash_error"] = str(exc)
        report.setdefault("assertions", {})["codex_binary_hash_stable"] = (
            report.get("binary_hashes", {}).get("codex", {}).get("unchanged") is True
        )
        report["assertions"]["emp_binary_hash_stable"] = (
            report.get("binary_hashes", {}).get("emp", {}).get("unchanged") is True
        )
        report["passed"] = (
            "error" not in report
            and "cleanup_error" not in report
            and bool(report["assertions"])
            and all(report["assertions"].values())
        )
        report["classification"] = (
            "candidate_success"
            if report["passed"]
            else "expected_baseline_failure"
            if expected_baseline_failure(report, ids, report.get("requests", []))
            else "unexpected_failure"
        )
        report["baseline_failure_demonstrated"] = report["classification"] == "expected_baseline_failure"
        final_code = 0 if report["passed"] else 1
        (root / "result.json").write_text(
            json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )

    print(
        json.dumps(
            {
                "passed": report.get("passed", False),
                "classification": report.get("classification"),
                "codex_version": report.get("codex_version"),
                "emp_version": report.get("emp_version"),
                "build_turn_outcomes": report.get("build_turn_outcomes", {}),
                "restore": report.get("restore", {}),
                "second_restore": {"no_op": report.get("second_restore", {}).get("no_op")},
                "resume_turn_outcomes": report.get("resume_turn_outcomes", {}),
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
