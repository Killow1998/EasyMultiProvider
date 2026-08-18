"""Read Codex account quota through an isolated app-server process."""

from __future__ import annotations

import json
import os
import queue
import subprocess
import tempfile
import threading
import time
from pathlib import Path
from typing import Any, Dict

from .accounts import AccountError, load_auth
from .vault import VaultError, write_encrypted_json

class QuotaError(ValueError):
    """Raised when Codex cannot provide a safe quota snapshot."""


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
                raise QuotaError("Codex account quota check failed")
            return

    try:
        # Keep stdin open and sequence account/rate-limit calls. app-server can
        # need the account response before it starts the rate-limit request.
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
    for line in output.splitlines():
        try:
            message = json.loads(line)
        except ValueError:
            continue
        result = message.get("result")
        if not isinstance(result, dict):
            continue
        if isinstance(result.get("account"), dict):
            account = result["account"]
        if isinstance(result.get("rateLimits"), dict):
            rate_limits = result["rateLimits"]
            rate_limits_result = result
    if rate_limits is None:
        raise QuotaError("Codex did not return account rate limits")
    return {
        "account_label": _mask_email(account.get("email")),
        "plan_type": account.get("planType") if isinstance(account.get("planType"), str) else None,
        "rate_limits": rate_limits,
        "credits": _safe_credit_snapshot(rate_limits, rate_limits_result),
        "updated_at": int(time.time()),
    }


def read_account_quota(account: Dict[str, Any], codex_binary: str = "codex", timeout: int = 45) -> Dict[str, Any]:
    auth_file = account.get("auth_file", "")
    if not auth_file:
        raise QuotaError("account credentials are not configured")
    try:
        auth = load_auth(account)
    except (AccountError, VaultError) as exc:
        raise QuotaError("account credentials are unavailable") from exc
    requests = [
        {
            "id": 1,
            "method": "initialize",
            "params": {
                "clientInfo": {
                    "name": "easy-multi-provider",
                    "title": "EasyMultiProvider",
                    "version": "0.1.0",
                },
                "capabilities": {},
            },
        },
        {"method": "initialized"},
        {"id": 2, "method": "account/read", "params": {"refreshToken": True}},
        {"id": 3, "method": "account/rateLimits/read"},
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
        env = os.environ.copy()
        env["CODEX_HOME"] = str(codex_home)
        process = None
        try:
            process = subprocess.Popen(
                [codex_binary, "app-server", "--stdio"],
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
        try:
            refreshed_auth = json.loads(plain_auth.read_text(encoding="utf-8"))
            write_encrypted_json(Path(auth_file), _validate_refreshed_auth(refreshed_auth))
        except (OSError, ValueError, VaultError) as exc:
            raise QuotaError("Codex returned invalid account credentials") from exc
        return parse_app_server_output(stdout)


def _validate_refreshed_auth(value: Any) -> Dict[str, Any]:
    if not isinstance(value, dict):
        raise ValueError("auth.json must be an object")
    return value
