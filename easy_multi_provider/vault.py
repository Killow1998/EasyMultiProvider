"""Small encrypted-at-rest vault for local credentials."""

from __future__ import annotations

import json
import os
import stat
import tempfile
import threading
from contextlib import contextmanager
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from cryptography.fernet import Fernet, InvalidToken


MASTER_KEY_ENV = "EASY_MULTI_PROVIDER_MASTER_KEY"
MASTER_KEY_FILE_ENV = "EASY_MULTI_PROVIDER_MASTER_KEY_FILE"
DEFAULT_MASTER_KEY_FILE = Path("state") / "master.key"
_FORMAT = b"easy-multi-provider-v1\n"
_default_master_key_file = DEFAULT_MASTER_KEY_FILE
_default_master_key_lock = threading.RLock()
_file_transaction_lock = threading.RLock()
_MAX_TRANSACTION_FILE_BYTES = 64 * 1024 * 1024


class VaultError(ValueError):
    """Raised when encrypted credentials cannot be read or written safely."""


class FileTransaction:
    """Restore a bounded set of managed files if a multi-file update fails."""

    def __init__(self) -> None:
        self._snapshots: Dict[Path, Optional[Tuple[bytes, int]]] = {}

    def remember(self, path: Path) -> None:
        target = Path(os.path.abspath(os.fspath(Path(path))))
        if target in self._snapshots:
            return
        try:
            info = target.lstat()
        except FileNotFoundError:
            self._snapshots[target] = None
            return
        except OSError as exc:
            raise VaultError("managed file is unavailable") from exc
        if not stat.S_ISREG(info.st_mode):
            raise VaultError("managed file is not a regular file")
        if info.st_size > _MAX_TRANSACTION_FILE_BYTES:
            raise VaultError("managed file is too large for an atomic update")
        try:
            value = target.read_bytes()
        except OSError as exc:
            raise VaultError("managed file is unavailable") from exc
        if len(value) > _MAX_TRANSACTION_FILE_BYTES:
            raise VaultError("managed file is too large for an atomic update")
        self._snapshots[target] = (value, stat.S_IMODE(info.st_mode))

    @staticmethod
    def _restore(path: Path, value: bytes, mode: int) -> None:
        path.parent.mkdir(parents=True, exist_ok=True)
        descriptor, temporary = tempfile.mkstemp(
            prefix=".emp-rollback-", dir=str(path.parent)
        )
        try:
            os.chmod(temporary, mode or 0o600)
            with os.fdopen(descriptor, "wb") as handle:
                descriptor = -1
                handle.write(value)
            os.replace(temporary, str(path))
        finally:
            if descriptor >= 0:
                os.close(descriptor)
            if os.path.exists(temporary):
                os.unlink(temporary)

    def rollback(self) -> None:
        for path, snapshot in reversed(tuple(self._snapshots.items())):
            if snapshot is None:
                try:
                    path.unlink()
                except FileNotFoundError:
                    pass
            else:
                self._restore(path, snapshot[0], snapshot[1])

    def commit(self) -> None:
        self._snapshots.clear()


@contextmanager
def file_transaction():
    """Serialize and atomically roll back one bounded multi-file update."""

    with _file_transaction_lock:
        transaction = FileTransaction()
        try:
            yield transaction
        except BaseException:
            try:
                transaction.rollback()
            except Exception as exc:
                raise VaultError("managed file rollback failed") from exc
            raise
        else:
            transaction.commit()


def _key_path() -> Path:
    return _safe_key_path(
        Path(
            os.environ.get(MASTER_KEY_FILE_ENV, "").strip()
            or _default_master_key_file
        )
    )


def _safe_key_path(path: Path) -> Path:
    """Return an absolute key path without following any symlink component."""

    candidate = Path(os.path.abspath(os.fspath(Path(path).expanduser())))
    for parent in reversed(candidate.parents):
        try:
            info = parent.lstat()
        except FileNotFoundError:
            continue
        except OSError as exc:
            raise VaultError("master key directory is unavailable") from exc
        if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
            raise VaultError("master key directory is not a regular directory")
    try:
        info = candidate.lstat()
    except FileNotFoundError:
        return candidate
    except OSError as exc:
        raise VaultError("master key file is unavailable") from exc
    if stat.S_ISLNK(info.st_mode):
        raise VaultError("master key file is not a regular file")
    return candidate


@contextmanager
def default_master_key_file(path: Path):
    """Use one process-local implicit key path for a bounded service lifetime."""

    global _default_master_key_file
    resolved = _safe_key_path(path)
    with _default_master_key_lock:
        previous = _default_master_key_file
        _default_master_key_file = resolved
    try:
        yield resolved
    finally:
        with _default_master_key_lock:
            _default_master_key_file = previous


