"""Private Codex account credential storage and redacted metadata."""

from __future__ import annotations

import copy
import hmac
import json
import os
import re
import tempfile
from pathlib import Path
from typing import Any, Dict, Iterable, Optional, Set, Tuple

from .vault import VaultError, read_encrypted_json, write_encrypted_json

_SEGMENT = re.compile(r"^[A-Za-z0-9][A-Za-z0-9._-]{0,63}$")
_MAX_AUTH_BYTES = 1024 * 1024


class AccountError(ValueError):
    """Raised when a private account credential cannot be accepted."""


def account_root(config: Dict[str, Any], config_path: Optional[Path] = None) -> Path:
    value = str(config.get("account_store_path", "state/accounts"))
    path = Path(value).expanduser()
    if not path.is_absolute() and config_path is not None:
        path = Path(config_path).parent / path
    return path


def account_auth_path(
    config: Dict[str, Any], account_id: str, config_path: Optional[Path] = None
) -> Path:
    account_id = _segment(account_id, "account.id")
    return account_root(config, config_path).expanduser().resolve() / account_id / "auth.json.enc"


def canonicalize_account_paths(config: Dict[str, Any], config_path: Path) -> Dict[str, Any]:
    """Keep every configured account credential inside the derived account vault."""
    for account in config.get("accounts", []):
        raw_path = account.get("auth_file", "")
        if not raw_path:
            continue
        expected = account_auth_path(config, account["id"], config_path)
        actual = Path(raw_path).expanduser()
        if not actual.is_absolute():
            actual = Path(config_path).parent / actual
        if actual.resolve() != expected or actual.is_symlink():
            raise AccountError("account.auth_file must be managed inside the account store")
        account["auth_file"] = str(expected)
    return config


def _segment(value: Any, field: str) -> str:
    if not isinstance(value, str) or not _SEGMENT.fullmatch(value.strip()):
        raise AccountError("%s must be a safe single path segment" % field)
    return value.strip()


def _name(value: Any, fallback: str) -> str:
    if value is None:
        return fallback
    if not isinstance(value, str):
        raise AccountError("account.name must be a string")
    return value.strip() or fallback


def _validate_auth(auth: Any) -> Dict[str, Any]:
    if not isinstance(auth, dict):
        raise AccountError("auth_json must be a JSON object")
    encoded = json.dumps(auth, ensure_ascii=False).encode("utf-8")
    if len(encoded) > _MAX_AUTH_BYTES:
        raise AccountError("auth_json is too large")
    tokens = auth.get("tokens")
    access_token = tokens.get("access_token") if isinstance(tokens, dict) else auth.get("access_token")
    if not isinstance(access_token, str) or not access_token.strip():
        raise AccountError("auth_json does not contain a ChatGPT access token")
    return copy.deepcopy(auth)


def validate_auth_json(auth: Any) -> Dict[str, Any]:
    """Validate imported credential JSON without writing it."""
    return _validate_auth(auth)


def normalize_account(raw: Dict[str, Any]) -> Dict[str, Any]:
    if not isinstance(raw, dict):
        raise AccountError("each account must be an object")
    account_id = _segment(raw.get("id"), "account.id")
    prefix = _segment(raw.get("prefix"), "account.prefix")
    auth_file = raw.get("auth_file", "")
    if not isinstance(auth_file, str):
        raise AccountError("account.auth_file must be a string")
    return {
        "id": account_id,
        "name": _name(raw.get("name"), account_id),
        "prefix": prefix,
        "auth_file": auth_file.strip(),
        "enabled": bool(raw.get("enabled", True)),
        "quota": copy.deepcopy(raw.get("quota")) if isinstance(raw.get("quota"), dict) else None,
    }


def _private_text(path: Path, value: str) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    os.chmod(str(path.parent), 0o700)
    fd, temporary = tempfile.mkstemp(prefix=".account-", dir=str(path.parent))
    try:
        os.chmod(temporary, 0o600)
        with os.fdopen(fd, "w", encoding="utf-8") as handle:
            handle.write(value)
        os.replace(temporary, str(path))
    finally:
        if os.path.exists(temporary):
            os.unlink(temporary)


def import_account(
    config: Dict[str, Any], metadata: Dict[str, Any], auth_json: Dict[str, Any],
    config_path: Optional[Path] = None,
) -> Dict[str, Any]:
    account_id = _segment(metadata.get("id"), "account.id")
    prefix = _segment(metadata.get("prefix"), "account.prefix")
    auth = _validate_auth(auth_json)
    path = account_auth_path(config, account_id, config_path)
    try:
        write_encrypted_json(path, auth)
    except VaultError as exc:
        raise AccountError(str(exc)) from exc
    _private_text(path.parent / "config.toml", 'cli_auth_credentials_store = "file"\n')
    return normalize_account(
        {
            "id": account_id,
            "name": metadata.get("name", account_id),
            "prefix": prefix,
            "auth_file": str(path),
            "enabled": metadata.get("enabled", True),
        }
    )


