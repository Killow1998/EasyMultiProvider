"""Quiet runtime migration, followed by native resume using the same thread ID."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import socket
import subprocess
import time

from .app_server import AppServer
from .common import sha256_file
from .constants import PROBE_PROMPTS, SUMMARY
from .emp_server import EmpServer
from .fake_provider import FakeResponsesProvider
from .fixture import (
    make_env, resume_thread, run_turn, start_thread,
    write_emp_configuration, write_fixture_config,
)
from .history import append_items, find_rollout, parent_fixture_items


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--codex-bin", required=True, type=Path)
    parser.add_argument("--emp-bin", required=True, type=Path)
    parser.add_argument("--root", required=True, type=Path)
    args = parser.parse_args()
    codex_bin = args.codex_bin.resolve()
    emp_bin = args.emp_bin.resolve()
    root = args.root.resolve()
    root.mkdir(mode=0o700)
    home, codex_home, temporary, workspace = [root / name for name in ("home", "codex", "tmp", "workspace")]
    state_dir = codex_home / "easy-multi-provider/integration"
    for path in (home / ".config", codex_home, temporary, workspace, state_dir):
        path.mkdir(parents=True, exist_ok=True)
    env = make_env(home, codex_home, temporary, "sk-synthetic-acceptance-only")
    limit = 1536 * 1024 * 1024
    emp = None
    version = subprocess.run([str(codex_bin), "--version"], env=env, check=True, capture_output=True, text=True, timeout=10).stdout.strip()
    report = {"codex_version": version, "real_provider_calls": 0, "passed": False,
        "binary_sha256": {"codex": sha256_file(codex_bin), "emp": sha256_file(emp_bin)}}
    provider = FakeResponsesProvider()
    provider.reject_emp1 = True
    try:
        native, _ = write_fixture_config(codex_home, state_dir, provider.base_url)
        (state_dir / "lease.json").unlink()
        (codex_home / "config.toml").write_bytes(native)
        (codex_home / "auth.json").write_text(json.dumps({
            "auth_mode": "apikey", "OPENAI_API_KEY": "sk-synthetic-acceptance-only"}))
        with AppServer(codex_bin, env, root / "create.log", limit) as server:
            thread_id = start_thread(server, workspace)
        rollout = find_rollout(codex_home, thread_id)
        append_items(rollout, parent_fixture_items())
        original = rollout.read_bytes()
        with AppServer(codex_bin, env, root / "before.log", limit) as server:
            resume_thread(server, thread_id)
            before = run_turn(server, thread_id, PROBE_PROMPTS["parent"])
        assert before != "completed", "baseline must reject the private checkpoint"
        report["baseline_upstream_status"] = provider.requests[-1]["http_status"]
        assert report["baseline_upstream_status"] == 400
        baseline = rollout.read_bytes()
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        config_path = write_emp_configuration(root, provider.base_url, port)
        config = json.loads(config_path.read_text())
        config["auto_enable_on_start"] = True
        config_path.write_text(json.dumps(config))
        (root / "native-models.json").write_text('{"models":[]}')
        emp = EmpServer(emp_bin, config_path, root, env, root / "emp.log", limit, port)
        started = time.monotonic()
        deadline = started + 45
        converted = False
        while time.monotonic() < deadline:
            rows = [json.loads(line) for line in rollout.read_bytes().splitlines() if line]
            checkpoints = [i for i, row in enumerate(rows) if row["type"] == "compacted"]
            if checkpoints:
                history = [row["payload"] for row in rows[checkpoints[-1] + 1:] if row["type"] == "response_item"]
                converted = any(SUMMARY in json.dumps(item) for item in history) and not any("emp1:" in json.dumps(item) for item in history)
                if converted:
                    break
            time.sleep(0.1)
        assert converted, "running EMP must convert the closed history without a repair request"
        assert rollout.read_bytes().startswith(baseline)
        assert baseline.startswith(original)
        report["background_elapsed_seconds"] = round(time.monotonic() - started, 3)
        report["manual_repair_calls"] = 0
        emp.stop()
        emp = None
        with AppServer(codex_bin, env, root / "after.log", limit) as server:
            resume_thread(server, thread_id)
            after = run_turn(server, thread_id, PROBE_PROMPTS["parent"])
        assert after == "completed", f"native resume failed: {after}"
        report.update({"thread_id_preserved": True, "original_prefix_preserved": True,
            "before": before, "after": after, "passed": True})
    finally:
        if emp is not None:
            emp.stop()
        provider.stop()
        (root / "report.json").write_text(json.dumps(report, indent=2) + "\n")
    print(json.dumps(report))


if __name__ == "__main__":
    main()
