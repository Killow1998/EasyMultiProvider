"""Portable, stop-only synchronization with the Codex runtime.

EMP never owns the Codex process lifecycle. After a user-confirmed config or
catalog mutation it asks Codex Remote Control to stop gracefully, then only
observes whether an external owner starts the App Server again. EMP never
starts, restarts, kills, or replaces that process.
"""

from __future__ import annotations

import json
import os
import shutil
import subprocess
import tempfile
import threading
import time
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Iterable, Mapping, Optional, Protocol, Sequence, Tuple, Union

from . import __version__
from .codex_compatibility import (
    CodexCompatibility,
    classify_codex_version,
    unavailable_compatibility,
)
from .integration import atomic_write_text


NOT_CHECKED = "not_checked"
RELOAD_REQUIRED = "reload_required"
STOPPING = "stopping"
EMP_LOADED = "emp_loaded"
NATIVE_LOADED = "native_loaded"
STOPPED_WAITING_FOR_START = "stopped_waiting_for_start"
STOP_FAILED = "stop_failed"
VERIFICATION_FAILED = "verification_failed"
UNSUPPORTED = "unsupported"

RUNTIME_STATE_SCHEMA = "easy-multi-provider.runtime-recovery"
RUNTIME_STATE_VERSION = 1
_RUNTIME_STATES = {
    NOT_CHECKED,
    RELOAD_REQUIRED,
    STOPPING,
    EMP_LOADED,
    NATIVE_LOADED,
    STOPPED_WAITING_FOR_START,
    STOP_FAILED,
    VERIFICATION_FAILED,
    UNSUPPORTED,
}
_MAX_EXPECTED_MODELS = 2000
_MAX_COMMAND_STDOUT_BYTES = 2 * 1024 * 1024
_MAX_COMMAND_STDERR_BYTES = 32 * 1024
_COMPATIBILITY_CACHE_SECONDS = 60.0


@dataclass(frozen=True)
class CommandResult:
    returncode: int
    stdout: str = ""
    stderr: str = ""


class CommandRunner(Protocol):
    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str = "",
        timeout: float = 0,
    ) -> CommandResult:
        """Run one bounded command without a shell."""


class SubprocessRunner:
    def run(
        self,
        args: Sequence[str],
        *,
        input_text: str = "",
        timeout: float = 0,
    ) -> CommandResult:
        with tempfile.TemporaryFile(mode="w+b") as stdout_sink, tempfile.TemporaryFile(
            mode="w+b"
        ) as stderr_sink:
            try:
                completed = subprocess.run(
                    list(args),
                    input=input_text,
                    text=True,
                    encoding="utf-8",
                    errors="replace",
                    stdout=stdout_sink,
                    stderr=stderr_sink,
                    timeout=timeout or 15,
                    check=False,
                )
            except FileNotFoundError:
                return CommandResult(127, "", "command unavailable")
            except subprocess.TimeoutExpired:
                return CommandResult(124, "", "command timed out")
            stdout_sink.seek(0)
            stderr_sink.seek(0)
            stdout = stdout_sink.read(_MAX_COMMAND_STDOUT_BYTES).decode(
                "utf-8", "replace"
            )
            stderr = stderr_sink.read(_MAX_COMMAND_STDERR_BYTES).decode(
                "utf-8", "replace"
            )
        return CommandResult(completed.returncode, stdout, stderr)


@dataclass(frozen=True)
class RuntimeSyncResult:
    state: str
    target: str
    verified: bool = False
    detail: str = ""
    observed_models: Tuple[str, ...] = ()


class RuntimeSyncError(RuntimeError):
    """A bounded read-only runtime observation failed."""

    def __init__(self, message: str, kind: str = "malformed") -> None:
        super().__init__(message)
        self.kind = kind


@dataclass(frozen=True)
class HostStopResult:
    status: str
    stopped_count: int = 0
    detail: str = ""


