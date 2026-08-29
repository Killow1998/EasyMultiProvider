"""Read Codex account quota through an isolated app-server process."""

from __future__ import annotations

import hashlib
import json
import os
import queue
import shutil
import ssl
import subprocess
import stat
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.error import HTTPError, URLError
from urllib.request import HTTPSHandler, ProxyHandler, Request, build_opener

from . import __version__
from .accounts import AccountError, load_auth, load_native_auth
from .vault import VaultError, write_encrypted_json

class QuotaError(ValueError):
    """Raised when Codex cannot provide a safe quota snapshot."""


class _CodexRequestError(QuotaError):
    """An app-server request error that can be handled by a safe retry."""

    def __init__(self, request_id: Any, code: Any, detail: Any) -> None:
        self.request_id = request_id
        self.code = code
        self.detail = str(detail or "")
        super().__init__("Codex account quota check failed")


# ponytail: one bounded lock is enough for the low-frequency quota path;
# replace with a bounded per-account pool only if measured quota contention matters.
_refresh_lock = threading.RLock()
_last_refresh = 0.0
_last_refresh_key = b""
_REFRESH_COOLDOWN_SECONDS = 2.0
_CODEX_USAGE_URL = "https://chatgpt.com/backend-api/wham/usage"
_MAX_USAGE_RESPONSE_BYTES = 1024 * 1024


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


_SERVER_AUTH_OID = "1.3.6.1.5.5.7.3.1"


def _write_windows_root_ca_bundle(directory: Path) -> Optional[Path]:
    """Export Windows TLS roots for clients that do not read the system store."""
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
            not isinstance(trust, (set, frozenset)) or _SERVER_AUTH_OID not in trust
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
    try:
        fd = os.open(str(bundle), os.O_WRONLY | os.O_CREAT | os.O_TRUNC, 0o600)
        try:
            with os.fdopen(fd, "w", encoding="ascii") as handle:
                fd = -1
                handle.write("".join(pem_certificates))
        finally:
            if fd >= 0:
                os.close(fd)
        os.chmod(str(bundle), 0o600)
    except OSError:
        return None
    return bundle


def account_refresh_lock(account_id: Any) -> threading.RLock:
    return _refresh_lock


def _account_refresh_key(account: Dict[str, Any]) -> bytes:
    # Retain one digest instead of a raw, user-controlled account identifier.
    return hashlib.sha256(str(account.get("id", "")).encode("utf-8")).digest()


def refresh_account_quota(account: Dict[str, Any]) -> Dict[str, Any]:
    """Serialize and cool down explicit and automatic account refreshes."""
    global _last_refresh, _last_refresh_key
    with account_refresh_lock(account.get("id")):
        now = time.monotonic()
        refresh_key = _account_refresh_key(account)
        if refresh_key == _last_refresh_key and now - _last_refresh < _REFRESH_COOLDOWN_SECONDS:
            raise QuotaError("account quota refresh is cooling down")
        result = read_account_quota(account)
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

    def wait_for(request_id: int) -> None:
        while True:
            remaining = deadline - time.monotonic()
            if remaining <= 0:
                raise QuotaError("Codex account quota check timed out")
            try:
                line = output.get(timeout=remaining)
            except queue.Empty as exc:
                raise QuotaError("Codex account quota check timed out") from exc
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
                error = message.get("error")
                if isinstance(error, dict):
                    raise _CodexRequestError(
                        request_id,
                        error.get("code"),
                        error.get("message"),
                    )
                raise _CodexRequestError(request_id, None, "")
            return

    try:
        # Keep stdin open and sequence account/rate-limit calls. An isolated
        # app-server may still be completing account refresh when it receives
        # the rate-limit request; the shared Codex home only hid this race.
        send(requests[0])
        wait_for(1)
        send(requests[1])
        send(requests[2])
        wait_for(2)
        send(requests[3])
        wait_for(3)
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

    def select_rate_limits(source: Dict[str, Any]) -> Optional[Dict[str, Any]]:
        candidate = source.get("rateLimits")
        if isinstance(candidate, dict):
            return candidate
        by_limit_id = source.get("rateLimitsByLimitId")
        if not isinstance(by_limit_id, dict):
            return None
        codex_limit = by_limit_id.get("codex")
        if isinstance(codex_limit, dict):
            return codex_limit
        for value in by_limit_id.values():
            if isinstance(value, dict):
                return value
        return None

    for line in output.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        result = message.get("result")
        if isinstance(result, dict):
            if isinstance(result.get("account"), dict):
                account = result["account"]
            selected = select_rate_limits(result)
            if selected is not None:
                rate_limits = selected
                rate_limits_result = result

        params = message.get("params")
        if message.get("method") == "account/rateLimits/updated" and isinstance(params, dict):
            selected = select_rate_limits(params)
            if selected is not None:
                rate_limits = selected
                rate_limits_result = params

    if rate_limits is None:
        raise QuotaError("Codex did not return account rate limits")
    plan_type = account.get("planType")
    if not isinstance(plan_type, str) and isinstance(rate_limits.get("planType"), str):
        plan_type = rate_limits["planType"]
    return {
        "account_label": _mask_email(account.get("email")),
        "plan_type": plan_type if isinstance(plan_type, str) else None,
        "rate_limits": rate_limits,
        "credits": _safe_credit_snapshot(rate_limits, rate_limits_result),
        "updated_at": int(time.time()),
    }


