"""Run official Codex contracts with an empty home and local fixture endpoints."""
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile


def main():
    binary = str(Path(sys.argv[1]).resolve(strict=True))
    root = Path(__file__).resolve().parents[1]
    with tempfile.TemporaryDirectory(prefix="emp-runtime-contract-") as directory:
        home = Path(directory)
        env = dict(os.environ, CODEX_HOME=str(home), OPENAI_API_KEY="fixture-only",
                   CODEX_API_KEY="fixture-only", HTTP_PROXY="http://127.0.0.1:1",
                   HTTPS_PROXY="http://127.0.0.1:1", ALL_PROXY="http://127.0.0.1:1",
                   NO_PROXY="127.0.0.1,localhost,::1")
        # The binary supplies its bundled catalog; no personal cache or account.
        output = subprocess.check_output([binary, "debug", "models"], cwd=home, env=env, timeout=30)
        catalog = home / "catalog.json"
        value = json.loads(output)
        if not isinstance(value, dict) or not value.get("models"):
            raise ValueError("official binary did not return a model catalog")
        catalog.write_bytes(output)
        env.update(EMP_CODEX_TEST_BINARY=binary, EMP_CODEX_TEST_CATALOG=str(catalog),
                   EMP_TEST_CODEX_BIN=binary, EASY_MP_RUN_CODEX_CLI="1")
        suites = ["tests.test_codex_cli_demo", "tests.test_codex_metadata_cli"]
        if env.get("EMP_RUST_BINARY"):
            # Rust uses real processes/sockets, never Python implementation mocks.
            env["EMP_RUST_BINARY"] = str(Path(env["EMP_RUST_BINARY"]).resolve(strict=True))
            suites.append("tests.test_rust_e2e")
        else:
            suites.extend(["tests.test_codex_live_catalog", "tests.test_codex_retry_cli"])
        return subprocess.call([sys.executable, "-m", "unittest", "-v", *suites], cwd=root, env=env)


if __name__ == "__main__":
    sys.exit(main())
