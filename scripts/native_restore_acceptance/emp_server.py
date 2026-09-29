from __future__ import annotations

import http.client
from pathlib import Path
import queue
import re
import signal
import subprocess
import threading
import time

from .common import PeakRssSampler, bounded_argv, fail


class EmpServer:
    def __init__(
        self,
        emp_bin: Path,
        config_path: Path,
        root: Path,
        env: dict[str, str],
        log_path: Path,
        address_space_limit_bytes: int,
        port: int,
    ) -> None:
        self.log_stream = log_path.open("wb")
        command = bounded_argv(
            [
                str(emp_bin),
                "serve",
                "--config",
                str(config_path),
                "--host",
                "127.0.0.1",
                "--port",
                str(port),
            ],
            address_space_limit_bytes,
        )
        emp_env = {
            key: value
            for key, value in env.items()
            if key not in {"EASY_MULTI_PROVIDER_CONFIG", "EASY_MULTI_PROVIDER_MASTER_KEY_FILE", "BROWSER"}
        }
        self.process = subprocess.Popen(
            command,
            cwd=root,
            env=emp_env,
            stdin=subprocess.DEVNULL,
            stdout=subprocess.PIPE,
            stderr=self.log_stream,
            bufsize=0,
        )
        self.rss_sampler = PeakRssSampler(self.process.pid)
        self.rss_sampler.start()
        self.peak_rss_bytes: int | None = None
        self._lines: queue.Queue[str] = queue.Queue()
        self._reader = threading.Thread(target=self._read_stdout, daemon=True)
        self._reader.start()
        self.output_lines: list[str] = []
        self.base_url = ""
        self.bootstrap_url = ""
        self._closed = False
        try:
            self._wait_ready()
        except BaseException:
            self.stop()
            raise

    def _read_stdout(self) -> None:
        assert self.process.stdout is not None
        for raw in self.process.stdout:
            self._lines.put(raw.decode("utf-8", errors="replace").rstrip("\r\n"))

    def _wait_ready(self) -> None:
        deadline = time.monotonic() + 30
        while time.monotonic() < deadline:
            if self.process.poll() is not None:
                fail(f"EMP exited during startup with status {self.process.returncode}")
            remaining = max(0.01, deadline - time.monotonic())
            try:
                line = self._lines.get(timeout=min(remaining, 1.0))
            except queue.Empty:
                continue
            self.output_lines.append(line)
            match = re.fullmatch(r"Open in browser: (http://127\.0\.0\.1:(\d+)/\?bootstrap=.*)", line)
            if match:
                self.bootstrap_url = match.group(1)
                self.base_url = f"http://127.0.0.1:{match.group(2)}"
                break
        if not self.base_url:
            fail(f"EMP did not announce its disposable loopback listener: {self.output_lines!r}")
        self._check_health()

    def _check_health(self) -> None:
        match = re.search(r":(\d+)$", self.base_url)
        if not match:
            fail("EMP listener did not have a valid loopback port")
        connection = http.client.HTTPConnection("127.0.0.1", int(match.group(1)), timeout=3)
        try:
            connection.request("GET", "/healthz")
            response = connection.getresponse()
            response.read()
            if response.status != 200:
                fail(f"disposable EMP health check returned HTTP {response.status}")
        finally:
            connection.close()

    @property
    def pid(self) -> int:
        return self.process.pid

    @property
    def api_base_url(self) -> str:
        return self.base_url + "/v1"

    def stop(self) -> None:
        if self._closed:
            return
        self._closed = True
        if self.process.poll() is None:
            self.process.send_signal(signal.SIGTERM)
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                self.process.kill()
                self.process.wait(timeout=5)
        self.peak_rss_bytes = self.rss_sampler.stop()
        if self.process.stdout is not None:
            self.process.stdout.close()
        self._reader.join(timeout=2)
        self.log_stream.close()

    def __enter__(self) -> "EmpServer":
        return self

    def __exit__(self, *_exc: object) -> None:
        self.stop()
