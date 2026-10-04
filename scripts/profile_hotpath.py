#!/usr/bin/env python3
"""Run the opt-in Rust workload with disposable homes and local JSON reports."""
import argparse
import json
import os
from pathlib import Path
import subprocess
import tempfile


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("test_binary", type=Path)
    parser.add_argument("output", type=Path, help="New report directory")
    parser.add_argument("--repeats", type=int, default=3)
    args = parser.parse_args()
    if not 1 <= args.repeats <= 10:
        parser.error("--repeats must be between 1 and 10")
    binary = args.test_binary.resolve(strict=True)
    output = args.output.resolve()
    output.mkdir(parents=True, exist_ok=False)
    for run in range(args.repeats):
        with tempfile.TemporaryDirectory(prefix="emp-hotpath-") as home:
            env = {key: value for key, value in os.environ.items()
                   if not key.startswith(("EMP_", "HOTPATH_"))}
            env.update(HOME=home, USERPROFILE=home, CODEX_HOME=f"{home}/codex",
                       XDG_CONFIG_HOME=f"{home}/config", XDG_DATA_HOME=f"{home}/data",
                       NO_PROXY="*", no_proxy="*", RUST_TEST_THREADS="1",
                       HOTPATH_METRICS_SERVER_OFF="true",
                       HOTPATH_OUTPUT_FORMAT="json",
                       HOTPATH_OUTPUT_PATH=str(output / f"hotpath-{run}.json"))
            result = subprocess.run(
                [str(binary), "tests::hotpath_profile::isolated_hotpath_workload",
                 "--exact", "--ignored", "--nocapture", "--test-threads=1"],
                env=env, capture_output=True, text=True, timeout=120,
            )
            (output / f"run-{run}.log").write_text(result.stdout + result.stderr)
            if result.returncode:
                raise SystemExit(f"Workload failed ({result.returncode}): {output / f'run-{run}.log'}")
            measurements = [json.loads(line.split("PROFILE ", 1)[1])
                            for line in result.stdout.splitlines() if "PROFILE " in line]
            if len(measurements) != 7 or not (output / f"hotpath-{run}.json").is_file():
                raise SystemExit("Workload did not produce all seven measurements and a hotpath report")
            (output / f"timing-{run}.json").write_text(json.dumps(measurements, indent=2) + "\n")
            print(json.dumps({"run": run, "measurements": measurements}), flush=True)


if __name__ == "__main__":
    main()
