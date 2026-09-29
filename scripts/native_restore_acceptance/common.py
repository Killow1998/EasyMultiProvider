from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import threading
import subprocess
from typing import Any


def fail(message: str) -> None:
    raise RuntimeError(message)


def sha256(data: bytes) -> str:
    return hashlib.sha256(data).hexdigest()


def semantic_function_output_retained(
    outputs: Any,
    *,
    call_id: str,
    name: str,
    output_sha256: str,
) -> bool:
    """Require one observed result for this call ID to retain its semantics."""
    if not isinstance(outputs, list) or any(not isinstance(item, dict) for item in outputs):
        return False
    matching = [item for item in outputs if item.get("call_id") == call_id]
    return len(matching) == 1 and (
        matching[0].get("name") == name
        and matching[0].get("output_sha256") == output_sha256
    )


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        while chunk := stream.read(1024 * 1024):
            digest.update(chunk)
    return digest.hexdigest()


class PeakRssSampler:
    """Sample a child process high-water mark without reading its output into memory."""

    def __init__(self, pid: int, interval: float = 0.025) -> None:
        self.pid = pid
        self.interval = interval
        self.peak_bytes: int | None = None
        self._stopped = threading.Event()
        self._thread = threading.Thread(target=self._sample, daemon=True)

    def start(self) -> None:
        self._sample_once()
        self._thread.start()

    def stop(self) -> int | None:
        self._sample_once()
        self._stopped.set()
        if self._thread.is_alive():
            self._thread.join(timeout=1)
        return self.peak_bytes

    def _sample(self) -> None:
        while not self._stopped.wait(self.interval):
            self._sample_once()

    def _sample_once(self) -> None:
        try:
            status = Path(f"/proc/{self.pid}/status").read_text(encoding="ascii")
        except (FileNotFoundError, OSError):
            return
        for line in status.splitlines():
            if line.startswith("VmHWM:"):
                fields = line.split()
                if len(fields) >= 2 and fields[1].isdigit():
                    sample = int(fields[1]) * 1024
                    if sample > 0:
                        self.peak_bytes = max(self.peak_bytes or 0, sample)
                return


def bounded_argv(argv: list[str], address_space_limit_bytes: int) -> list[str]:
    """Use util-linux prlimit to cap the child and its descendants before exec."""
    if address_space_limit_bytes <= 0:
        fail("address-space limit must be positive")
    prlimit = "/usr/bin/prlimit"
    if not os.path.isfile(prlimit) or not os.access(prlimit, os.X_OK):
        fail("/usr/bin/prlimit is required to run acceptance subprocesses safely")
    return [prlimit, f"--as={address_space_limit_bytes}:{address_space_limit_bytes}", "--", *argv]


def run_bounded_capture(
    argv: list[str],
    address_space_limit_bytes: int,
    *,
    env: dict[str, str],
    cwd: Path,
    timeout: float,
) -> tuple[subprocess.CompletedProcess[str], int | None]:
    process = subprocess.Popen(
        bounded_argv(argv, address_space_limit_bytes),
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
        env=env,
        cwd=cwd,
    )
    sampler = PeakRssSampler(process.pid)
    sampler.start()
    try:
        stdout, stderr = process.communicate(timeout=timeout)
    except subprocess.TimeoutExpired:
        process.kill()
        stdout, stderr = process.communicate()
        fail(f"bounded process timed out: {Path(argv[0]).name}")
    peak_rss = sampler.stop()
    return (
        subprocess.CompletedProcess(argv, process.returncode, stdout, stderr),
        peak_rss,
    )


def json_value_sha256(value: Any) -> str:
    encoded = json.dumps(value, ensure_ascii=False, sort_keys=True, separators=(",", ":")).encode("utf-8")
    return sha256(encoded)


def contains_string(value: Any, needle: str) -> bool:
    if isinstance(value, str):
        return needle in value
    if isinstance(value, dict):
        return any(contains_string(key, needle) or contains_string(child, needle) for key, child in value.items())
    if isinstance(value, list):
        return any(contains_string(item, needle) for item in value)
    return False


def confined_path(root: Path, relative: Any, description: str) -> Path:
    if not isinstance(relative, str) or not relative:
        fail(f"history repair manifest has an invalid {description} path")
    resolved_root = root.resolve(strict=True)
    try:
        candidate = (resolved_root / relative).resolve(strict=True)
        candidate.relative_to(resolved_root)
    except (OSError, ValueError):
        fail(f"history repair manifest {description} escapes or is missing under the disposable Codex home")
    if not candidate.is_file():
        fail(f"history repair manifest {description} is not a regular file")
    return candidate
