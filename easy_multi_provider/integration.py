"""Transactional, field-level integration with an explicit Codex config.

The module owns only ``openai_base_url`` and ``model_catalog_json``.  TOML
parsing and style-preserving serialization are delegated to tomlkit; lease
transitions and file replacement remain local and explicit.
"""

from __future__ import annotations

import contextlib
import errno
import json
import os
import stat
import tempfile
import time
import uuid
from dataclasses import dataclass, replace
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Callable, Dict, Iterator, Mapping, Optional, Sequence, Tuple, Union

from tomlkit import document, dumps, parse
from tomlkit.exceptions import ParseError


MANAGED_FIELDS = ("openai_base_url", "model_catalog_json")
LEASE_SCHEMA = "easy-multi-provider.integration-lease"
LEASE_VERSION = 2
LEASE_STATUSES = ("prepared", "active", "restoring", "restored")
_ALLOWED_TRANSITIONS = {
    "prepared": {"active", "restoring", "restored"},
    "active": {"active", "restoring", "restored"},
    "restoring": {"active", "restoring", "restored"},
    "restored": {"restored"},
}
_LEASE_KEYS = {
    "schema",
    "version",
    "config_path",
    "config_existed",
    "fields",
    "lease_id",
    "instance_id",
    "pid",
    "status",
    "created_at",
    "updated_at",
}


class IntegrationError(ValueError):
    """Raised when integration cannot proceed without risking user state."""


class ServiceNotReady(IntegrationError):
    """Raised when the caller has not proved that EMP accepts requests."""


class LeaseError(IntegrationError):
    """Raised when a recovery record is malformed or cannot be read."""


class LockTimeout(IntegrationError):
    """Raised when another process holds the integration transaction lock."""


class SymlinkConfigError(IntegrationError):
    """Raised because config symlinks are rejected rather than replaced."""


@dataclass(frozen=True)
class FieldState:
    present: bool
    value: Optional[str]

    def __post_init__(self) -> None:
        if self.present and not isinstance(self.value, str):
            raise ValueError("a present managed field must have a string value")
        if not self.present and self.value is not None:
            raise ValueError("an absent managed field cannot have a value")

    def to_dict(self) -> Dict[str, Any]:
        return {"present": self.present, "value": self.value}


@dataclass(frozen=True)
class FieldRecovery:
    original: FieldState
    applied: FieldState

    def to_dict(self) -> Dict[str, Any]:
        return {"original": self.original.to_dict(), "applied": self.applied.to_dict()}


@dataclass(frozen=True)
class LeaseRecord:
    """Only the values and instance metadata needed to recover the config."""

    config_path: str
    config_existed: bool
    fields: Mapping[str, FieldRecovery]
    lease_id: str
    instance_id: str
    pid: int
    status: str
    created_at: str
    updated_at: str

    def to_dict(self) -> Dict[str, Any]:
        return {
            "schema": LEASE_SCHEMA,
            "version": LEASE_VERSION,
            "config_path": self.config_path,
            "config_existed": self.config_existed,
            "fields": {field: self.fields[field].to_dict() for field in MANAGED_FIELDS},
            "lease_id": self.lease_id,
            "instance_id": self.instance_id,
            "pid": self.pid,
            "status": self.status,
            "created_at": self.created_at,
            "updated_at": self.updated_at,
        }


@dataclass(frozen=True)
class IntegrationStatus:
    state: str
    relation: str
    config_path: Path
    config_exists: bool
    fields: Mapping[str, FieldState]
    lease: Optional[LeaseRecord]
    same_instance: bool
    conflicts: Tuple[str, ...]

    def to_dict(self) -> Dict[str, Any]:
        return {
            "state": self.state,
            "relation": self.relation,
            "config_path": str(self.config_path),
            "config_exists": self.config_exists,
            "fields": {field: self.fields[field].to_dict() for field in MANAGED_FIELDS},
            "lease": self.lease.to_dict() if self.lease else None,
            "same_instance": self.same_instance,
            "conflicts": list(self.conflicts),
        }