class TargetedHostStopper(Protocol):
    def stop_stale_codex_hosts(self) -> HostStopResult:
        """Gracefully stop only verified same-user Codex background hosts."""


@dataclass(frozen=True)
class ProcessIdentity:
    pid: int
    parent_pid: int
    username: str
    create_time: float
    executable: str
    argv: Tuple[str, ...]
    effective_codex_home: str


@dataclass(frozen=True)
class _ProcessMetadata:
    pid: int
    parent_pid: int
    username: str
    create_time: float
    executable: str
    argv: Tuple[str, ...]


class ProcessInventory(Protocol):
    current_username: str

    def list_processes(self) -> Sequence[ProcessIdentity]:
        """Return a bounded identity snapshot."""

    def terminate(self, expected: ProcessIdentity, timeout: float) -> str:
        """Revalidate the exact identity, terminate, and wait."""


class _IneligibleProcess(RuntimeError):
    """A process cannot be proved to belong to the active integration home."""


class PsutilProcessInventory:
    """Lazy psutil adapter used only during a confirmed targeted fallback."""

    def __init__(
        self,
        target_codex_home: Path,
        pids: Optional[Sequence[int]] = None,
        default_codex_home: Optional[Path] = None,
    ) -> None:
        import psutil

        self._psutil = psutil
        self.target_codex_home = _normalize_codex_home(target_codex_home)
        self.default_codex_home = _normalize_codex_home(
            default_codex_home or (Path.home() / ".codex")
        )
        self._pids = (
            tuple(dict.fromkeys(int(pid) for pid in pids)) if pids is not None else None
        )
        self.current_username = psutil.Process().username()

    def _effective_codex_home(self, process: Any) -> str:
        environment = process.environ()
        if not isinstance(environment, dict):
            raise _IneligibleProcess("process environment is ambiguous")
        missing = object()
        try:
            configured = environment.get("CODEX_HOME", missing)
        finally:
            environment.clear()
            del environment
        if configured is missing:
            return self.default_codex_home
        if not isinstance(configured, str) or not configured:
            raise _IneligibleProcess("process Codex home is ambiguous")
        try:
            return _normalize_codex_home(configured)
        except ValueError as exc:
            raise _IneligibleProcess("process Codex home is ambiguous") from exc

    def _metadata(self, process: Any) -> _ProcessMetadata:
        username = str(process.username())
        if username != self.current_username:
            raise _IneligibleProcess("process owner does not match")
        return _ProcessMetadata(
            int(process.pid),
            int(process.ppid()),
            username,
            float(process.create_time()),
            str(process.exe()),
            tuple(str(part) for part in process.cmdline()),
        )

    def _snapshot(self, process: Any) -> ProcessIdentity:
        metadata = self._metadata(process)
        if _classify_semantic_host(metadata) is None:
            raise _IneligibleProcess("process is not a supported Codex background host")
        effective_codex_home = self._effective_codex_home(process)
        if effective_codex_home != self.target_codex_home:
            raise _IneligibleProcess("process Codex home does not match")
        return ProcessIdentity(
            metadata.pid,
            metadata.parent_pid,
            metadata.username,
            metadata.create_time,
            metadata.executable,
            metadata.argv,
            effective_codex_home,
        )

    def list_processes(self) -> Sequence[ProcessIdentity]:
        identities = []
        if self._pids is None:
            processes = self._psutil.process_iter()
        else:
            processes = (self._psutil.Process(pid) for pid in self._pids)
        for process in processes:
            try:
                identities.append(self._snapshot(process))
            except (
                _IneligibleProcess,
                self._psutil.NoSuchProcess,
                self._psutil.AccessDenied,
                self._psutil.ZombieProcess,
                OSError,
            ):
                continue
            if len(identities) >= 4096:
                break
        return tuple(identities)

    def terminate(self, expected: ProcessIdentity, timeout: float) -> str:
        try:
            process = self._psutil.Process(expected.pid)
            if self._snapshot(process) != expected:
                return "raced"
            process.terminate()
            process.wait(timeout=max(0.001, timeout))
            return "stopped"
        except self._psutil.NoSuchProcess:
            return "gone"
        except _IneligibleProcess:
            return "raced"
        except self._psutil.AccessDenied:
            return "denied"
        except self._psutil.TimeoutExpired:
            return "timeout"
        except (self._psutil.ZombieProcess, OSError, ValueError):
            return "error"


