"""Small encrypted-at-rest vault for local credentials."""

from __future__ import annotations

import json
import os
import tempfile
from pathlib import Path
from typing import Any

from cryptography.fernet import Fernet, InvalidToken


MASTER_KEY_ENV = "EASY_MULTI_PROVIDER_MASTER_KEY"
_FORMAT = b"easy-multi-provider-v1\n"


class VaultError(ValueError):
    """Raised when encrypted credentials cannot be read or written safely."""


def _fernet() -> Fernet:
    raw = os.environ.get(MASTER_KEY_ENV, "").strip()
    if not raw:
        raise VaultError("set %s before using encrypted credentials" % MASTER_KEY_ENV)
    try:
        return Fernet(raw.encode("ascii"))
    except (UnicodeEncodeError, ValueError, TypeError) as exc:
        raise VaultError("%s is not a valid Fernet key" % MASTER_KEY_ENV) from exc


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