@dataclass(frozen=True)
class IntegrationResult:
    action: str
    state: str
    relation: str
    fields: Mapping[str, FieldState]
    lease: Optional[LeaseRecord]
    conflicts: Tuple[str, ...] = ()

    @property
    def ok(self) -> bool:
        return self.state != "conflict" and not self.conflicts

    def to_dict(self) -> Dict[str, Any]:
        return {
            "action": self.action,
            "state": self.state,
            "relation": self.relation,
            "fields": {field: self.fields[field].to_dict() for field in MANAGED_FIELDS},
            "lease": self.lease.to_dict() if self.lease else None,
            "conflicts": list(self.conflicts),
        }


def _now() -> str:
    return datetime.now(timezone.utc).isoformat()


def _toml_value(value: Any, field: str) -> str:
    if not isinstance(value, str) or "\n" in value or "\r" in value:
        raise IntegrationError("%s must be a single-line string" % field)
    return value


def _reject_symlink(path: Path, label: str) -> None:
    try:
        is_link = path.is_symlink()
    except OSError as exc:
        raise IntegrationError("unable to inspect %s" % label) from exc
    if is_link:
        raise SymlinkConfigError("%s must not be a symlink" % label)


def _reject_symlink_directory_components(path: Path, label: str) -> Path:
    """Return an absolute directory path whose existing components are real."""

    candidate = Path(os.path.abspath(os.fspath(Path(path).expanduser())))
    for component in list(reversed(candidate.parents)) + [candidate]:
        try:
            info = component.lstat()
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise IntegrationError("unable to inspect %s" % label) from exc
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise SymlinkConfigError("%s must not contain a symlink" % label)
    return candidate


def _fsync_directory(directory: Path) -> None:
    """Best-effort directory durability after the name replacement."""

    try:
        descriptor = os.open(str(directory), os.O_RDONLY)
    except OSError:
        return
    try:
        try:
            os.fsync(descriptor)
        except OSError:
            # The replacement already happened.  Do not report it as a failed
            # write merely because this optional durability step is missing.
            return
    finally:
        try:
            os.close(descriptor)
        except OSError:
            pass


def atomic_write_text(path: Path, text: str, mode: int = 0o600) -> Path:
    """Replace a regular file atomically, with permissions set before replace."""

    path = Path(path)
    _reject_symlink(path, "target path")
    _reject_symlink_directory_components(path.parent, "target directory")
    path.parent.mkdir(parents=True, exist_ok=True)
    _reject_symlink_directory_components(path.parent, "target directory")
    fd, temporary = tempfile.mkstemp(prefix=".emp-integration-", dir=str(path.parent))
    try:
        try:
            os.fchmod(fd, mode)
        except AttributeError:  # pragma: no cover - exercised on Windows
            os.chmod(temporary, mode)
        with os.fdopen(fd, "w", encoding="utf-8", newline="") as handle:
            fd = -1
            handle.write(text)
            handle.flush()
            os.fsync(handle.fileno())
        _reject_symlink(path, "target path")
        os.replace(temporary, str(path))
        _fsync_directory(path.parent)
    finally:
        if fd >= 0:
            os.close(fd)
        if os.path.exists(temporary):
            os.unlink(temporary)
    return path


try:
    import fcntl
except ImportError:  # pragma: no cover - exercised on Windows
    fcntl = None

try:
    import msvcrt
except ImportError:  # pragma: no cover - exercised on POSIX
    msvcrt = None