def _validate_key(raw: str, source: str) -> str:
    try:
        Fernet(raw.encode("ascii"))
    except (UnicodeEncodeError, ValueError, TypeError) as exc:
        raise VaultError("%s is not a valid Fernet key" % source) from exc
    return raw


def _read_key_file(key_path: Path) -> str:
    key_path = _safe_key_path(key_path)
    try:
        info = key_path.lstat()
        if key_path.is_symlink() or not stat.S_ISREG(info.st_mode):
            raise VaultError("master key file is not a regular file")
        if os.name != "nt":
            current_uid = getattr(os, "getuid", lambda: info.st_uid)()
            if info.st_uid not in {0, current_uid}:
                raise VaultError("master key file is not owned by the current user")
            if info.st_mode & (stat.S_IRWXG | stat.S_IRWXO):
                raise VaultError("master key file must be private")
        raw = key_path.read_text(encoding="utf-8").strip()
    except FileNotFoundError:
        raise
    except OSError as exc:
        raise VaultError("master key file is unavailable") from exc
    return _validate_key(raw, "master key file")


def _create_key_file(key_path: Path) -> None:
    key_path = _safe_key_path(key_path)
    parent_existed = key_path.parent.exists()
    try:
        key_path.parent.mkdir(parents=True, exist_ok=True)
        _safe_key_path(key_path)
        if os.name != "nt" and not parent_existed:
            os.chmod(str(key_path.parent), 0o700)
    except OSError as exc:
        raise VaultError("master key directory is unavailable") from exc

    flags = os.O_WRONLY | os.O_CREAT | os.O_EXCL
    if hasattr(os, "O_BINARY"):
        flags |= os.O_BINARY
    if hasattr(os, "O_NOFOLLOW"):
        flags |= os.O_NOFOLLOW
    try:
        descriptor = os.open(str(key_path), flags, 0o600)
    except FileExistsError:
        return
    except OSError as exc:
        raise VaultError("master key file cannot be created") from exc

    try:
        if os.name != "nt" and hasattr(os, "fchmod"):
            os.fchmod(descriptor, 0o600)
        with os.fdopen(descriptor, "wb") as handle:
            descriptor = -1
            handle.write(Fernet.generate_key() + b"\n")
            handle.flush()
            os.fsync(handle.fileno())
    except OSError as exc:
        try:
            key_path.unlink()
        except OSError:
            pass
        raise VaultError("master key file cannot be created") from exc
    finally:
        if descriptor >= 0:
            os.close(descriptor)


def _master_key() -> str:
    raw = os.environ.get(MASTER_KEY_ENV, "").strip()
    if raw:
        return _validate_key(raw, MASTER_KEY_ENV)
    key_path = _key_path()
    try:
        return _read_key_file(key_path)
    except FileNotFoundError:
        _create_key_file(key_path)
        return _read_key_file(key_path)


def ensure_master_key() -> Optional[Path]:
    """Ensure a valid local key exists without exposing or replacing it."""

    if os.environ.get(MASTER_KEY_ENV, "").strip():
        _master_key()
        return None
    key_path = _key_path()
    _master_key()
    return key_path


def _fernet() -> Fernet:
    return Fernet(_master_key().encode("ascii"))


def _write(path: Path, value: bytes) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(str(path.parent), 0o700)
    fd, temporary = tempfile.mkstemp(prefix=".vault-", dir=str(path.parent))
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "wb") as handle:
            handle.write(value)
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def write_encrypted_bytes(path: Path, value: bytes) -> None:
    if not isinstance(value, bytes):
        raise VaultError("vault values must be bytes")
    _write(Path(path), _FORMAT + _fernet().encrypt(value))


def read_encrypted_bytes(path: Path) -> bytes:
    try:
        raw = Path(path).read_bytes()
    except OSError as exc:
        raise VaultError("encrypted credential file is unavailable") from exc
    if not raw.startswith(_FORMAT):
        raise VaultError("credential file is not encrypted with the supported format")
    try:
        return _fernet().decrypt(raw[len(_FORMAT):])
    except InvalidToken as exc:
        raise VaultError("credential file cannot be decrypted with the current master key") from exc


def write_encrypted_json(path: Path, value: Any) -> None:
    write_encrypted_bytes(
        Path(path),
        json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8"),
    )


def read_encrypted_json(path: Path) -> Any:
    try:
        return json.loads(read_encrypted_bytes(Path(path)).decode("utf-8"))
    except (UnicodeDecodeError, ValueError) as exc:
        raise VaultError("encrypted credential content is invalid JSON") from exc


def write_encrypted_text(path: Path, value: str) -> None:
    write_encrypted_bytes(Path(path), value.encode("utf-8"))


def read_encrypted_text(path: Path) -> str:
    try:
        return read_encrypted_bytes(Path(path)).decode("utf-8")
    except UnicodeDecodeError as exc:
        raise VaultError("encrypted credential content is not valid text") from exc