def _number(value: Any) -> Optional[float]:
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        return value
    return None


def _direct_rate_window(value: Any) -> Optional[Dict[str, Any]]:
    if not isinstance(value, dict):
        return None
    result = {}
    used_percent = _number(value.get("used_percent"))
    if used_percent is not None:
        result["usedPercent"] = used_percent
    duration_seconds = _number(value.get("limit_window_seconds"))
    if duration_seconds is not None:
        result["windowDurationMins"] = duration_seconds / 60
    resets_at = value.get("reset_at")
    if isinstance(resets_at, (str, int, float)) and not isinstance(resets_at, bool):
        result["resetsAt"] = resets_at
    return result or None


def _safe_direct_reset_credits(value: Any) -> Any:
    if not isinstance(value, dict):
        return None
    snapshot = {}
    available = value.get("available_count")
    if isinstance(available, int) and not isinstance(available, bool):
        snapshot["available_count"] = available
    details = value.get("credits")
    if details is None and "credits" in value:
        snapshot["credits"] = None
    elif isinstance(details, list):
        snapshot["credits"] = [
            {
                key: item[key]
                for key in (
                    "reset_type",
                    "status",
                    "granted_at",
                    "expires_at",
                    "title",
                    "description",
                )
                if key in item
                and (
                    item[key] is None
                    or isinstance(item[key], (str, int, float, bool))
                )
            }
            for item in details
            if isinstance(item, dict)
        ]
    return snapshot or None


def parse_direct_usage_payload(payload: Any) -> Dict[str, Any]:
    """Normalize the bounded, read-only wham/usage response for the EMP UI."""
    if not isinstance(payload, dict):
        raise QuotaError("Codex account quota check failed")
    source = payload.get("rate_limit")
    if not isinstance(source, dict):
        raise QuotaError("Codex did not return account rate limits")
    rate_limits = {
        "primary": _direct_rate_window(source.get("primary_window")),
        "secondary": _direct_rate_window(source.get("secondary_window")),
    }
    if not rate_limits["primary"] and not rate_limits["secondary"]:
        raise QuotaError("Codex did not return account rate limits")

    credit_snapshot = {}
    credits = payload.get("credits")
    if isinstance(credits, dict):
        for source_name, target_name in (
            ("has_credits", "has_credits"),
            ("unlimited", "unlimited"),
            ("balance", "balance"),
        ):
            value = credits.get(source_name)
            if value is None or isinstance(value, (str, int, float, bool)):
                if source_name in credits:
                    credit_snapshot[target_name] = value

    reset_credits = _safe_direct_reset_credits(payload.get("rate_limit_reset_credits"))
    if reset_credits is not None:
        credit_snapshot["reset_credits"] = reset_credits
    spend_control = payload.get("spend_control")
    if isinstance(spend_control, bool):
        credit_snapshot["spend_control_reached"] = spend_control
    elif isinstance(spend_control, dict):
        reached = spend_control.get("spend_control_reached")
        if isinstance(reached, bool):
            credit_snapshot["spend_control_reached"] = reached

    plan_type = payload.get("plan_type")
    return {
        "account_label": _mask_email(payload.get("email")),
        "plan_type": plan_type if isinstance(plan_type, str) else None,
        "rate_limits": rate_limits,
        "credits": credit_snapshot or None,
        "updated_at": int(time.time()),
    }


def _direct_usage_credentials(auth: Dict[str, Any]) -> tuple[str, str]:
    tokens = auth.get("tokens")
    source = tokens if isinstance(tokens, dict) else auth
    access_token = source.get("access_token")
    account_id = source.get("account_id") or source.get("chatgpt_account_id")
    if not isinstance(account_id, str) or not account_id.strip():
        account_id = auth.get("account_id")
    if (
        not isinstance(access_token, str)
        or not access_token.strip()
        or not isinstance(account_id, str)
        or not account_id.strip()
    ):
        raise QuotaError("Codex account credentials are incomplete")
    return access_token.strip(), account_id.strip()


def _direct_usage_ssl_context(directory: Path) -> ssl.SSLContext:
    configured_bundle = os.environ.get("SSL_CERT_FILE")
    if configured_bundle:
        try:
            return ssl.create_default_context(cafile=configured_bundle)
        except (OSError, ssl.SSLError):
            pass
    if os.name == "nt":
        windows_bundle = _write_windows_root_ca_bundle(directory)
        if windows_bundle is not None:
            try:
                return ssl.create_default_context(cafile=str(windows_bundle))
            except (OSError, ssl.SSLError):
                pass
    return ssl.create_default_context()