class _FileLock:
    def __init__(self, path: Path, timeout: float, poll_interval: float = 0.02) -> None:
        if timeout < 0:
            raise ValueError("lock timeout cannot be negative")
        self.path = Path(path)
        self.timeout = timeout
        self.poll_interval = max(0.001, poll_interval)
        self._fd = -1

    def _try_acquire(self) -> bool:
        if fcntl is not None:
            try:
                fcntl.flock(self._fd, fcntl.LOCK_EX | fcntl.LOCK_NB)
                return True
            except OSError as exc:
                if exc.errno in (errno.EACCES, errno.EAGAIN):
                    return False
                raise LockTimeout("unable to acquire integration lock") from exc
        if msvcrt is not None:  # pragma: no cover - exercised on Windows
            try:
                os.lseek(self._fd, 0, os.SEEK_SET)
                if os.fstat(self._fd).st_size == 0:
                    os.write(self._fd, b"\0")
                os.lseek(self._fd, 0, os.SEEK_SET)
                msvcrt.locking(self._fd, msvcrt.LK_NBLCK, 1)
                return True
            except OSError as exc:
                if exc.errno in (errno.EACCES, errno.EDEADLK, errno.EAGAIN):
                    return False
                raise LockTimeout("unable to acquire integration lock") from exc
        raise LockTimeout("this platform has no supported file-lock primitive")

    def __enter__(self) -> "_FileLock":
        _reject_symlink(self.path, "integration lock")
        parent = _reject_symlink_directory_components(
            self.path.parent, "integration lock directory"
        )
        self.path.parent.mkdir(parents=True, exist_ok=True)
        parent = _reject_symlink_directory_components(
            self.path.parent, "integration lock directory"
        )
        parent_fd = -1
        try:
            if os.name != "nt" and hasattr(os, "O_DIRECTORY"):
                directory_flags = os.O_RDONLY | os.O_DIRECTORY
                if hasattr(os, "O_NOFOLLOW"):
                    directory_flags |= os.O_NOFOLLOW
                parent_fd = os.open(str(parent), directory_flags)
                if hasattr(os, "fchmod"):
                    os.fchmod(parent_fd, 0o700)
                file_flags = os.O_RDWR | os.O_CREAT
                if hasattr(os, "O_NOFOLLOW"):
                    file_flags |= os.O_NOFOLLOW
                self._fd = os.open(
                    self.path.name,
                    file_flags,
                    0o600,
                    dir_fd=parent_fd,
                )
            else:  # pragma: no cover - Windows exercises this path
                os.chmod(str(parent), 0o700)
                self._fd = os.open(str(self.path), os.O_RDWR | os.O_CREAT, 0o600)
            if not stat.S_ISREG(os.fstat(self._fd).st_mode):
                raise OSError("integration lock is not a regular file")
            if hasattr(os, "fchmod"):
                os.fchmod(self._fd, 0o600)
            else:  # pragma: no cover - Windows without fchmod
                os.chmod(str(self.path), 0o600)
        except OSError as exc:
            if self._fd >= 0:
                os.close(self._fd)
                self._fd = -1
            raise LockTimeout("unable to open integration lock") from exc
        finally:
            if parent_fd >= 0:
                os.close(parent_fd)
        deadline = time.monotonic() + self.timeout
        try:
            while True:
                if self._try_acquire():
                    return self
                if time.monotonic() >= deadline:
                    raise LockTimeout(
                        "integration lock timed out after %.3f seconds" % self.timeout
                    )
                time.sleep(self.poll_interval)
        except Exception:
            os.close(self._fd)
            self._fd = -1
            raise

    def __exit__(self, exc_type: Any, exc_value: Any, traceback: Any) -> None:
        if self._fd < 0:
            return
        try:
            if fcntl is not None:
                fcntl.flock(self._fd, fcntl.LOCK_UN)
            elif msvcrt is not None:  # pragma: no cover - exercised on Windows
                os.lseek(self._fd, 0, os.SEEK_SET)
                msvcrt.locking(self._fd, msvcrt.LK_UNLCK, 1)
        finally:
            os.close(self._fd)
            self._fd = -1


def _field_state(value: Any, field: str) -> FieldState:
    if not isinstance(value, dict) or set(value) != {"present", "value"}:
        raise LeaseError("invalid lease field state: %s" % field)
    present = value["present"]
    raw = value["value"]
    if not isinstance(present, bool):
        raise LeaseError("invalid lease field presence: %s" % field)
    if present and not isinstance(raw, str):
        raise LeaseError("invalid lease field value: %s" % field)
    if not present and raw is not None:
        raise LeaseError("invalid lease field value: %s" % field)
    return FieldState(present, raw)


