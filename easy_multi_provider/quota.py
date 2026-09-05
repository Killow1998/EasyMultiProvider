"""Read Codex account quota through an isolated app-server process."""

from __future__ import annotations

import hashlib
import json
import os
import queue
import re
import shutil
import ssl
import subprocess
import stat
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, Optional

from . import __version__
from .accounts import AccountError, load_auth, load_native_auth, validate_auth_json
from .vault import VaultError, write_encrypted_json


class QuotaError(ValueError):
    """Raised when Codex cannot provide a safe quota snapshot."""

    def __init__(self, message: str, code: str = "quota_error"):
        super().__init__(message)
        self.code = code


def _quota_rpc_error(method: str, error: Any) -> QuotaError:
    # Codex's backend error includes URL and response body. Classify it here;
    # neither of those values may be returned to the browser or journal.
    message = error.get("message", "") if isinstance(error, dict) else ""
    message = message if isinstance(message, str) else ""
    auth_required = message in {
        "codex account authentication required to read rate limits",
        "chatgpt authentication required to read rate limits",
    }
    status = re.search(r" failed: (\d{3})\b[^;]*; content-type=", message.split("; body=", 1)[0])
    status_code = int(status[1]) if status else None
    if auth_required or status_code == 401:
        return QuotaError("Codex account needs sign-in; sign in again and re-import the account", "quota_auth_required")
    if status_code == 403:
        return QuotaError("Codex quota access was denied (403); check account access and network", "quota_access_denied")
    if status_code == 429:
        return QuotaError("Codex quota queries are rate limited (429); try again later", "quota_rate_limited")
    if method == "account/rateLimits/read":
        return QuotaError("Codex quota service query failed; check network connectivity and try again", "quota_fetch_failed")
    if method == "account/read":
        return QuotaError("Codex account read failed", "quota_account_read_failed")
    return QuotaError("Codex app-server initialization failed", "quota_initialize_failed")


# ponytail: one bounded lock is enough for the low-frequency quota path;
# replace with a bounded per-account pool only if measured quota contention matters.
_refresh_lock = threading.RLock()
_last_refresh = 0.0
_last_refresh_key = b""
_REFRESH_COOLDOWN_SECONDS = 2.0
_SERVER_AUTH_OID = "1.3.6.1.5.5.7.3.1"


def _trusted_codex_binary(codex_binary: str):
    binary_path = shutil.which(codex_binary) if not os.path.isabs(codex_binary) else codex_binary
    if not binary_path:
        raise QuotaError("Codex executable is unavailable")
    binary = Path(binary_path).resolve()
    try:
        target = binary.stat()
    except OSError as exc:
        raise QuotaError("Codex executable is unavailable") from exc
    current_uid = getattr(os, "getuid", lambda: None)()
    allowed_owners = {0, current_uid} if current_uid is not None else {target.st_uid}
    if (
        not stat.S_ISREG(target.st_mode)
        or not os.access(str(binary), os.X_OK)
        or target.st_uid not in allowed_owners
    ):
        raise QuotaError("Codex executable is not trusted")
    if os.name != "nt":
        for parent in (binary, *binary.parents):
            info = parent.stat()
            if info.st_mode & stat.S_IWOTH:
                raise QuotaError("Codex executable path is writable")
            if info.st_mode & stat.S_IWGRP and info.st_uid not in allowed_owners:
                raise QuotaError("Codex executable path is not owner-managed")
    return binary, (target.st_dev, target.st_ino)


def _verify_codex_binary(binary: Path, identity) -> None:
    try:
        verified, current_identity = _trusted_codex_binary(str(binary))
    except QuotaError as exc:
        raise QuotaError("Codex executable was replaced") from exc
    if verified != binary or current_identity != identity:
        raise QuotaError("Codex executable was replaced")


