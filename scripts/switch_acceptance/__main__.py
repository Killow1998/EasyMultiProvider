"""Run only with explicit real-model authorization and a new private root."""
import argparse
import json
import os
import socket
import sqlite3
from pathlib import Path

from scripts.native_restore_acceptance.app_server import AppServer
from scripts.native_restore_acceptance.common import sha256_file
from scripts.native_restore_acceptance.emp_server import EmpServer
from .environment import Environment, EXTERNALS, NATIVE
from .environment import digest
from .scenarios import scenario


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--allow-real-model", action="store_true", required=True)
    parser.add_argument("--emp-bin", type=Path, required=True)
    parser.add_argument("--codex-bin", type=Path, required=True)
    parser.add_argument("--root", type=Path, required=True)
    parser.add_argument("--config", type=Path, required=True)
    parser.add_argument("--native-auth", type=Path, required=True)
    parser.add_argument("--external", choices=EXTERNALS, action="append")
    parser.add_argument("--without-subagents", action="store_true")
    args = parser.parse_args()
    os.umask(0o077)
    fixture = Environment(args.root, args.config, args.native_auth)
    report = {"passed": False, "real_models": True, "turns": []}
    output = args.root / "result.json"
    try:
        report["binary_sha256"] = {"emp": sha256_file(args.emp_bin),
                                   "codex": sha256_file(args.codex_bin)}
        with socket.socket() as reservation:
            reservation.bind(("127.0.0.1", 0))
            port = reservation.getsockname()[1]
        with EmpServer(args.emp_bin, fixture.config, args.root, fixture.env,
                       args.root / "emp.stderr", 1536 * 1024 * 1024, port) as emp:
            fixture.configure_codex(emp.base_url)
            with AppServer(args.codex_bin, fixture.env, args.root / "codex.stderr") as client:
                client.rpc("account/login/start", fixture.login)
                del fixture.login
                for external in args.external or EXTERNALS:
                    scenario(client, fixture.project, external, report, output, not args.without_subagents)
        report["passed"] = True
    except Exception as error:
        report["error_type"] = type(error).__name__
        # Errors can echo endpoints or account metadata; keep details private.
        (args.root / "failure.txt").write_text(str(error))
    finally:
        database = args.root / "state/usage.sqlite3"
        if database.exists():
            with sqlite3.connect("file:" + str(database) + "?mode=ro", uri=True) as ledger:
                keys = ("route", "client_model", "upstream_model", "response_model", "state",
                        "success", "error_code", "attempts", "input_tokens", "output_tokens")
                calls = [json.loads(row[0]) for row in ledger.execute("SELECT facts FROM call_records")]
                report["calls"] = [{key: row.get(key) for key in keys} for row in calls
                                   if row.get("route") in ("responses", "agent_task_recovery")]
                def history_replay(row):
                    return (row.get("state") == "recovery_required"
                            and row.get("error_code") == "previous_response_not_found"
                            and row.get("attempts") == 0)
                report["history_replays"] = sum(history_replay(row) for row in report["calls"])
                report["recovery_calls"] = sum(row["route"] == "agent_task_recovery"
                                                for row in report["calls"])
                report["failed_calls"] = sum(row.get("success") is not True and not history_replay(row)
                                             for row in report["calls"])
                if report["failed_calls"]:
                    report["passed"] = False
        expected_models = {NATIVE, *(args.external or EXTERNALS)}
        report["observed_models"] = sorted({row["client_model"]
            for row in report.get("calls", []) if row["success"] is True
            and row["client_model"] in expected_models
            and row["upstream_model"] == row["client_model"].split("/", 1)[1]})
        if set(report["observed_models"]) != expected_models:
            report["passed"] = False
        report["production_inputs_unchanged"] = fixture.unchanged()
        report["source_config_byte_identical"] = digest(args.config) == fixture.source_digest
        if not report["production_inputs_unchanged"]:
            report["passed"] = False
        output.write_text(json.dumps(report, indent=2))
    print(json.dumps({"passed": report["passed"],
                      "production_inputs_unchanged": report["production_inputs_unchanged"],
                      "turns": len(report["turns"])}), flush=True)
    return 0 if report["passed"] else 1


if __name__ == "__main__":
    raise SystemExit(main())