@dataclass(frozen=True)
class _HostCandidate:
    identity: ProcessIdentity
    role: str
    launcher: bool


def _basename(value: str) -> str:
    return value.replace("\\", "/").rsplit("/", 1)[-1].casefold()


def _canonical_official_codex_script(value: str) -> Optional[Path]:
    try:
        script = Path(value).resolve(strict=True)
    except (OSError, RuntimeError, ValueError):
        return None
    parts = tuple(part.casefold() for part in script.parts)
    if len(parts) < 4 or parts[-4:] != ("@openai", "codex", "bin", "codex.js"):
        return None
    return script


def _official_codex_invocation(
    identity: Union[ProcessIdentity, _ProcessMetadata],
) -> Optional[Tuple[Tuple[str, ...], bool]]:
    if not identity.executable or not identity.argv:
        return None
    executable = _basename(identity.executable)
    if executable in ("codex", "codex.exe"):
        return tuple(identity.argv[1:]), False
    if executable not in ("node", "node.exe") or len(identity.argv) < 2:
        return None
    if _canonical_official_codex_script(identity.argv[1]) is None:
        return None
    return tuple(identity.argv[2:]), True


_ROOT_OPTIONS_WITH_VALUE = frozenset(("-c", "--config", "--enable", "--disable"))
_ROOT_OPTIONS_WITH_EQUALS = ("--config=", "--enable=", "--disable=")
_SEMANTIC_COMMANDS = frozenset(("remote-control", "app-server"))


def _parse_codex_root(arguments: Sequence[str]) -> Optional[Tuple[str, Tuple[str, ...]]]:
    index = 0
    while index < len(arguments):
        token = arguments[index]
        if token in _SEMANTIC_COMMANDS:
            return token, tuple(arguments[index + 1 :])
        if token in _ROOT_OPTIONS_WITH_VALUE:
            if index + 1 >= len(arguments):
                return None
            value = arguments[index + 1]
            if not value or value.startswith("-") or value in _SEMANTIC_COMMANDS:
                return None
            index += 2
            continue
        equals_prefix = next(
            (prefix for prefix in _ROOT_OPTIONS_WITH_EQUALS if token.startswith(prefix)),
            None,
        )
        if equals_prefix is not None:
            if len(token) == len(equals_prefix):
                return None
            index += 1
            continue
        return None
    return None


def _normalize_codex_home(path: Union[Path, str]) -> str:
    value = os.fspath(path)
    if not value or not os.path.isabs(value):
        raise ValueError("Codex home must be an absolute path")
    return os.path.normcase(os.path.realpath(os.path.abspath(value)))


def _classify_semantic_host(
    identity: Union[ProcessIdentity, _ProcessMetadata],
) -> Optional[Tuple[str, bool]]:
    invocation = _official_codex_invocation(identity)
    if invocation is None:
        return None
    arguments, launcher = invocation
    parsed = _parse_codex_root(arguments)
    if parsed is None:
        return None
    command, remainder = parsed
    if command == "remote-control":
        if any(token in ("start", "stop", "pair") for token in remainder):
            return None
        return "remote_control", launcher
    if command != "app-server":
        return None
    if any(
        token in ("proxy", "daemon")
        or "schema" in token.casefold()
        or token.casefold().startswith("generate-")
        for token in remainder
    ):
        return None
    if not any(token == "--listen" or token.startswith("--listen=") for token in remainder):
        return None
    return "app_server_listener", launcher