def _write_windows_root_ca_bundle(directory: Path) -> Optional[Path]:
    """Export Windows server-auth roots for an isolated Codex subprocess."""
    enum_certificates = getattr(ssl, "enum_certificates", None)
    if not callable(enum_certificates):
        return None
    try:
        certificates = enum_certificates("ROOT")
    except (OSError, ssl.SSLError):
        return None

    seen = set()
    pem_certificates = []
    for certificate, encoding, trust in certificates:
        if encoding != "x509_asn" or not isinstance(certificate, bytes):
            continue
        if trust is not True and (
            not isinstance(trust, (set, frozenset))
            or _SERVER_AUTH_OID not in trust
        ):
            continue
        if certificate in seen:
            continue
        try:
            pem = ssl.DER_cert_to_PEM_cert(certificate)
        except (ValueError, ssl.SSLError):
            continue
        seen.add(certificate)
        pem_certificates.append(pem if pem.endswith("\n") else pem + "\n")

    if not pem_certificates:
        return None
    bundle = directory / "windows-root-ca.pem"
    fd = -1
    try:
        fd = os.open(str(bundle), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        with os.fdopen(fd, "w", encoding="ascii") as handle:
            fd = -1
            handle.write("".join(pem_certificates))
        os.chmod(str(bundle), 0o600)
    except OSError:
        return None
    finally:
        if fd >= 0:
            os.close(fd)
    return bundle


def account_refresh_lock(account_id: Any) -> threading.RLock:
    return _refresh_lock


def _account_refresh_key(account: Dict[str, Any]) -> bytes:
    # Retain one digest instead of a raw, user-controlled account identifier.
    return hashlib.sha256(str(account.get("id", "")).encode("utf-8")).digest()


def refresh_account_quota(account: Dict[str, Any], codex_binary: str = "codex") -> Dict[str, Any]:
    """Serialize and cool down explicit and automatic account refreshes."""
    global _last_refresh, _last_refresh_key
    with account_refresh_lock(account.get("id")):
        now = time.monotonic()
        refresh_key = _account_refresh_key(account)
        if refresh_key == _last_refresh_key and now - _last_refresh < _REFRESH_COOLDOWN_SECONDS:
            raise QuotaError("account quota refresh is cooling down")
        result = read_account_quota(account, codex_binary=codex_binary)
        _last_refresh = time.monotonic()
        _last_refresh_key = refresh_key
        return result


def _enqueue_lines(stream: Any, output: Any) -> None:
    if stream is None:
        output.put(None)
        return
    try:
        for line in iter(stream.readline, ""):
            output.put(line)
    finally:
        output.put(None)


def _drain_lines(stream: Any) -> None:
    if stream is None:
        return
    for _line in iter(stream.readline, ""):
        pass


def _query_app_server(process: Any, requests: list, timeout: int) -> str:
    if process.stdin is None or process.stdout is None:
        raise QuotaError("Codex account quota check failed")

    output = queue.Queue()
    lines = []
    threading.Thread(
        target=_enqueue_lines, args=(process.stdout, output), daemon=True
    ).start()
    threading.Thread(
        target=_drain_lines, args=(process.stderr,), daemon=True
    ).start()
    deadline = time.monotonic() + timeout

    def send(request: Dict[str, Any]) -> None:
        try:
            process.stdin.write(json.dumps(request) + "\n")
            process.stdin.flush()
        except (OSError, ValueError) as exc:
            raise QuotaError("Codex account quota check failed") from exc

    def wait_for(request_id: int, method: str) -> None:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise QuotaError("Codex account quota check timed out", "quota_timeout")
            try:
                line = output.get(timeout=remaining)
            except queue.Empty as exc:
                raise QuotaError("Codex account quota check timed out", "quota_timeout") from exc
            if line is None:
                raise QuotaError("Codex did not return account rate limits")
            lines.append(line)
            try:
                message = json.loads(line)
            except ValueError:
                continue
            if message.get("id") != request_id:
                continue
            if "error" in message:
                raise _quota_rpc_error(method, message["error"])
            result = message.get("result")
            if method == "account/read" and isinstance(result, dict) and "account" in result and result["account"] is None:
                # Codex can return account:null after a permanent refresh
                # failure instead of exposing the refresh error over RPC.
                raise QuotaError("Codex account needs sign-in; sign in again and re-import the account", "quota_auth_required")
            return

    try:
        # Keep stdin open and sequence account/rate-limit calls. An isolated
        # app-server may still be completing account refresh when it receives
        # the rate-limit request; the shared Codex home only hid this race.
        send(requests[0])
        wait_for(1, "initialize")
        send(requests[1])
        send(requests[2])
        wait_for(2, "account/read")
        send(requests[3])
        wait_for(3, "account/rateLimits/read")
    finally:
        try:
            process.stdin.close()
        except (OSError, ValueError):
            pass
        try:
            process.wait(timeout=2)
        except (OSError, subprocess.TimeoutExpired, ValueError):
            try:
                process.kill()
                process.wait(timeout=2)
            except (OSError, subprocess.TimeoutExpired, ValueError):
                pass

    if process.returncode not in (0, None):
        raise QuotaError("Codex account quota check failed")
    return "".join(lines)


def _mask_email(value: Any) -> str:
    if not isinstance(value, str) or "@" not in value:
        return ""
    local, domain = value.split("@", 1)
    return (local[:1] or "*") + "***@" + domain


def _safe_fields(source: Dict[str, Any], names: tuple) -> Dict[str, Any]:
    return {
        name: source[name]
        for name in names
        if name in source
        and (source[name] is None or isinstance(source[name], (str, int, float, bool)))
    }


def _safe_reset_credits(value: Any) -> Any:
    if not isinstance(value, dict):
        return None
    snapshot = {}
    if isinstance(value.get("availableCount"), int) and not isinstance(value.get("availableCount"), bool):
        snapshot["available_count"] = value["availableCount"]
    if "credits" in value:
        details = value["credits"]
        if details is None:
            snapshot["credits"] = None
        elif isinstance(details, list):
            snapshot["credits"] = [
                {
                    target: item[source]
                    for source, target in (
                        ("resetType", "reset_type"),
                        ("status", "status"),
                        ("grantedAt", "granted_at"),
                        ("expiresAt", "expires_at"),
                        ("title", "title"),
                        ("description", "description"),
                    )
                    if source in item
                    and (
                        item[source] is None
                        or isinstance(item[source], (str, int, float, bool))
                    )
                }
                for item in details
                if isinstance(item, dict)
            ]
    return snapshot or None


def _safe_credit_snapshot(rate_limits: Dict[str, Any], result: Dict[str, Any]) -> Any:
    snapshot = {}
    credits = rate_limits.get("credits")
    if isinstance(credits, dict):
        fields = _safe_fields(credits, ("hasCredits", "unlimited", "balance"))
        snapshot.update(
            {
                target: fields[source]
                for source, target in (
                    ("hasCredits", "has_credits"),
                    ("unlimited", "unlimited"),
                    ("balance", "balance"),
                )
                if source in fields
            }
        )

    individual_limit = rate_limits.get("individualLimit")
    if isinstance(individual_limit, dict):
        fields = _safe_fields(individual_limit, ("limit", "used", "remainingPercent", "resetsAt"))
        if fields:
            snapshot["individual_limit"] = {
                "limit": fields.get("limit"),
                "used": fields.get("used"),
                "remaining_percent": fields.get("remainingPercent"),
                "resets_at": fields.get("resetsAt"),
            }
            snapshot["individual_limit"] = {
                key: value for key, value in snapshot["individual_limit"].items() if value is not None
            }

    spend_control = rate_limits.get("spendControlReached")
    if isinstance(spend_control, bool):
        snapshot["spend_control_reached"] = spend_control

    reset_snapshot = _safe_reset_credits(result.get("rateLimitResetCredits"))
    if reset_snapshot is not None:
        snapshot["reset_credits"] = reset_snapshot
    return snapshot or None


def parse_app_server_output(output: str) -> Dict[str, Any]:
    account = {}
    rate_limits = None
    rate_limits_result = {}
    buckets = {}

    def read_limits(payload):
        nonlocal rate_limits, rate_limits_result, buckets
        if not isinstance(payload, dict):
            return
        by_id = payload.get("rateLimitsByLimitId")
        if isinstance(by_id, dict):
            current = {key: value for key, value in by_id.items()
                       if isinstance(key, str) and key and isinstance(value, dict)}
            if current:
                buckets = current
                rate_limits = buckets.get("codex", next(iter(buckets.values())))
                rate_limits_result = payload
                return
        candidate = payload.get("rateLimits")
        if isinstance(candidate, dict):
            limit_id = candidate.get("limitId") or candidate.get("limit_id") or "codex"
            buckets[str(limit_id)] = candidate
            rate_limits = buckets.get("codex", candidate)
            rate_limits_result = payload
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        result = message.get("result")
        if isinstance(result, dict):
            if isinstance(result.get("account"), dict):
                account = result["account"]
            read_limits(result)

        if message.get("method") == "account/rateLimits/updated":
            read_limits(message.get("params"))
    if rate_limits is None:
        raise QuotaError("Codex did not return account rate limits")
    return {
        "account_label": _mask_email(account.get("email")),
        "plan_type": (
            account.get("planType")
            if isinstance(account.get("planType"), str)
            else rate_limits.get("planType")
            if isinstance(rate_limits.get("planType"), str)
            else None
        ),
        "rate_limits": rate_limits,
        "rate_limits_by_limit_id": buckets,
        "credits": _safe_credit_snapshot(rate_limits, rate_limits_result),
        "updated_at": int(time.time()),
    }


def _run_quota_query(
    auth: Dict[str, Any],
    codex_binary: str,
    timeout: int,
    allow_refresh: bool,
    persist_path: Optional[Path],
) -> Dict[str, Any]:
    """Run the isolated app-server quota query against a validated auth object.

    Policy flags keep the imported-account and native-login callers explicit:
    - allow_refresh: whether account/read may request token rotation.
    - persist_path: when set and allow_refresh is True, a refreshed credential
      is persisted to this encrypted EMP path; when None, temporary refreshed
      state is discarded and the native auth file is never mutated.
    """
    binary, identity = _trusted_codex_binary(codex_binary)
    requests = [
        {
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "easy-multi-provider",
                    "title": "EMP",
                    "version": __version__,
                },
                "capabilities": {},
            },
        },
        {"method": "initialized"},
        {"id": 2, "method": "account/read", "params": {"refreshToken": allow_refresh}},
        {"id": 3, "method": "account/rateLimits/read", "params": None},
    ]
    with tempfile.TemporaryDirectory(prefix="easy-mp-codex-account-") as temporary:
        codex_home = Path(temporary)
        codex_home.chmod(0o700)
        plain_auth = codex_home / "auth.json"
        fd = os.open(str(plain_auth), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="utf-8") as handle:
                json.dump(auth, handle, ensure_ascii=False)
        finally:
            fd = -1
        (codex_home / "config.toml").write_text(
            'cli_auth_credentials_store = "file"\n', encoding="utf-8"
        )
        os.chmod(str(codex_home / "config.toml"), 0o600)
        env = {
            "CODEX_HOME": str(codex_home),
            "HOME": str(Path.home()),
            "PATH": os.environ.get("PATH", ""),
        }
        # Keep only process settings needed by Codex and network/TLS proxy
        # settings. Do not inherit API keys or unrelated host state.
        for key in (
            # Windows networking needs this even for an otherwise isolated env.
            "SYSTEMROOT",
            "LANG",
            "LC_ALL",
            "TERM",
            "HTTP_PROXY",
            "HTTPS_PROXY",
            "ALL_PROXY",
            "NO_PROXY",
            "http_proxy",
            "https_proxy",
            "all_proxy",
            "no_proxy",
            "SSL_CERT_FILE",
            "SSL_CERT_DIR",
            "CODEX_CA_CERTIFICATE",
            "NODE_EXTRA_CA_CERTS",
        ):
            if os.environ.get(key):
                env[key] = os.environ[key]
        if os.name == "nt" and not env.get("SSL_CERT_FILE") and not env.get(
            "CODEX_CA_CERTIFICATE"
        ):
            windows_ca_bundle = _write_windows_root_ca_bundle(codex_home)
            if windows_ca_bundle is not None:
                env["SSL_CERT_FILE"] = str(windows_ca_bundle)
        process = None
        try:
            _verify_codex_binary(binary, identity)
            process = subprocess.Popen(
                [str(binary), "app-server", "--stdio"],
                cwd=str(codex_home),
                env=env,
                stdin=subprocess.PIPE,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                text=True,
            )
            stdout = _query_app_server(process, requests, timeout)
        except (OSError, subprocess.TimeoutExpired) as exc:
            if process is not None:
                try:
                    process.kill()
                    process.communicate()
                except (OSError, ValueError):
                    pass
            raise QuotaError("Codex account quota check failed") from exc
        finally:
            # Token rotation and quota retrieval are separate operations. Keep
            # Codex's refreshed credential even if the later query fails, or
            # the vault can retain a refresh token that has already been used.
            if process is not None and allow_refresh and persist_path is not None:
                try:
                    refreshed_auth = json.loads(plain_auth.read_text(encoding="utf-8"))
                    write_encrypted_json(Path(persist_path), validate_auth_json(refreshed_auth))
                except (OSError, ValueError, VaultError) as exc:
                    raise QuotaError("Codex refreshed credentials could not be saved", "quota_credentials_save_failed") from exc
        return parse_app_server_output(stdout)