def _lease_from_dict(raw: Any, config_path: Path) -> LeaseRecord:
    if not isinstance(raw, dict) or set(raw) != _LEASE_KEYS:
        raise LeaseError("lease record contains unsupported fields")
    if raw.get("schema") != LEASE_SCHEMA or raw.get("version") != LEASE_VERSION:
        raise LeaseError("unsupported lease record version")
    if raw.get("config_path") != str(config_path.resolve()):
        raise LeaseError("lease record targets another config")
    if not isinstance(raw.get("config_existed"), bool):
        raise LeaseError("invalid lease config existence")
    raw_fields = raw.get("fields")
    if not isinstance(raw_fields, dict) or set(raw_fields) != set(MANAGED_FIELDS):
        raise LeaseError("invalid lease managed fields")
    fields: Dict[str, FieldRecovery] = {}
    for field in MANAGED_FIELDS:
        recovery = raw_fields[field]
        if not isinstance(recovery, dict) or set(recovery) != {"original", "applied"}:
            raise LeaseError("invalid lease recovery field: %s" % field)
        original = _field_state(recovery["original"], field)
        applied = _field_state(recovery["applied"], field)
        if not applied.present:
            raise LeaseError("lease applied field is absent: %s" % field)
        fields[field] = FieldRecovery(original=original, applied=applied)
    for key in ("lease_id", "instance_id", "created_at", "updated_at"):
        if not isinstance(raw.get(key), str) or not raw[key]:
            raise LeaseError("invalid lease metadata: %s" % key)
    if not isinstance(raw.get("pid"), int) or isinstance(raw.get("pid"), bool):
        raise LeaseError("invalid lease process metadata")
    if raw.get("status") not in LEASE_STATUSES:
        raise LeaseError("invalid lease status")
    return LeaseRecord(
        config_path=raw["config_path"],
        config_existed=raw["config_existed"],
        fields=fields,
        lease_id=raw["lease_id"],
        instance_id=raw["instance_id"],
        pid=raw["pid"],
        status=raw["status"],
        created_at=raw["created_at"],
        updated_at=raw["updated_at"],
    )


ServiceReady = Union[bool, Callable[[], bool]]