def _host_candidate(
    identity: ProcessIdentity,
    current_username: str,
    target_codex_home: str,
) -> Optional[_HostCandidate]:
    if (
        identity.username != current_username
        or identity.effective_codex_home != target_codex_home
    ):
        return None
    classification = _classify_semantic_host(identity)
    if classification is None:
        return None
    role, launcher = classification
    return _HostCandidate(identity, role, launcher)


class TargetedCodexHostStopper:
    """Gracefully terminate only verified same-user Codex background hosts."""

    def __init__(
        self,
        target_codex_home: Path,
        process_inventory: Optional[ProcessInventory] = None,
        termination_timeout: float = 5.0,
    ) -> None:
        self.target_codex_home = _normalize_codex_home(target_codex_home)
        self.process_inventory = process_inventory
        self.termination_timeout = min(10.0, max(0.01, termination_timeout))

    def stop_stale_codex_hosts(self) -> HostStopResult:
        inventory = self.process_inventory or PsutilProcessInventory(
            self.target_codex_home
        )
        candidates = [
            candidate
            for identity in inventory.list_processes()
            for candidate in (
                _host_candidate(
                    identity,
                    inventory.current_username,
                    self.target_codex_home,
                ),
            )
            if candidate is not None
        ]
        if not candidates:
            return HostStopResult(
                "none", 0, ""
            )
        by_pid = {candidate.identity.pid: candidate for candidate in candidates}

        def depth(candidate: _HostCandidate) -> int:
            value = 0
            parent = candidate.identity.parent_pid
            seen = set()
            while parent in by_pid and parent not in seen:
                seen.add(parent)
                value += 1
                parent = by_pid[parent].identity.parent_pid
            return value

        candidates.sort(
            key=lambda candidate: (depth(candidate), not candidate.launcher),
            reverse=True,
        )
        deadline = time.monotonic() + self.termination_timeout
        stopped_count = 0
        for candidate in candidates:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                return HostStopResult(
                    "failed", stopped_count, "Codex background host did not stop in time"
                )
            outcome = inventory.terminate(candidate.identity, remaining)
            if outcome == "stopped":
                stopped_count += 1
                continue
            if outcome == "gone":
                continue
            detail = {
                "raced": "Codex background host identity changed before termination",
                "denied": "Codex background host termination was denied",
                "timeout": "Codex background host did not stop in time",
            }.get(outcome, "Codex background host could not be stopped safely")
            return HostStopResult("failed", stopped_count, detail)
        return HostStopResult("stopped", stopped_count, "")


@dataclass(frozen=True)
class RuntimeRecoveryRecord:
    state: str
    target: str
    configuration_relation: str
    expected_models: Tuple[str, ...]
    verified: bool
    detail: str
    updated_at: str

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": RUNTIME_STATE_SCHEMA,
            "version": RUNTIME_STATE_VERSION,
            "state": self.state,
            "target": self.target,
            "configuration_relation": self.configuration_relation,
            "expected_models": list(self.expected_models),
            "verified": self.verified,
            "detail": self.detail,
            "updated_at": self.updated_at,
        }


