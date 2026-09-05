"""Read-only observation of a separately owned Codex runtime.

EMP may update its integration files, but it never controls the shared Codex
App Server lifecycle. Runtime synchronization means observing the catalog
already loaded by the backend owner; a stale catalog remains pending until
that owner restarts Codex in a safe maintenance window.
"""

from __future__ import annotations

import errno
import json
import os
import platform
import shutil
import socket
import subprocess
import tempfile
import threading
import time
from concurrent.futures import ThreadPoolExecutor
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Iterable, Mapping, Optional, Protocol, Sequence, Tuple, Union

import websocket

from . import __version__
from .codex_compatibility import (
    CodexCompatibility,
    classify_codex_version,
    unavailable_compatibility,
)
from .integration import atomic_write_text


NOT_CHECKED = "not_checked"
CATALOG_UNVERIFIED = "catalog_unverified"
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
    CATALOG_UNVERIFIED,
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
_APP_SERVER_CONTROL_SOCKET_DIR = "app-server-control"
_APP_SERVER_CONTROL_SOCKET_NAME = "app-server-control.sock"
_WINDOWS_APP_RUNTIME_PARTS = ("OpenAI", "Codex", "bin")
_MAX_WINDOWS_APP_RUNTIMES = 64
_MAX_EDITOR_EXTENSIONS = 64
_SELECTABLE_COMPATIBILITY = frozenset(("recommended", "supported", "unverified"))
_COMPATIBILITY_PRIORITY = {"recommended": 0, "supported": 1, "unverified": 2}
_RUNTIME_SOURCES = frozenset(
    (
        "auto",
        "configured",
        "codex_app",
        "managed",
        "vscode",
        "vscode_insiders",
        "cursor",
        "path_cli",
    )
)
_MAX_CONTROL_MESSAGES_PER_RESPONSE = 100
_MAX_CONTROL_MESSAGE_BYTES = 2 * 1024 * 1024


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
class CodexRuntimeCandidate:
    source: str
    name: str
    executable: str


def _normalize_runtime_preferences(
    sources: Optional[Sequence[str]],
) -> Tuple[str, ...]:
    if sources is None:
        return ("auto",)
    if isinstance(sources, (str, bytes)):
        raise ValueError("Codex runtime sources must be a sequence")
    normalized = tuple(dict.fromkeys(sources))
    if not normalized or any(source not in _RUNTIME_SOURCES for source in normalized):
        raise ValueError("unknown Codex runtime source")
    if "auto" in normalized and len(normalized) != 1:
        raise ValueError("automatic Codex runtime selection cannot be combined")
    return normalized


class ModelCatalogProbe(Protocol):
    def model_list(self, codex_home: Path, timeout: float) -> Tuple[str, ...]:
        """Read the catalog loaded by an existing shared Codex backend."""