class IntegrationManager:
    """Manage integration with explicit paths and serialized transactions."""

    def __init__(
        self,
        config_path: Path,
        lease_path: Path,
        instance_id: Optional[str] = None,
        lock_timeout: float = 5.0,
        lock_path: Optional[Path] = None,
    ) -> None:
        self.config_path = Path(config_path)
        self.lease_path = Path(lease_path)
        self.lock_path = Path(lock_path or (str(self.lease_path) + ".lock"))
        self.operation_lock_path = self.lease_path.with_name("operation.lock")
        if self.config_path.resolve() == self.lease_path.resolve():
            raise ValueError("config and lease paths must differ")
        self.instance_id = instance_id or ("instance-" + uuid.uuid4().hex)
        if not isinstance(self.instance_id, str) or not self.instance_id:
            raise ValueError("instance_id is required")
        self.lock_timeout = lock_timeout

    def _assert_safe_paths(self) -> None:
        _reject_symlink(self.config_path, "Codex config")
        _reject_symlink(self.lease_path, "integration lease")
        _reject_symlink(self.lock_path, "integration lock")
        _reject_symlink(self.operation_lock_path, "integration operation lock")

    @contextlib.contextmanager
    def _transaction_lock(self) -> Iterator[None]:
        with _FileLock(self.lock_path, self.lock_timeout):
            yield

    @contextlib.contextmanager
    def operation_lock(self) -> Iterator[None]:
        """Serialize the complete config-plus-runtime operation across EMP processes."""

        with _FileLock(self.operation_lock_path, self.lock_timeout):
            yield

    def _read_config(self) -> Tuple[Any, bool]:
        _reject_symlink(self.config_path, "Codex config")
        if not self.config_path.exists():
            return document(), False
        if not self.config_path.is_file():
            raise IntegrationError("Codex config path is not a file")
        try:
            text = self.config_path.read_text(encoding="utf-8")
            return parse(text), True
        except (OSError, ParseError, ValueError, TypeError) as exc:
            raise IntegrationError("unable to parse Codex TOML config") from exc

    @staticmethod
    def _states(config: Any) -> Dict[str, FieldState]:
        states: Dict[str, FieldState] = {}
        for field in MANAGED_FIELDS:
            if field not in config:
                states[field] = FieldState(False, None)
                continue
            try:
                value = config[field].unwrap()
            except AttributeError as exc:
                raise IntegrationError("managed TOML field is not a scalar: %s" % field) from exc
            if not isinstance(value, str):
                raise IntegrationError("managed TOML field is not a string: %s" % field)
            states[field] = FieldState(True, value)
        return states

    @staticmethod
    def _set_states(config: Any, states: Mapping[str, FieldState]) -> Any:
        for field in MANAGED_FIELDS:
            state = states[field]
            if state.present:
                config[field] = _toml_value(state.value, field)
            elif field in config:
                del config[field]
        return config

    def _write_config(self, config: Any) -> None:
        atomic_write_text(self.config_path, dumps(config), mode=0o600)

    def _remove_config(self) -> None:
        """Remove a config that EMP created and that is now empty."""

        _reject_symlink(self.config_path, "Codex config")
        try:
            self.config_path.unlink()
        except FileNotFoundError:
            pass

    @staticmethod
    def _has_content(config: Any) -> bool:
        """Treat TOML keys and comments as user content, not blank trivia."""

        return bool(dumps(config).strip())

    def _restore_config(self, config: Any, lease: LeaseRecord) -> None:
        """Apply original fields, deleting an EMP-created empty config only."""

        original = {field: lease.fields[field].original for field in MANAGED_FIELDS}
        self._set_states(config, original)
        if not lease.config_existed and not self._has_content(config):
            self._remove_config()
        else:
            self._write_config(config)

    def _read_lease(self) -> Optional[LeaseRecord]:
        _reject_symlink(self.lease_path, "integration lease")
        if not self.lease_path.exists():
            return None
        try:
            raw = json.loads(self.lease_path.read_text(encoding="utf-8"))
        except (OSError, ValueError) as exc:
            raise LeaseError("unable to read integration lease") from exc
        return _lease_from_dict(raw, self.config_path)

    def _write_lease(self, lease: LeaseRecord) -> LeaseRecord:
        self.lease_path.parent.mkdir(parents=True, exist_ok=True)
        os.chmod(str(self.lease_path.parent), 0o700)
        atomic_write_text(
            self.lease_path,
            json.dumps(lease.to_dict(), indent=2, sort_keys=True) + "\n",
            mode=0o600,
        )
        return lease

    def _make_lease(
        self,
        original: Mapping[str, FieldState],
        applied: Mapping[str, FieldState],
        config_existed: bool,
        status: str,
    ) -> LeaseRecord:
        now = _now()
        return LeaseRecord(
            config_path=str(self.config_path.resolve()),
            config_existed=config_existed,
            fields={
                field: FieldRecovery(original=original[field], applied=applied[field])
                for field in MANAGED_FIELDS
            },
            lease_id="lease-" + uuid.uuid4().hex,
            instance_id=self.instance_id,
            pid=os.getpid(),
            status=status,
            created_at=now,
            updated_at=now,
        )

    def _transition(self, lease: LeaseRecord, status: str, re_adopt: bool = False) -> LeaseRecord:
        if status not in LEASE_STATUSES:
            raise LeaseError("invalid lease transition")
        if status not in _ALLOWED_TRANSITIONS[lease.status]:
            raise LeaseError("invalid lease transition from %s to %s" % (lease.status, status))
        return replace(
            lease,
            lease_id="lease-" + uuid.uuid4().hex if re_adopt else lease.lease_id,
            instance_id=self.instance_id if re_adopt else lease.instance_id,
            pid=os.getpid() if re_adopt else lease.pid,
            status=status,
            updated_at=_now(),
        )

    @staticmethod
    def _relation(current: Mapping[str, FieldState], lease: LeaseRecord) -> str:
        original = {field: lease.fields[field].original for field in MANAGED_FIELDS}
        applied = {field: lease.fields[field].applied for field in MANAGED_FIELDS}
        if current == original:
            return "original"
        if current == applied:
            return "applied"
        if all(
            current[field] in (original[field], applied[field])
            for field in MANAGED_FIELDS
        ):
            return "mixed"
        return "other"

    @staticmethod
    def _conflicts(current: Mapping[str, FieldState], lease: LeaseRecord) -> Tuple[str, ...]:
        return tuple(
            field
            for field in MANAGED_FIELDS
            if current[field]
            not in (lease.fields[field].original, lease.fields[field].applied)
        )

    @classmethod
    def _conflict_names(
        cls, current: Mapping[str, FieldState], lease: LeaseRecord
    ) -> Tuple[str, ...]:
        conflicts = cls._conflicts(current, lease)
        if not conflicts:
            relation = cls._relation(current, lease)
            if relation == "mixed":
                return ("mixed_state",)
            if relation == "applied":
                return ("lease_state_mismatch",)
        return conflicts

    def _result(
        self,
        action: str,
        state: str,
        relation: str,
        fields: Mapping[str, FieldState],
        lease: Optional[LeaseRecord],
        conflicts: Sequence[str] = (),
    ) -> IntegrationResult:
        return IntegrationResult(action, state, relation, dict(fields), lease, tuple(conflicts))

    @staticmethod
    def _confirm_service(service_ready: ServiceReady) -> None:
        try:
            ready = service_ready() if callable(service_ready) else service_ready
        except Exception as exc:
            raise ServiceNotReady("EMP service readiness check failed") from exc
        if ready is not True:
            raise ServiceNotReady("EMP service must be listening and accepting requests")

    def status(self) -> IntegrationStatus:
        self._assert_safe_paths()
        with self._transaction_lock():
            config, config_exists = self._read_config()
            fields = self._states(config)
            lease = self._read_lease()
            if lease is None:
                return IntegrationStatus(
                    "native", "unleased", self.config_path, config_exists, fields, None, False, ()
                )
            relation = self._relation(fields, lease)
            conflicts: Tuple[str, ...] = ()
            if relation == "other" or (lease.status == "restored" and relation != "original"):
                names = self._conflict_names(fields, lease)
                conflicts = tuple(dict.fromkeys(("lease_state_mismatch",) + names))
            state = "conflict" if conflicts else lease.status
            return IntegrationStatus(
                state,
                relation,
                self.config_path,
                config_exists,
                fields,
                lease,
                lease.instance_id == self.instance_id,
                conflicts,
            )

    def _adopt_applied(self, lease: LeaseRecord, fields: Mapping[str, FieldState]) -> IntegrationResult:
        adopted = self._transition(lease, "active", re_adopt=True)
        self._write_lease(adopted)
        return self._result("re_adopted", "active", "applied", fields, adopted)

    def enable(
        self,
        openai_base_url: str,
        model_catalog_json: str,
        service_ready: ServiceReady = False,
    ) -> IntegrationResult:
        """Prepare a lease, apply TOML, then commit the lease as active."""

        desired = {
            "openai_base_url": FieldState(True, _toml_value(openai_base_url, "openai_base_url")),
            "model_catalog_json": FieldState(True, _toml_value(model_catalog_json, "model_catalog_json")),
        }
        self._confirm_service(service_ready)
        self._assert_safe_paths()
        with self._transaction_lock():
            config, config_exists = self._read_config()
            current = self._states(config)
            lease = self._read_lease()
            if lease is not None and lease.status != "restored":
                relation = self._relation(current, lease)
                if relation == "other" or relation == "mixed":
                    return self._result(
                        "conflict", "conflict", relation, current, lease, self._conflict_names(current, lease)
                    )
                if relation == "applied":
                    applied = {field: lease.fields[field].applied for field in MANAGED_FIELDS}
                    if desired != applied:
                        return self._result(
                            "conflict", "conflict", relation, current, lease, ("active_lease",)
                        )
                    return self._adopt_applied(lease, current)
                self._write_lease(self._transition(lease, "restored"))

            prepared = self._make_lease(current, desired, config_exists, "prepared")
            self._write_lease(prepared)
            self._set_states(config, desired)
            self._write_config(config)
            verified_config, _ = self._read_config()
            verified_fields = self._states(verified_config)
            if verified_fields != desired:
                return self._result(
                    "conflict",
                    "conflict",
                    self._relation(verified_fields, prepared),
                    verified_fields,
                    prepared,
                    self._conflict_names(verified_fields, prepared),
                )
            active = self._transition(prepared, "active")
            self._write_lease(active)
            return self._result("enabled", "active", "applied", desired, active)

    def restore(self) -> IntegrationResult:
        """Restore original values offline and converge every recoverable phase."""

        self._assert_safe_paths()
        with self._transaction_lock():
            config, config_exists = self._read_config()
            current = self._states(config)
            lease = self._read_lease()
            if lease is None:
                return self._result("noop", "native", "unleased", current, None)
            relation = self._relation(current, lease)
            if lease.status == "restored":
                if relation == "original":
                    return self._result("noop", "restored", relation, current, lease)
                return self._result(
                    "conflict", "conflict", relation, current, lease, self._conflict_names(current, lease)
                )
            if relation == "other":
                return self._result(
                    "conflict", "conflict", relation, current, lease, self._conflict_names(current, lease)
                )
            if relation == "original":
                if not lease.config_existed and config_exists and not self._has_content(config):
                    restoring = self._transition(lease, "restoring")
                    self._write_lease(restoring)
                    self._remove_config()
                    restored = self._transition(restoring, "restored")
                    self._write_lease(restored)
                    return self._result("restored", "restored", relation, current, restored)
                restored = self._transition(lease, "restored")
                self._write_lease(restored)
                return self._result("restored", "restored", relation, current, restored)

            restoring = self._transition(lease, "restoring")
            self._write_lease(restoring)
            original = {field: lease.fields[field].original for field in MANAGED_FIELDS}
            self._restore_config(config, lease)
            verified_config, _ = self._read_config()
            verified_fields = self._states(verified_config)
            if verified_fields != original:
                return self._result(
                    "conflict",
                    "conflict",
                    self._relation(verified_fields, restoring),
                    verified_fields,
                    restoring,
                    self._conflict_names(verified_fields, restoring),
                )
            restored = self._transition(restoring, "restored")
            self._write_lease(restored)
            return self._result("restored", "restored", "original", original, restored)

    def recover(
        self,
        re_adopt: bool = False,
        service_ready: ServiceReady = False,
    ) -> IntegrationResult:
        """Recover stale state, optionally re-adopting only after readiness proof."""

        if not re_adopt:
            return self.restore()
        self._confirm_service(service_ready)
        self._assert_safe_paths()
        with self._transaction_lock():
            config, _ = self._read_config()
            current = self._states(config)
            lease = self._read_lease()
            if lease is None:
                return self._result("noop", "native", "unleased", current, None)
            relation = self._relation(current, lease)
            if lease.status == "restored":
                if relation == "original":
                    return self._result("noop", "restored", relation, current, lease)
                return self._result(
                    "conflict", "conflict", relation, current, lease, self._conflict_names(current, lease)
                )
            if relation == "other" or relation == "mixed":
                return self._result(
                    "conflict", "conflict", relation, current, lease, self._conflict_names(current, lease)
                )
            if relation == "original":
                restored = self._transition(lease, "restored")
                self._write_lease(restored)
                return self._result("recovered_restored", "restored", relation, current, restored)
            return self._adopt_applied(lease, current)


