from __future__ import annotations

import json
import os
import select
import subprocess
import time
from pathlib import Path
from typing import Any

from .common import PeakRssSampler, bounded_argv, fail


class AppServer:
    def __init__(
        self,
        codex_bin: Path,
        env: dict[str, str],
        log_path: Path,
        address_space_limit_bytes: int = 1536 * 1024 * 1024,
    ) -> None:
        self.log_stream = log_path.open("wb")
        try:
            self.process = subprocess.Popen(
                bounded_argv(
                    [str(codex_bin), "app-server", "--stdio"],
                    address_space_limit_bytes,
                ),
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=self.log_stream,
                env=env,
                bufsize=0,
            )
        except BaseException:
            self.log_stream.close()
            raise
        self._closed = False
        self.rss_sampler = PeakRssSampler(self.process.pid)
        self.rss_sampler.start()
        self._peak_rss_bytes: int | None = None
        self._stdout_buffer = bytearray()
        self.next_id = 1
        self.events: list[dict[str, Any]] = []
        try:
            if self.process.stdin is None or self.process.stdout is None:
                fail("unable to open Codex app-server stdio")
            result = self.rpc(
                "initialize",
                {
                    "clientInfo": {
                        "name": "emp-native-restore-acceptance",
                        "title": "EMP native restore acceptance",
                        "version": "1.0.0",
                    },
                    "capabilities": {"experimentalApi": True},
                },
            )
            if not isinstance(result, dict):
                fail("Codex app-server initialization returned an invalid result")
            self.notify("initialized", {})
        except BaseException:
            self.close()
            raise

    def send(self, value: dict[str, Any]) -> None:
        assert self.process.stdin is not None
        self.process.stdin.write(json.dumps(value, separators=(",", ":")).encode("utf-8") + b"\n")
        self.process.stdin.flush()

    def read_message(self, timeout: float) -> dict[str, Any]:
        assert self.process.stdout is not None
        deadline = time.monotonic() + timeout
        while True:
            newline = self._stdout_buffer.find(b"\n")
            if newline >= 0:
                raw = bytes(self._stdout_buffer[:newline])
                del self._stdout_buffer[: newline + 1]
                break
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                if self.process.poll() is not None:
                    fail("Codex app-server exited before replying")
                fail("timed out waiting for Codex app-server")
            ready, _, _ = select.select([self.process.stdout], [], [], remaining)
            if not ready:
                if self.process.poll() is not None:
                    fail("Codex app-server exited before replying")
                fail("timed out waiting for Codex app-server")
            try:
                chunk = os.read(self.process.stdout.fileno(), 65536)
            except OSError as exc:
                fail(f"unable to read Codex app-server output: {exc}")
            if not chunk:
                fail("Codex app-server closed its protocol output")
            self._stdout_buffer.extend(chunk)
            if len(self._stdout_buffer) > 16 * 1024 * 1024:
                fail("Codex app-server protocol line exceeded the acceptance limit")
        try:
            value = json.loads(raw)
        except json.JSONDecodeError:
            fail("Codex app-server emitted invalid JSON-RPC")
        if not isinstance(value, dict):
            fail("Codex app-server emitted a non-object JSON-RPC message")
        return value

    def rpc(self, method: str, params: dict[str, Any], timeout: float = 60) -> Any:
        request_id = self.next_id
        self.next_id += 1
        deadline = time.monotonic() + timeout
        self.send({"id": request_id, "method": method, "params": params})
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                fail(f"timed out waiting for Codex {method} request")
            message = self.read_message(remaining)
            if message.get("id") == request_id:
                if "error" in message:
                    error = message["error"]
                    if isinstance(error, dict):
                        error_message = error.get("message", "")
                        fail(
                            f"Codex {method} request failed: "
                            f"{error.get('code', 'error')} {error_message}".strip()
                        )
                    fail(f"Codex {method} request failed")
                return message.get("result")
            if "method" in message:
                self.events.append(message)

    def notify(self, method: str, params: dict[str, Any]) -> None:
        self.send({"method": method, "params": params})

    def wait_turn(self, thread_id: str, timeout: float = 90) -> str:
        deadline = time.monotonic() + timeout
        while True:
            for index, event in enumerate(self.events):
                params = event.get("params")
                if not isinstance(params, dict):
                    continue
                event_thread = params.get("threadId", params.get("thread_id"))
                turn = params.get("turn")
                if event_thread != thread_id or not isinstance(turn, dict):
                    continue
                method = event.get("method")
                if method == "turn/completed":
                    self.events.pop(index)
                    return str(turn.get("status", "unknown"))
                if method in {"turn/failed", "turn/interrupted"}:
                    self.events.pop(index)
                    return method.split("/", 1)[1]
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return "timeout"
            message = self.read_message(remaining)
            if "method" in message:
                self.events.append(message)
            elif "id" in message:
                self.events.append(message)

    def close(self) -> None:
        if self._closed:
            return
        self._closed = True
        try:
            if self.process.stdin is not None:
                try:
                    self.process.stdin.close()
                except OSError:
                    pass
            try:
                self.process.wait(timeout=10)
            except subprocess.TimeoutExpired:
                try:
                    self.process.terminate()
                except ProcessLookupError:
                    pass
                try:
                    self.process.wait(timeout=5)
                except subprocess.TimeoutExpired:
                    try:
                        self.process.kill()
                    except ProcessLookupError:
                        pass
                    self.process.wait(timeout=5)
        finally:
            self.peak_rss_bytes = self.rss_sampler.stop()
            self.log_stream.close()

    def __enter__(self) -> "AppServer":
        return self

    @property
    def peak_rss_bytes(self) -> int | None:
        samples = [
            value
            for value in (self._peak_rss_bytes, self.rss_sampler.peak_bytes)
            if isinstance(value, int) and value > 0
        ]
        return max(samples) if samples else None

    @peak_rss_bytes.setter
    def peak_rss_bytes(self, value: int | None) -> None:
        self._peak_rss_bytes = value

    def __exit__(self, *_exc: Any) -> None:
        self.close()