class UnixSocketModelCatalogProbe:
    """Read ``model/list`` over Codex's Unix WebSocket control socket."""

    @staticmethod
    def _socket_path(codex_home: Path) -> Path:
        return (
            codex_home
            / _APP_SERVER_CONTROL_SOCKET_DIR
            / _APP_SERVER_CONTROL_SOCKET_NAME
        )

    @staticmethod
    def _send(websocket_connection: Any, value: Mapping[str, Any]) -> None:
        websocket_connection.send(
            json.dumps(value, ensure_ascii=False, separators=(",", ":"))
        )

    @staticmethod
    def _response(websocket_connection: Any, request_id: int) -> Mapping[str, Any]:
        for _message in range(_MAX_CONTROL_MESSAGES_PER_RESPONSE):
            payload = websocket_connection.recv()
            if payload in (None, "", b""):
                raise RuntimeSyncError(
                    "The shared Codex backend closed the catalog probe", "unavailable"
                )
            if isinstance(payload, bytes):
                try:
                    payload = payload.decode("utf-8")
                except UnicodeDecodeError as exc:
                    raise RuntimeSyncError(
                        "Codex returned an invalid model catalog", "malformed"
                    ) from exc
            if (
                not isinstance(payload, str)
                or len(payload.encode("utf-8")) > _MAX_CONTROL_MESSAGE_BYTES
            ):
                raise RuntimeSyncError(
                    "Codex returned an invalid model catalog", "malformed"
                )
            try:
                message = json.loads(payload)
            except (TypeError, ValueError) as exc:
                raise RuntimeSyncError(
                    "Codex returned an invalid model catalog", "malformed"
                ) from exc
            if not isinstance(message, Mapping) or message.get("id") != request_id:
                continue
            error = message.get("error")
            if error is not None:
                raise RuntimeSyncError(
                    "Codex rejected the read-only model catalog query", "permission"
                )
            return message
        raise RuntimeSyncError(
            "Codex did not return a usable model catalog", "malformed"
        )

    def model_list(self, codex_home: Path, timeout: float) -> Tuple[str, ...]:
        if not hasattr(socket, "AF_UNIX"):
            raise RuntimeSyncError(
                "The shared Codex backend is unavailable on this platform",
                "unavailable",
            )
        socket_path = self._socket_path(codex_home)
        raw_socket = socket.socket(socket.AF_UNIX, socket.SOCK_STREAM)
        websocket_connection = None
        try:
            raw_socket.settimeout(timeout)
            raw_socket.connect(str(socket_path))
            # websocket-client does not advertise compression extensions.  The
            # preconnected socket keeps this a local, read-only Unix transport.
            websocket_connection = websocket.create_connection(
                "ws://localhost/",
                timeout=timeout,
                socket=raw_socket,
                suppress_origin=True,
                enable_multithread=False,
            )
            self._send(
                websocket_connection,
                {
                    "id": 1,
                    "method": "initialize",
                    "params": {
                        "clientInfo": {
                            "name": "easy-multi-provider",
                            "title": "EMP",
                            "version": __version__,
                        },
                        "capabilities": None,
                    },
                },
            )
            self._response(websocket_connection, 1)
            self._send(websocket_connection, {"method": "initialized"})

            models = []
            cursor: Optional[str] = None
            seen_cursors = set()
            for page_index in range(20):
                params: Dict[str, Any] = {
                    "includeHidden": False,
                    "limit": 1000,
                }
                if cursor is not None:
                    params["cursor"] = cursor
                request_id = page_index + 2
                self._send(
                    websocket_connection,
                    {"id": request_id, "method": "model/list", "params": params},
                )
                response = self._response(websocket_connection, request_id)
                result = response.get("result")
                data = result.get("data") if isinstance(result, Mapping) else None
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
                    raise RuntimeSyncError(
                        "Codex model catalog is too large", "malformed"
                    )
                next_cursor = result.get("nextCursor")
                if next_cursor in (None, ""):
                    return tuple(dict.fromkeys(models))
                if (
                    not isinstance(next_cursor, str)
                    or len(next_cursor) > 1024
                    or next_cursor in seen_cursors
                ):
                    raise RuntimeSyncError(
                        "Codex returned an invalid model cursor", "malformed"
                    )
                seen_cursors.add(next_cursor)
                cursor = next_cursor
            raise RuntimeSyncError(
                "Codex model catalog pagination exceeded its limit", "malformed"
            )
        except PermissionError as exc:
            raise RuntimeSyncError(
                "Codex model catalog access was denied", "permission"
            ) from exc
        except (FileNotFoundError, ConnectionRefusedError) as exc:
            raise RuntimeSyncError(
                "The shared Codex backend is unavailable", "unavailable"
            ) from exc
        except (socket.timeout, websocket.WebSocketTimeoutException) as exc:
            raise RuntimeSyncError(
                "The shared Codex backend did not answer the catalog probe",
                "unavailable",
            ) from exc
        except websocket.WebSocketException as exc:
            raise RuntimeSyncError(
                "Codex rejected the WebSocket catalog probe", "malformed"
            ) from exc
        except OSError as exc:
            if exc.errno in (errno.ENOENT, errno.ECONNREFUSED):
                raise RuntimeSyncError(
                    "The shared Codex backend is unavailable", "unavailable"
                ) from exc
            raise RuntimeSyncError(
                "Codex model catalog query failed", "command"
            ) from exc
        finally:
            if websocket_connection is not None:
                try:
                    websocket_connection.close()
                except OSError:
                    pass
            else:
                raw_socket.close()


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
    """Observe a shared Codex runtime without controlling its lifecycle."""

    def __init__(
        self,
        runner: Optional[CommandRunner] = None,
        codex_executable: Optional[str] = None,
        control_timeout: float = 15.0,
        observation_timeout: float = 20.0,
        poll_interval: float = 0.25,
        host_stopper: Optional[TargetedHostStopper] = None,
        target_codex_home: Optional[Path] = None,
        model_catalog_probe: Optional[ModelCatalogProbe] = None,
        windows_local_app_data: Optional[Path] = None,
        runtime_user_home: Optional[Path] = None,
        runtime_preferences: Optional[Sequence[str]] = None,
    ) -> None:
        self.runner = runner or SubprocessRunner()
        self._configured_codex_executable = codex_executable
        self._path_codex_executable = shutil.which("codex")
        self.codex_executable = (
            codex_executable or self._path_codex_executable or "codex"
        )
        self.control_timeout = max(0.01, float(control_timeout))
        self.observation_timeout = min(20.0, max(0.0, float(observation_timeout)))
        self.poll_interval = max(0.001, float(poll_interval))
        self.host_stopper = host_stopper
        self.model_catalog_probe = model_catalog_probe or UnixSocketModelCatalogProbe()
        if windows_local_app_data is not None:
            self._windows_local_app_data = Path(windows_local_app_data)
        elif os.name == "nt" and os.environ.get("LOCALAPPDATA", "").strip():
            self._windows_local_app_data = Path(os.environ["LOCALAPPDATA"])
        else:
            self._windows_local_app_data = None
        self._runtime_user_home = Path(runtime_user_home or Path.home())
        self._runtime_preferences = _normalize_runtime_preferences(
            runtime_preferences
        )
        self._compatibility_lock = threading.Lock()
        self._compatibility_checked_at = 0.0
        self._compatibility: Optional[Dict[str, Any]] = None
        self.target_codex_home = (
            _normalize_codex_home(target_codex_home)
            if target_codex_home is not None
            else None
        )

    def set_target_codex_home(self, target_codex_home: Path) -> None:
        """Bind fallback process selection to the active integration manager."""

        self.target_codex_home = _normalize_codex_home(target_codex_home)
        with self._compatibility_lock:
            self._compatibility = None
            self._compatibility_checked_at = 0.0

    def _targeted_host_stopper(self) -> Optional[TargetedHostStopper]:
        if self.host_stopper is not None:
            return self.host_stopper
        if self.target_codex_home is None:
            return None
        return TargetedCodexHostStopper(Path(self.target_codex_home))

    def _windows_app_codex_executable(self) -> Optional[str]:
        if platform.system() != "Windows":
            return None

        # The unified ChatGPT desktop app publishes its active Codex runtime at
        # this exact path. Fresh installs may have no legacy build directory
        # under LOCALAPPDATA yet.
        if self.target_codex_home is not None:
            plugin_runtime = (
                Path(self.target_codex_home)
                / "plugins"
                / ".plugin-appserver"
                / "codex.exe"
            )
            executable = self._runtime_file(plugin_runtime)
            if executable is not None:
                return executable

        candidates = []
        local_app_data = self._windows_local_app_data
        if local_app_data is not None:
            runtime_root = local_app_data.joinpath(*_WINDOWS_APP_RUNTIME_PARTS)
            try:
                resolved_root = runtime_root.resolve(strict=True)
                entries = tuple(runtime_root.iterdir())
            except (OSError, RuntimeError):
                entries = ()
                resolved_root = None
            if len(entries) <= _MAX_WINDOWS_APP_RUNTIMES and resolved_root is not None:
                # Current builds can place codex.exe directly in bin. Older
                # builds use one build-identity directory below it.
                locations = (runtime_root / "codex.exe",) + tuple(
                    entry / "codex.exe" for entry in entries if entry.is_dir()
                )
                for candidate in locations:
                    executable = self._runtime_file(candidate)
                    if executable is None:
                        continue
                    try:
                        resolved = Path(executable)
                        if (
                            resolved.parent != resolved_root
                            and resolved.parent.parent != resolved_root
                        ):
                            continue
                        candidates.append((resolved.stat().st_mtime_ns, executable))
                    except (OSError, RuntimeError):
                        continue
        if not candidates:
            return None
        # Prefer the most recently installed known App runtime when several
        # layouts or builds remain.
        return max(candidates, key=lambda item: (item[0], item[1]))[1]

    @staticmethod
    def _runtime_file(path: Path) -> Optional[str]:
        try:
            resolved = path.resolve(strict=True)
            if not path.is_file() or path.is_symlink():
                return None
            if resolved.suffix.casefold() != ".exe" and not os.access(
                str(resolved), os.X_OK
            ):
                return None
            return str(resolved)
        except (OSError, RuntimeError):
            return None

    def _codex_home_managed_executable(self) -> Optional[str]:
        if self.target_codex_home is None:
            return None
        current = Path(self.target_codex_home) / "packages" / "standalone" / "current"
        names = ("codex.exe", "codex") if os.name == "nt" else ("codex", "codex.exe")
        candidates = [current / "bin" / name for name in names]
        candidates.extend(current / name for name in names)
        for candidate in candidates:
            try:
                if candidate.is_file() and (
                    os.name == "nt" or os.access(str(candidate), os.X_OK)
                ):
                    return str(candidate)
            except OSError:
                continue
        return None

    def _mac_app_codex_executable(self) -> Optional[str]:
        if platform.system() != "Darwin":
            return None
        # Read the signed app's actual binary, not a standalone package or
        # models_cache.json version left behind by a previous installation.
        for root in (self._runtime_user_home / "Applications", Path("/Applications")):
            for name in ("ChatGPT.app", "Codex.app"):
                executable = self._runtime_file(root / name / "Contents" / "Resources" / "codex")
                if executable is not None:
                    return executable
        if self.target_codex_home is not None:
            return self._runtime_file(Path(self.target_codex_home) / "plugins" / ".plugin-appserver" / "codex")
        return None

    def _editor_codex_executable(self, extensions_root: Path) -> Optional[str]:
        system = {"Windows": "windows", "Linux": "linux", "Darwin": "darwin"}.get(
            platform.system()
        )
        machine = platform.machine().casefold()
        architecture = {
            "amd64": "x86_64",
            "x86_64": "x86_64",
            "arm64": "aarch64",
            "aarch64": "aarch64",
        }.get(machine)
        if system is None or architecture is None:
            return None
        try:
            root = extensions_root.resolve(strict=True)
            extensions = tuple(
                item
                for item in extensions_root.iterdir()
                if item.name.casefold().startswith("openai.chatgpt-")
            )
        except (OSError, RuntimeError):
            return None
        if len(extensions) > _MAX_EDITOR_EXTENSIONS:
            return None

        candidates = []
        name = "codex.exe" if system == "windows" else "codex"
        for extension in extensions:
            bin_root = extension / "bin"
            # Extensions can bundle Windows and WSL binaries together. Only
            # inspect this host's platform, regardless of other files' mtimes.
            locations = (bin_root, bin_root / (system + "-" + architecture))
            for location in locations:
                executable = self._runtime_file(location / name)
                if executable is None:
                    continue
                try:
                    resolved = Path(executable)
                    if root not in resolved.parents:
                        continue
                    candidates.append((resolved.stat().st_mtime_ns, executable))
                except OSError:
                    continue
        if not candidates:
            return None
        return max(candidates, key=lambda item: (item[0], item[1]))[1]

    def _runtime_candidates(self) -> Tuple[CodexRuntimeCandidate, ...]:
        candidates = []
        if self._configured_codex_executable:
            candidates.append(
                CodexRuntimeCandidate(
                    "configured", "Configured Codex", self._configured_codex_executable
                )
            )
        app_executable = self._windows_app_codex_executable() or self._mac_app_codex_executable()
        if app_executable:
            candidates.append(
                CodexRuntimeCandidate(
                    "codex_app", "ChatGPT App (Codex)", app_executable
                )
            )
        managed = self._codex_home_managed_executable()
        if managed:
            candidates.append(
                CodexRuntimeCandidate("managed", "Managed Codex runtime", managed)
            )

        editor_roots = (
            (
                "vscode",
                "VS Code extension",
                self._runtime_user_home / ".vscode" / "extensions",
            ),
            (
                "vscode_insiders",
                "VS Code Insiders extension",
                self._runtime_user_home / ".vscode-insiders" / "extensions",
            ),
            (
                "cursor",
                "Cursor extension",
                self._runtime_user_home / ".cursor" / "extensions",
            ),
        )
        for source, name, root in editor_roots:
            executable = self._editor_codex_executable(root)
            if executable:
                candidates.append(CodexRuntimeCandidate(source, name, executable))

        path_cli = self._path_cli_executable()
        if path_cli:
            candidates.append(
                CodexRuntimeCandidate("path_cli", "PATH Codex CLI", path_cli)
            )

        unique = []
        for candidate in candidates:
            if any(
                self._same_executable(candidate.executable, existing.executable)
                for existing in unique
            ):
                continue
            unique.append(candidate)
        return tuple(unique)

    @staticmethod
    def _same_executable(left: str, right: str) -> bool:
        try:
            return Path(left).resolve() == Path(right).resolve()
        except (OSError, RuntimeError, ValueError):
            return os.path.normcase(os.path.abspath(left)) == os.path.normcase(
                os.path.abspath(right)
            )

    def _path_cli_executable(self) -> Optional[str]:
        current = shutil.which("codex")
        if current:
            self._path_codex_executable = current
        return self._path_codex_executable

    def executable(self) -> str:
        """Return the selected compatible runtime without managing its lifecycle."""

        compatibility = self.compatibility()
        helper_source = compatibility.get("helper_source")
        for runtime in compatibility.get("runtimes", []):
            if runtime.get("source") == helper_source and runtime.get("helper") is True:
                return str(runtime["path"])
        return self.codex_executable

    def _command(self, *parts: str) -> Tuple[str, ...]:
        return (self.executable(), *parts)

    def _observe_compatibility(self, executable: str) -> CodexCompatibility:
        try:
            result = self.runner.run(
                (executable, "--version"),
                timeout=min(self.control_timeout, 2.0),
            )
        except FileNotFoundError:
            return unavailable_compatibility()
        except (OSError, ValueError):
            # A broken or inaccessible client must not discard healthy clients
            # from the concurrent inventory. Do not expose raw OS error paths.
            return CodexCompatibility(None, "unknown")
        if result.returncode == 0:
            return classify_codex_version(result.stdout)
        if result.returncode == 127 or "not found" in (
            result.stdout + " " + result.stderr
        ).casefold():
            return unavailable_compatibility()
        return CodexCompatibility(None, "unknown")

    def set_runtime_preferences(self, sources: Sequence[str]) -> None:
        preferences = _normalize_runtime_preferences(sources)
        with self._compatibility_lock:
            self._runtime_preferences = preferences
            self._compatibility = None
            self._compatibility_checked_at = 0.0

    def refresh_compatibility(self) -> Dict[str, Any]:
        with self._compatibility_lock:
            self._compatibility = None
            self._compatibility_checked_at = 0.0
        return self.compatibility()

    def compatibility(self) -> Dict[str, Any]:
        """Return one bounded, cached inventory of local Codex runtimes."""
        now = time.monotonic()
        with self._compatibility_lock:
            if (
                self._compatibility is not None
                and now - self._compatibility_checked_at
                < _COMPATIBILITY_CACHE_SECONDS
            ):
                return dict(self._compatibility)

        candidates = self._runtime_candidates()
        if candidates:
            workers = min(4, len(candidates))
            with ThreadPoolExecutor(max_workers=workers) as executor:
                observed = tuple(
                    executor.map(
                        lambda candidate: self._observe_compatibility(candidate.executable),
                        candidates,
                    )
                )
        else:
            observed = ()

        runtimes = []
        for candidate, compatibility in zip(candidates, observed):
            item = compatibility.public()
            item.update(
                {
                    "source": candidate.source,
                    "name": candidate.name,
                    "path": candidate.executable,
                    "selectable": compatibility.status in _SELECTABLE_COMPATIBILITY,
                    "helper": False,
                }
            )
            runtimes.append(item)

        preferences = self._runtime_preferences
        automatic = preferences == ("auto",)
        allowed_sources = None if automatic else frozenset(preferences)
        for item in runtimes:
            item["targeted"] = bool(
                item["selectable"]
                and (allowed_sources is None or item["source"] in allowed_sources)
            )
        selectable = [item for item in runtimes if item["selectable"]]
        selected = next(
            (
                item
                for item in selectable
                if item["source"] == "configured"
                and self._configured_codex_executable is not None
            ),
            None,
        )
        if selected is None and selectable:
            selected = min(
                enumerate(selectable),
                key=lambda entry: (
                    _COMPATIBILITY_PRIORITY.get(str(entry[1].get("status")), 99),
                    entry[0],
                ),
            )[1]
        if selected is not None:
            selected["helper"] = True
            public = {
                key: selected.get(key)
                for key in ("installed", "status", "supported_range", "recommended")
            }
            public["source"] = selected["source"]
            helper_source: Optional[str] = str(selected["source"])
        elif runtimes:
            first = runtimes[0]
            public = {
                key: first.get(key)
                for key in ("installed", "status", "supported_range", "recommended")
            }
            public["source"] = first["source"]
            helper_source = None
        else:
            public = unavailable_compatibility().public()
            public["source"] = "path_cli"
            helper_source = None
        public["helper_source"] = helper_source
        public["preferences"] = list(preferences)
        public["runtimes"] = runtimes
        with self._compatibility_lock:
            self._compatibility = dict(public)
            self._compatibility_checked_at = time.monotonic()
        return public

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
        if self.target_codex_home is None:
            raise RuntimeSyncError(
                "The active Codex integration home is unavailable", "unsupported"
            )
        return self.model_catalog_probe.model_list(
            Path(self.target_codex_home), self.control_timeout
        )

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
        if not expected:
            return RuntimeSyncResult(
                CATALOG_UNVERIFIED,
                target,
                False,
                "Catalog settings are saved. Without distinct EMP model IDs, "
                "this check cannot verify native model visibility or display names. "
                "Restart active Codex clients safely, then check their model picker.",
                observed,
            )
        observed_set = set(observed)
        if target == "emp":
            missing = sorted(expected - observed_set)
            if missing:
                detail = "Codex is missing %d expected EMP model(s)" % len(missing)
                return RuntimeSyncResult(
                    VERIFICATION_FAILED, target, False, detail, observed
                )
            return RuntimeSyncResult(
                EMP_LOADED,
                target,
                True,
                "The shared Codex backend exposes all expected EMP model IDs; "
                "other startup settings are not verified",
                observed,
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
            NATIVE_LOADED,
            target,
            True,
            "The shared Codex backend exposes no expected EMP model IDs; other "
            "startup settings are not verified",
            observed,
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
                "Confirmation is required before checking the shared Codex backend",
            )
        if not expected_models:
            return self._validate_models((), expected_models, target)
        try:
            observed_models = self._model_list()
        except RuntimeSyncError as error:
            if error.kind == "unavailable":
                return RuntimeSyncResult(
                    STOPPED_WAITING_FOR_START,
                    target,
                    False,
                    "The shared Codex backend is unavailable; configuration is saved "
                    "and will load when its owner starts it",
                )
            return RuntimeSyncResult(
                UNSUPPORTED if error.kind == "unsupported" else VERIFICATION_FAILED,
                target,
                False,
                str(error),
            )
        observed = self._validate_models(observed_models, expected_models, target)
        if not observed.verified:
            return RuntimeSyncResult(
                RELOAD_REQUIRED,
                target,
                False,
                "Configuration is saved, but the shared Codex backend still has the "
                "previous catalog; wait for its owner to restart it in a safe "
                "maintenance window",
                observed.observed_models,
            )
        return observed

    def observe(
        self,
        expected_models: Sequence[str],
        target: str,
    ) -> RuntimeSyncResult:
        """Verify the live catalog without stopping or mutating Codex."""
        if target not in ("emp", "native"):
            return RuntimeSyncResult(UNSUPPORTED, target, False, "Unknown runtime target")
        if not expected_models:
            return self._validate_models((), expected_models, target)
        try:
            observed = self._model_list()
        except RuntimeSyncError as error:
            if error.kind == "unavailable":
                return RuntimeSyncResult(
                    STOPPED_WAITING_FOR_START,
                    target,
                    False,
                    "The shared Codex backend is unavailable; configuration is saved "
                    "and will load when its owner starts it",
                )
            return RuntimeSyncResult(
                UNSUPPORTED if error.kind == "unsupported" else VERIFICATION_FAILED,
                target,
                False,
                str(error),
            )
        return self._validate_models(observed, expected_models, target)