def read_account_quota(account: Dict[str, Any], codex_binary: str = "codex", timeout: int = 45) -> Dict[str, Any]:
    """Query quota for a normal imported EMP account.

    Uses the account's encrypted EMP credential snapshot, may request token
    refresh, and persists a validated refreshed credential back into the EMP
    encrypted vault.
    """
    auth_file = account.get("auth_file", "")
    if not auth_file:
        raise QuotaError("account credentials are not configured")
    try:
        auth = load_auth(account)
    except (AccountError, VaultError) as exc:
        raise QuotaError(str(exc)) from exc
    return _run_quota_query(
        auth,
        codex_binary,
        timeout,
        allow_refresh=True,
        persist_path=Path(auth_file),
    )


def read_native_login_quota(
    codex_binary: str = "codex",
    timeout: int = 45,
    auth_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Query quota for an account that duplicates the current native Codex login.

    Uses the live native auth.json (the authoritative source when tokens have
    rotated away from a stale EMP snapshot). Does not request token rotation
    (refreshToken: False) and never writes, persists, or mutates the native
    auth file or any EMP encrypted credential.
    """
    try:
        auth = load_native_auth(auth_path)
    except AccountError as exc:
        raise QuotaError(str(exc)) from exc
    return _run_quota_query(
        auth,
        codex_binary,
        timeout,
        allow_refresh=False,
        persist_path=None,
    )
