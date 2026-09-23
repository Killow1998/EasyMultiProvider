"""Run official Codex contracts with an empty home and local fixture endpoints."""
import argparse
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

_CODEX_VERSION = re.compile(r"^codex(?:-cli)?\s+v?(\d+\.\d+\.\d+)(?:\s|$)", re.IGNORECASE)
_SEMVER = re.compile(r"^\d+\.\d+\.\d+$")


def _arguments(argv):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("binary", help="official Codex CLI binary to exercise")
    parser.add_argument(
        "--require-version",
        default=os.environ.get("EMP_CODEX_EXPECTED_VERSION"),
        metavar="VERSION",
        help="fail unless codex --version reports this exact semantic version",
    )
    parser.add_argument(
        "--test",
        dest="tests",
        action="append",
        help="run only this unittest module or fully-qualified test (repeatable)",
    )
    args = parser.parse_args(argv)
    if args.require_version and not _SEMVER.fullmatch(args.require_version):
        parser.error("--require-version must be a semantic version such as 0.156.1")
    return args


def _reported_version(output):
    for line in output.splitlines():
        match = _CODEX_VERSION.match(line.strip())
        if match:
            return match.group(1)
    return None


def _record_result(value):
    print(
        "CODEX_RUNTIME_RESULT=" + json.dumps(value, sort_keys=True),
        file=sys.stderr,
        flush=True,
    )


def main(argv=None):
    args = _arguments(sys.argv[1:] if argv is None else argv)
    binary = str(Path(args.binary).resolve(strict=True))
    root = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="emp-runtime-contract-") as directory:
        home = Path(directory)
        env = dict(os.environ, CODEX_HOME=str(home), OPENAI_API_KEY="fixture-only",
                   CODEX_API_KEY="fixture-only", HTTP_PROXY="http://127.0.0.1:1",
                   HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1",
                   NO_PROXY="127.0.0.1,localhost,::1")
        version_process = subprocess.run(
            [binary, "--version"],
            cwd=home,
            env=env,
            capture_output=True,
            text=True,
            encoding="utf-8",
            errors="replace",
            timeout=30,
            check=False,
        )
        version_output = "\n".join(
            part.strip()
            for part in (version_process.stdout, version_process.stderr)
            if part and part.strip()
        )
        actual_version = _reported_version(version_output)
        print(f"Codex runtime: {version_output or '(no version output)'}", file=sys.stderr)
        if args.require_version and (
            version_process.returncode != 0 or actual_version != args.require_version
        ):
            _record_result({
                "binary": binary,
                "version": actual_version,
                "version_output": version_output,
                "expected_version": args.require_version,
                "status": "version_mismatch",
                "exit_code": 2,
            })
            return 2
        # The binary supplies its bundled catalog; no personal cache or account.
        output = subprocess.check_output(
            [binary, "debug", "models"], cwd=home, env=env, timeout=30
        )
        catalog = home / "catalog.json"
        value = json.loads(output)
        if not isinstance(value, dict) or not value.get("models"):
            raise ValueError("official binary did not return a model catalog")
        catalog.write_bytes(output)
        env.update(EMP_CODEX_TEST_BINARY=binary, EMP_CODEX_TEST_CATALOG=str(catalog),
                   EMP_TEST_CODEX_BIN=binary, EASY_MP_RUN_CODEX_CLI="1")
        if args.tests:
            suites = args.tests
        else:
            suites = ["tests.test_codex_cli_demo", "tests.test_codex_metadata_cli"]
            if env.get("EMP_RUST_BINARY"):
                # Rust uses real processes/sockets, never Python implementation mocks.
                env["EMP_RUST_BINARY"] = str(Path(env["EMP_RUST_BINARY"]).resolve(strict=True))
                suites.append("tests.test_rust_e2e")
            else:
                suites.extend(["tests.test_codex_live_catalog", "tests.test_codex_retry_cli"])
        exit_code = subprocess.call(
            [sys.executable, "-m", "unittest", "-v", *suites], cwd=root, env=env
        )
        _record_result({
            "binary": binary,
            "version": actual_version,
            "version_output": version_output,
            "expected_version": args.require_version,
            "tests": suites,
            "status": "passed" if exit_code == 0 else "failed",
            "exit_code": exit_code,
        })
        return exit_code


if __name__ == "__main__":
    sys.exit(main())