def _read_direct_chatgpt_quota(auth: Dict[str, Any], timeout: int) -> Dict[str, Any]:
    """Read wham/usage without exposing, logging, refreshing, or persisting auth."""
    access_token, account_id = _direct_usage_credentials(auth)
    request = Request(
        _CODEX_USAGE_URL,
        headers={
            "Authorization": "Bearer " + access_token,
            "ChatGPT-Account-Id": account_id,
            "Accept": "application/json",
            "User-Agent": "EasyMultiProvider/" + __version__,
        },
        method="GET",
    )
    try:
        with tempfile.TemporaryDirectory(prefix="easy-mp-quota-tls-") as temporary:
            context = _direct_usage_ssl_context(Path(temporary))
            # ProxyHandler reads the same HTTP(S)_PROXY environment configured
            # for EMP; HTTPSHandler keeps certificate verification enabled.
            opener = build_opener(ProxyHandler(), HTTPSHandler(context=context))
            with opener.open(request, timeout=timeout) as response:
                status_code = getattr(response, "status", response.getcode())
                if status_code != 200:
                    raise QuotaError("Codex account quota check failed")
                encoded = response.read(_MAX_USAGE_RESPONSE_BYTES + 1)
        if len(encoded) > _MAX_USAGE_RESPONSE_BYTES:
            raise QuotaError("Codex account quota response is too large")
        payload = json.loads(encoded.decode("utf-8"))
    except QuotaError:
        raise
    except (HTTPError, URLError, OSError, ssl.SSLError, UnicodeError, ValueError) as exc:
        # Never include HTTP bodies, headers, URLs with query data, or tokens in
        # the outward error. urllib does not log request headers by default.
        raise QuotaError("Codex account quota check failed") from exc
    return parse_direct_usage_payload(payload)


def _is_codex_usage_transport_error(exc: _CodexRequestError) -> bool:
    detail = exc.detail.lower()
    return (
        exc.request_id == 3
        and exc.code == -32603
        and (
            "failed to fetch codex rate limits" in detail
            or "backend-api/wham/usage" in detail
        )
    )


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
                    "title": "EasyMultiProvider",
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
            "NODE_EXTRA_CA_CERTS",
        ):
            if os.environ.get(key):
                env[key] = os.environ[key]
        # Codex 0.151 on Windows may not load the Windows trusted-root store,
        # producing UnknownIssuer for an otherwise valid ChatGPT certificate
        # chain. Keep an explicit operator-provided bundle when present;
        # otherwise export the current Windows TLS roots into the isolated
        # temporary home used only by this quota subprocess.
        if os.name == "nt" and not env.get("SSL_CERT_FILE"):
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
        if process.returncode not in (0, None):
            raise QuotaError("Codex account quota check failed")
        if allow_refresh and persist_path is not None:
            try:
                refreshed_auth = json.loads(plain_auth.read_text(encoding="utf-8"))
                write_encrypted_json(Path(persist_path), _validate_refreshed_auth(refreshed_auth))
            except (OSError, ValueError, VaultError) as exc:
                raise QuotaError("Codex returned invalid account credentials") from exc
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
    try:
        return _run_quota_query(
            auth,
            codex_binary,
            timeout,
            allow_refresh=True,
            persist_path=Path(auth_file),
        )
    except _CodexRequestError as exc:
        if _is_codex_usage_transport_error(exc):
            return _read_direct_chatgpt_quota(auth, timeout)
        if exc.request_id != 2:
            raise
        # Codex 0.151 treats imported ChatGPT credentials as external auth and
        # may reject the proactive refresh request. Retry with the current
        # access token only; this path never writes temporary auth back to the
        # EMP vault, so a failed refresh cannot corrupt the account.
        try:
            return _run_quota_query(
                auth,
                codex_binary,
                timeout,
                allow_refresh=False,
                persist_path=None,
            )
        except _CodexRequestError as retry_exc:
            if _is_codex_usage_transport_error(retry_exc):
                return _read_direct_chatgpt_quota(auth, timeout)
            raise


def read_native_login_quota(codex_binary: str = "codex", timeout: int = 45) -> Dict[str, Any]:
    """Query quota for an account that duplicates the current native Codex login.

    Uses the live native auth.json (the authoritative source when tokens have
    rotated away from a stale EMP snapshot). Does not request token rotation
    (refreshToken: False) and never writes, persists, or mutates the native
    auth file or any EMP encrypted credential.
    """
    try:
        auth = load_native_auth()
    except AccountError as exc:
        raise QuotaError(str(exc)) from exc
    try:
        return _run_quota_query(
            auth,
            codex_binary,
            timeout,
            allow_refresh=False,
            persist_path=None,
        )
    except _CodexRequestError as exc:
        if _is_codex_usage_transport_error(exc):
            return _read_direct_chatgpt_quota(auth, timeout)
        raise


def _validate_refreshed_auth(value: Any) -> Dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError("auth.json must be an object")
    return value