def enable(
    config_path: Path,
    lease_path: Path,
    openai_base_url: str,
    model_catalog_json: str,
    service_ready: ServiceReady = False,
    instance_id: Optional[str] = None,
    lock_timeout: float = 5.0,
) -> IntegrationResult:
    return IntegrationManager(config_path, lease_path, instance_id, lock_timeout).enable(
        openai_base_url, model_catalog_json, service_ready
    )


def status(
    config_path: Path,
    lease_path: Path,
    instance_id: Optional[str] = None,
    lock_timeout: float = 5.0,
) -> IntegrationStatus:
    return IntegrationManager(config_path, lease_path, instance_id, lock_timeout).status()


def restore(
    config_path: Path,
    lease_path: Path,
    instance_id: Optional[str] = None,
    lock_timeout: float = 5.0,
) -> IntegrationResult:
    return IntegrationManager(config_path, lease_path, instance_id, lock_timeout).restore()


def recover(
    config_path: Path,
    lease_path: Path,
    re_adopt: bool = False,
    service_ready: ServiceReady = False,
    instance_id: Optional[str] = None,
    lock_timeout: float = 5.0,
) -> IntegrationResult:
    return IntegrationManager(config_path, lease_path, instance_id, lock_timeout).recover(
        re_adopt, service_ready
    )