def public_accounts(accounts: Iterable[Dict[str, Any]]) -> list:
    accounts = list(accounts)
    result = []
    duplicates = duplicate_account_status(accounts)
    for raw in accounts:
        account = normalize_account(raw)
        result.append(
            {
                "id": account["id"],
                "name": account["name"],
                "prefix": account["prefix"],
                "enabled": account["enabled"],
                "credential_set": bool(account["auth_file"]),
                "quota": account["quota"],
                "duplicate": account["id"] in duplicates,
                "duplicate_of": duplicates.get(account["id"], ""),
            }
        )
    return result


def load_auth(account: Dict[str, Any]) -> Dict[str, Any]:
    path = account.get("auth_file", "")
    if not isinstance(path, str) or not path:
        raise AccountError("credentials are not configured for account: %s" % account.get("id", ""))
    try:
        if Path(path).is_symlink():
            raise AccountError("stored account credentials cannot be a symlink")
        return _validate_auth(read_encrypted_json(Path(path)))
    except (OSError, VaultError, ValueError) as exc:
        raise AccountError("stored encrypted auth.json is invalid") from exc


def _auth_identities(auth: Dict[str, Any]) -> Set[Tuple[str, str]]:
    """Return stable in-memory identities without exposing credential values."""
    tokens = auth.get("tokens") if isinstance(auth.get("tokens"), dict) else auth
    identities: Set[Tuple[str, str]] = set()
    for source in (tokens, auth):
        for key in ("account_id", "chatgpt_account_id"):
            value = source.get(key) if isinstance(source, dict) else None
            if isinstance(value, str) and value.strip():
                identities.add(("account_id", value.strip()))
    access_token = tokens.get("access_token") if isinstance(tokens, dict) else None
    if isinstance(access_token, str) and access_token.strip():
        identities.add(("access_token", access_token.strip()))
    return identities


def _auth_file_identities(path: Path) -> Set[Tuple[str, str]]:
    try:
        with path.open("r", encoding="utf-8") as handle:
            value = json.load(handle)
        return _auth_identities(_validate_auth(value))
    except (OSError, ValueError, AccountError):
        return set()


def _stored_account_identities(account: Dict[str, Any]) -> Set[Tuple[str, str]]:
    try:
        return _auth_identities(load_auth(account))
    except AccountError:
        return set()


def codex_auth_path() -> Path:
    root = os.environ.get("CODEX_HOME")
    return (Path(root).expanduser() if root else Path.home() / ".codex") / "auth.json"


def valid_caller_authorization(value: str) -> bool:
    """Accept only the bearer token from the current Codex login."""
    if not isinstance(value, str) or not value.startswith("Bearer "):
        return False
    supplied = value[7:].strip()
    if not supplied:
        return False
    for kind, candidate in _auth_file_identities(codex_auth_path()):
        if kind == "access_token" and hmac.compare_digest(supplied, candidate):
            return True
    return False


def duplicate_account_status(accounts: Iterable[Dict[str, Any]]) -> Dict[str, str]:
    """Map duplicate account IDs to a safe human-readable source label."""
    seen: Dict[Tuple[str, str], str] = {}
    current = _auth_file_identities(codex_auth_path())
    for identity in current:
        seen[identity] = "当前 Codex 登录"

    duplicates: Dict[str, str] = {}
    for raw in accounts:
        account = normalize_account(raw)
        if not account["auth_file"]:
            continue
        identities = _stored_account_identities(account)
        if not identities:
            continue
        source = next((seen[item] for item in identities if item in seen), None)
        if source:
            duplicates[account["id"]] = source
            continue
        for identity in identities:
            seen[identity] = account["id"]
    return duplicates


def auth_headers(account: Dict[str, Any]) -> Dict[str, str]:
    auth = load_auth(account)
    tokens = auth.get("tokens") if isinstance(auth.get("tokens"), dict) else auth
    access_token = tokens.get("access_token", "")
    account_id = tokens.get("account_id") or auth.get("account_id", "")
    if not isinstance(access_token, str) or not access_token:
        raise AccountError("stored auth.json has no access token")
    result = {"Authorization": "Bearer " + access_token}
    if isinstance(account_id, str) and account_id:
        result["chatgpt-account-id"] = account_id
    return result