class RuntimeRecoveryStore:
    """Persist bounded accounting only; never request or response content."""

    def __init__(self, path: Path) -> None:
        self.path = Path(path)

    def load(self) -> Optional[RuntimeRecoveryRecord]:
        if not self.path.exists():
            return None
        try:
            raw = json.loads(self.path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise RuntimeSyncError("runtime recovery record is unreadable") from exc
        required = {
            "schema",
            "version",
            "state",
            "target",
            "configuration_relation",
            "expected_models",
            "verified",
            "detail",
            "updated_at",
        }
        if not isinstance(raw, dict) or set(raw) != required:
            raise RuntimeSyncError("runtime recovery record is invalid")
        models = raw.get("expected_models")
        valid_models = (
            isinstance(models, list)
            and len(models) <= _MAX_EXPECTED_MODELS
            and all(isinstance(item, str) and 0 < len(item) <= 512 for item in models)
        )
        if (
            raw.get("schema") != RUNTIME_STATE_SCHEMA
            or raw.get("version") != RUNTIME_STATE_VERSION
            or raw.get("state") not in _RUNTIME_STATES
            or raw.get("target") not in ("emp", "native")
            or raw.get("configuration_relation")
            not in ("unleased", "original", "applied", "mixed", "other")
            or not valid_models
            or not isinstance(raw.get("verified"), bool)
            or not isinstance(raw.get("detail"), str)
            or len(raw.get("detail", "")) > 1024
            or not isinstance(raw.get("updated_at"), str)
        ):
            raise RuntimeSyncError("runtime recovery record is invalid")
        return RuntimeRecoveryRecord(
            raw["state"],
            raw["target"],
            raw["configuration_relation"],
            tuple(dict.fromkeys(models)),
            raw["verified"],
            raw["detail"],
            raw["updated_at"],
        )

    def save(
        self,
        state: str,
        target: str,
        configuration_relation: str,
        expected_models: Sequence[str],
        verified: bool,
        detail: str,
    ) -> RuntimeRecoveryRecord:
        models = tuple(
            dict.fromkeys(
                model
                for model in expected_models
                if isinstance(model, str) and 0 < len(model) <= 512
            )
        )
        if len(models) > _MAX_EXPECTED_MODELS:
            raise RuntimeSyncError("too many expected models for runtime recovery")
        if (
            state not in _RUNTIME_STATES
            or target not in ("emp", "native")
            or configuration_relation
            not in ("unleased", "original", "applied", "mixed", "other")
        ):
            raise RuntimeSyncError("runtime recovery state is invalid")
        record = RuntimeRecoveryRecord(
            state,
            target,
            configuration_relation,
            models,
            bool(verified),
            str(detail)[:1024],
            datetime.now(timezone.utc).isoformat(),
        )
        self.path.parent.mkdir(parents=True, exist_ok=True)
        self.path.parent.chmod(0o700)
        atomic_write_text(
            self.path,
            json.dumps(record.to_dict(), indent=2, sort_keys=True) + "\n",
            mode=0o600,
        )
        return record


def offline_runtime_snapshot(
    record: Optional[RuntimeRecoveryRecord], *, confidence: str = "offline"
) -> Dict[str, Any]:
    """Project durable accounting without pretending it is a live probe."""

    if record is None:
        return {
            "state": NOT_CHECKED,
            "target": "native",
            "verified": False,
            "confidence": confidence,
            "detail": "Codex runtime has not been checked",
            "last_known": None,
        }
    pending = record.state in (RELOAD_REQUIRED, STOPPING)
    detail = (
        "A previously requested runtime reload still requires confirmation"
        if record.state == RELOAD_REQUIRED
        else "A previous runtime operation was interrupted and must be retried"
        if record.state == STOPPING
        else "Last-known runtime status is stale; no live check was performed"
    )
    return {
        "state": RELOAD_REQUIRED if pending else NOT_CHECKED,
        "target": record.target,
        "verified": False,
        "confidence": confidence,
        "detail": detail,
        "last_known": {
            "state": record.state,
            "target": record.target,
            "verified": record.verified,
            "observed_at": record.updated_at,
        },
    }


def _safe_error_detail(result: CommandResult, fallback: str) -> str:
    lowered = (result.stdout + " " + result.stderr).lower()
    if "timed out" in lowered:
        return "Codex runtime control timed out"
    if "not found" in lowered or "unavailable" in lowered:
        return "Codex CLI is unavailable"
    return fallback


def _is_documented_unmanaged_host_error(result: CommandResult) -> bool:
    message = (result.stdout + " " + result.stderr).casefold()
    return "app server is running but is not managed" in message


class CodexRuntimeController:
    """Issue one graceful stop and perform bounded, read-only observation."""

    def __init__(
        self,
        runner: Optional[CommandRunner] = None,
        codex_executable: Optional[str] = None,
        control_timeout: float = 15.0,
        observation_timeout: float = 20.0,
        poll_interval: float = 0.25,
        host_stopper: Optional[TargetedHostStopper] = None,
        target_codex_home: Optional[Path] = None,
    ) -> None:
        self.runner = runner or SubprocessRunner()
        self.codex_executable = codex_executable or shutil.which("codex") or "codex"
        self.control_timeout = max(0.01, float(control_timeout))
        self.observation_timeout = min(20.0, max(0.0, float(observation_timeout)))
        self.poll_interval = max(0.001, float(poll_interval))
        self.host_stopper = host_stopper
        self._compatibility_lock = threading.Lock()
        self._compatibility_checked_at = 0.0
        self._compatibility: Optional[CodexCompatibility] = None
        self.target_codex_home = (
            _normalize_codex_home(target_codex_home)
            if target_codex_home is not None
            else None
        )

    def set_target_codex_home(self, target_codex_home: Path) -> None:
        """Bind fallback process selection to the active integration manager."""

        self.target_codex_home = _normalize_codex_home(target_codex_home)

    def _targeted_host_stopper(self) -> Optional[TargetedHostStopper]:
        if self.host_stopper is not None:
            return self.host_stopper
        if self.target_codex_home is None:
            return None
        return TargetedCodexHostStopper(Path(self.target_codex_home))

    def _command(self, *parts: str) -> Tuple[str, ...]:
        return (self.codex_executable, *parts)

    def compatibility(self) -> Dict[str, Any]:
        """Return one bounded, short-lived observation of `codex --version`."""
        now = time.monotonic()
        with self._compatibility_lock:
            if (
                self._compatibility is not None
                and now - self._compatibility_checked_at
                < _COMPATIBILITY_CACHE_SECONDS
            ):
                return self._compatibility.public()

        result = self.runner.run(
            self._command("--version"),
            timeout=min(self.control_timeout, 5.0),
        )
        if result.returncode == 0:
            compatibility = classify_codex_version(result.stdout)
        elif result.returncode == 127 or "not found" in (
            result.stdout + " " + result.stderr
        ).casefold():
            compatibility = unavailable_compatibility()
        else:
            compatibility = CodexCompatibility(None, "unknown")
        with self._compatibility_lock:
            self._compatibility = compatibility
            self._compatibility_checked_at = time.monotonic()
        return compatibility.public()

    def _stop_current_app_server(self, target: str) -> Optional[RuntimeSyncResult]:
        result = self.runner.run(
            self._command("remote-control", "stop", "--json"),
            timeout=self.control_timeout,
        )
        if result.returncode != 0:
            if _is_documented_unmanaged_host_error(result):
                stopper = self._targeted_host_stopper()
                if stopper is None:
                    return RuntimeSyncResult(
                        UNSUPPORTED,
                        target,
                        False,
                        "The active Codex integration home is unavailable",
                    )
                fallback = stopper.stop_stale_codex_hosts()
                if fallback.status == "stopped" and fallback.stopped_count > 0:
                    return None
                if fallback.status == "none":
                    return RuntimeSyncResult(
                        UNSUPPORTED,
                        target,
                        False,
                        "No safely identifiable Codex background host",
                    )
                if fallback.status == "failed":
                    return RuntimeSyncResult(
                        STOP_FAILED,
                        target,
                        False,
                        fallback.detail or "Codex background host did not stop",
                    )
                return RuntimeSyncResult(
                    UNSUPPORTED,
                    target,
                    False,
                    fallback.detail or "No safely identifiable Codex background host",
                )
            lowered = (result.stdout + " " + result.stderr).lower()
            state = UNSUPPORTED if "unknown command" in lowered else STOP_FAILED
            return RuntimeSyncResult(
                state,
                target,
                False,
                _safe_error_detail(result, "Codex could not be stopped gracefully"),
            )
        try:
            payload = json.loads(result.stdout)
        except (TypeError, ValueError):
            return RuntimeSyncResult(
                UNSUPPORTED,
                target,
                False,
                "Codex returned an unsupported graceful-stop response",
            )
        if not isinstance(payload, Mapping):
            return RuntimeSyncResult(
                UNSUPPORTED,
                target,
                False,
                "Codex returned an unsupported graceful-stop response",
            )
        status = payload.get("status")
        if status not in ("notRunning", "stopped"):
            return RuntimeSyncResult(
                UNSUPPORTED,
                target,
                False,
                "Codex returned an unsupported graceful-stop response",
            )
        stopper = self._targeted_host_stopper()
        if stopper is None:
            return RuntimeSyncResult(
                UNSUPPORTED,
                target,
                False,
                "The active Codex integration home is unavailable",
            )
        residual = stopper.stop_stale_codex_hosts()
        if residual.status == "failed":
            return RuntimeSyncResult(
                STOP_FAILED,
                target,
                False,
                residual.detail or "Codex background host did not stop",
            )
        if residual.status not in ("none", "stopped"):
            return RuntimeSyncResult(
                UNSUPPORTED,
                target,
                False,
                residual.detail or "Codex background host state is unsupported",
            )
        if status == "notRunning" and residual.status == "none":
            return RuntimeSyncResult(
                STOPPED_WAITING_FOR_START,
                target,
                False,
                "No Codex runtime was running; the target will load on next start",
            )
        return None

    def _model_list(self) -> Tuple[str, ...]:
        models = []
        cursor: Optional[str] = None
        seen_cursors = set()
        for _page in range(20):
            params: Dict[str, Any] = {"includeHidden": False, "limit": 1000}
            if cursor is not None:
                params["cursor"] = cursor
            request_lines = (
                {
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {
                            "name": "easy-multi-provider",
                            "version": __version__,
                        },
                        "capabilities": {"experimentalApi": True},
                    },
                },
                {"method": "initialized", "params": {}},
                {"id": 2, "method": "model/list", "params": params},
            )
            input_text = "".join(
                json.dumps(item, separators=(",", ":")) + "\n"
                for item in request_lines
            )
            result = self.runner.run(
                self._command("app-server", "proxy"),
                input_text=input_text,
                timeout=self.control_timeout,
            )
            if result.returncode != 0:
                lowered = (result.stdout + " " + result.stderr).lower()
                if any(
                    marker in lowered
                    for marker in ("connection refused", "not running", "no app server")
                ):
                    raise RuntimeSyncError(
                        "Codex App Server is not available", "unavailable"
                    )
                if "permission denied" in lowered or "forbidden" in lowered:
                    raise RuntimeSyncError(
                        "Codex model catalog access was denied", "permission"
                    )
                if "unknown command" in lowered or result.returncode == 127:
                    raise RuntimeSyncError(
                        "Codex model-list control is unsupported", "unsupported"
                    )
                raise RuntimeSyncError("Codex model catalog query failed", "command")
            response: Optional[Mapping[str, Any]] = None
            for line in result.stdout.splitlines():
                try:
                    message = json.loads(line)
                except (TypeError, ValueError):
                    continue
                if isinstance(message, Mapping) and message.get("id") == 2:
                    response = message
                    break
            if response is None or response.get("error") is not None:
                raise RuntimeSyncError(
                    "Codex did not return a usable model catalog", "malformed"
                )
            payload = response.get("result")
            data = payload.get("data") if isinstance(payload, Mapping) else None
            if not isinstance(data, list):
                raise RuntimeSyncError(
                    "Codex returned an invalid model catalog", "malformed"
                )
            models.extend(
                item["id"]
                for item in data
                if isinstance(item, Mapping) and isinstance(item.get("id"), str)
            )
            if len(models) > 20_000:
                raise RuntimeSyncError("Codex model catalog is too large", "malformed")
            next_cursor = payload.get("nextCursor")
            if next_cursor in (None, ""):
                return tuple(dict.fromkeys(models))
            if (
                not isinstance(next_cursor, str)
                or len(next_cursor) > 1024
                or next_cursor in seen_cursors
            ):
                raise RuntimeSyncError("Codex returned an invalid model cursor", "malformed")
            seen_cursors.add(next_cursor)
            cursor = next_cursor
        raise RuntimeSyncError("Codex model catalog pagination exceeded its limit", "malformed")

    @staticmethod
    def _validate_models(
        observed_models: Iterable[str],
        expected_models: Sequence[str],
        target: str,
    ) -> RuntimeSyncResult:
        observed = tuple(dict.fromkeys(observed_models))
        expected = {
            model for model in expected_models if isinstance(model, str) and model
        }
        observed_set = set(observed)
        if target == "emp":
            missing = sorted(expected - observed_set)
            if not expected or missing:
                detail = (
                    "EMP catalog has no expected visible models"
                    if not expected
                    else "Codex is missing %d expected EMP model(s)" % len(missing)
                )
                return RuntimeSyncResult(
                    VERIFICATION_FAILED, target, False, detail, observed
                )
            return RuntimeSyncResult(
                EMP_LOADED, target, True, "Codex loaded the complete EMP catalog", observed
            )
        remaining = sorted(expected.intersection(observed_set))
        if remaining:
            return RuntimeSyncResult(
                VERIFICATION_FAILED,
                target,
                False,
                "Codex still exposes %d EMP model(s)" % len(remaining),
                observed,
            )
        return RuntimeSyncResult(
            NATIVE_LOADED, target, True, "Codex loaded the native catalog", observed
        )

    def reload(
        self,
        expected_models: Sequence[str],
        target: str,
        *,
        confirm_reload: bool,
    ) -> RuntimeSyncResult:
        if target not in ("emp", "native"):
            return RuntimeSyncResult(UNSUPPORTED, target, False, "Unknown runtime target")
        if not confirm_reload:
            return RuntimeSyncResult(
                RELOAD_REQUIRED,
                target,
                False,
                "Confirmation is required before reconnecting Codex",
            )

        stop_result = self._stop_current_app_server(target)
        if stop_result is not None:
            return stop_result

        deadline = time.monotonic() + self.observation_timeout
        while True:
            try:
                observed = self._model_list()
            except RuntimeSyncError as error:
                if error.kind == "unavailable":
                    observed = None
                else:
                    return RuntimeSyncResult(
                        UNSUPPORTED if error.kind == "unsupported" else VERIFICATION_FAILED,
                        target,
                        False,
                        str(error),
                    )
            if observed is not None:
                return self._validate_models(observed, expected_models, target)
            if time.monotonic() >= deadline:
                return RuntimeSyncResult(
                    STOPPED_WAITING_FOR_START,
                    target,
                    False,
                    "Codex stopped and will load the new configuration when it next starts",
                )
            time.sleep(min(self.poll_interval, max(0.0, deadline - time.monotonic())))

    def observe(
        self,
        expected_models: Sequence[str],
        target: str,
    ) -> RuntimeSyncResult:
        """Verify the live catalog without stopping or mutating Codex."""
        if target not in ("emp", "native"):
            return RuntimeSyncResult(UNSUPPORTED, target, False, "Unknown runtime target")
        try:
            observed = self._model_list()
        except RuntimeSyncError as error:
            if error.kind == "unavailable":
                return RuntimeSyncResult(
                    STOPPED_WAITING_FOR_START,
                    target,
                    False,
                    "Codex is not running; the target will load on next start",
                )
            return RuntimeSyncResult(
                UNSUPPORTED if error.kind == "unsupported" else VERIFICATION_FAILED,
                target,
                False,
                str(error),
            )
        return self._validate_models(observed, expected_models, target)
