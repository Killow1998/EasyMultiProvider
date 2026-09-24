"""Small, allowlisted support snapshot for the local management UI."""

from __future__ import annotations

import datetime
import os
import re
import sys
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import urlsplit

from . import __version__
from .accounts import NATIVE_ACCOUNT_ID
from .main import resolve_desktop_config_path
from .network_proxy import proxy_for_url


_COMPATIBILITY = frozenset(("recommended", "supported", "unverified", "unsupported", "unavailable", "unknown"))
_RUNTIME_SOURCES = frozenset(("configured", "codex_app", "managed", "vscode", "vscode_insiders", "cursor", "path_cli"))
_CREDENTIAL_STATES = frozenset(("valid", "invalid", "unknown"))
_PROXY_SOURCES = frozenset(("environment", "system", "direct"))
_PROXY_SCHEMES = frozenset(("http", "https", "socks4", "socks4a", "socks5", "socks5h"))
_VERSION = re.compile(r"\d{1,4}\.\d{1,4}\.\d{1,6}(?:-[A-Za-z0-9.-]{1,32})?(?:\+[A-Za-z0-9.-]{1,32})?\Z")


def _choice(value: Any, allowed: frozenset, fallback: str = "unknown") -> str:
    return value if isinstance(value, str) and value in allowed else fallback


def _version(value: Any) -> Optional[str]:
    return value if isinstance(value, str) and _VERSION.fullmatch(value) else None


def _configuration(path: Path) -> Dict[str, Any]:
    absolute = Path(os.path.abspath(os.fspath(path.expanduser())))
    home = Path(os.path.abspath(os.fspath(Path.home())))
    desktop = Path(os.path.abspath(os.fspath(resolve_desktop_config_path())))
    current = Path(os.path.abspath(os.fspath(Path.cwd() / "config.json")))
    if absolute == desktop:
        location = "desktop_default"
        if sys.platform == "win32":
            display = "<user-config>/EasyMultiProvider/config.json"
        elif sys.platform == "darwin":
            display = "~/Library/Application Support/EasyMultiProvider/config.json"
        else:
            xdg = os.environ.get("XDG_CONFIG_HOME", "").strip()
            root = Path(os.path.abspath(os.fspath(Path(xdg).expanduser()))) if xdg else home / ".config"
            display = (
                "~/.config/easy-multi-provider/config.json"
                if root == home / ".config"
                else "<xdg-config>/easy-multi-provider/config.json"
            )
    elif absolute == current:
        location = "working_directory"
        display = "./config.json"
    else:
        location = "custom"
        try:
            under_home = os.path.commonpath((str(absolute), str(home))) == str(home)
        except ValueError:
            under_home = False
        filename = "config.json" if absolute.name == "config.json" else "<custom-file>"
        display = ("~/<custom>/" if under_home else "<custom>/") + filename
    try:
        exists = absolute.is_file()
        writable = os.access(absolute if exists else absolute.parent, os.W_OK)
        write_access = "allowed" if writable else "denied"
    except OSError:
        exists = False
        write_access = "unknown"
    return {
        "path": display,
        "location": location,
        "exists": exists,
        "write_access_hint": write_access,
    }


def _runtime(state: Any) -> Dict[str, Any]:
    try:
        compatibility = state.codex_compatibility_snapshot()
    except (OSError, ValueError):
        compatibility = {}
    try:
        sync = state.runtime_sync_snapshot()
    except (OSError, ValueError):
        sync = {}
    if not isinstance(compatibility, dict):
        compatibility = {}
    if not isinstance(sync, dict):
        sync = {}
    inventory = compatibility.get("runtimes")
    items = []
    for item in inventory[:16] if isinstance(inventory, list) else []:
        if not isinstance(item, dict):
            continue
        items.append({
            "version": _version(item.get("installed")),
            "compatibility": _choice(item.get("status"), _COMPATIBILITY),
            "source": _choice(item.get("source"), _RUNTIME_SOURCES),
            "selected": item.get("helper") is True,
            "targeted": item.get("targeted") is True,
        })
    return {
        "version": _version(compatibility.get("installed")),
        "compatibility": _choice(compatibility.get("status"), _COMPATIBILITY),
        "source": _choice(compatibility.get("source"), _RUNTIME_SOURCES),
        "state": _choice(sync.get("state"), frozenset((
            "not_checked", "catalog_unverified", "reload_required", "stopping",
            "emp_loaded", "native_loaded", "stopped_waiting_for_start", "stop_failed",
            "verification_failed", "unsupported",
        ))),
        "target": _choice(sync.get("target"), frozenset(("emp", "native"))),
        "verified": sync.get("verified") is True,
        "inventory": items,
        "inventory_truncated": isinstance(inventory, list) and len(inventory) > 16,
    }


def _network(proxy_source: Any) -> Dict[str, Any]:
    try:
        proxy = proxy_for_url("https://chatgpt.com")
    except (OSError, TypeError, ValueError):
        proxy = None
        route = "unknown"
    else:
        route = "proxy" if proxy else "direct"
    try:
        scheme = urlsplit(proxy).scheme.lower() if isinstance(proxy, str) else ""
    except ValueError:
        scheme = ""
    return {
        "source_at_startup": _choice(proxy_source, _PROXY_SOURCES),
        "chatgpt_route": route,
        "proxy_scheme": _choice(scheme, _PROXY_SCHEMES) if proxy else None,
        "connectivity_probe": "not_run",
    }


def _quota_status(error: Any, credential_status: str, credential_present: bool, quota: Any) -> str:
    if error == "quota_auth_required":
        return "auth_required"
    if error == "quota_transport_error":
        return "transport_error"
    if error == "quota_rate_limited":
        return "rate_limited"
    if isinstance(error, str) and error:
        return "unclassified_error"
    if credential_status == "invalid":
        return "auth_required"
    if not credential_present:
        return "credential_missing"
    return "success" if isinstance(quota, dict) else "not_checked"


def _accounts(state: Any) -> Dict[str, Any]:
    public = state.accounts_snapshot()
    errors = public.get("refresh_errors", {})
    if not isinstance(errors, dict):
        errors = {}
    native = public.get("native_account") or {}
    if not isinstance(native, dict):
        native = {}
    native_present = native.get("credential_set") is True
    native_report = {
        "credential_present": native_present,
        "quota_status": _quota_status(errors.get(NATIVE_ACCOUNT_ID), "unknown", native_present, native.get("quota")),
    }
    imported = []
    accounts = public.get("accounts") or []
    if not isinstance(accounts, list):
        accounts = []
    for index, account in enumerate(accounts[:32], 1):
        if not isinstance(account, dict):
            continue
        credential_status = _choice(account.get("credential_status"), _CREDENTIAL_STATES)
        present = account.get("credential_set") is True
        imported.append({
            "index": index,
            "credential_present": present,
            "credential_status": credential_status,
            "quota_status": _quota_status(errors.get(account.get("id")), credential_status, present, account.get("quota")),
        })
    return {
        "native": native_report,
        "imported_count": len(accounts),
        "imported": imported,
        "imported_truncated": len(accounts) > 32,
        "scope": "saved_status_and_this_process_errors",
    }


def build_support_report(state: Any, proxy_source: Any = None) -> Dict[str, Any]:
    """Build a read-only report from whitelisted facts; never return raw state."""
    return {
        "schema_version": 1,
        "generated_at": datetime.datetime.now(datetime.timezone.utc).isoformat().replace("+00:00", "Z"),
        "emp_version": __version__,
        "configuration": _configuration(state.path),
        "codex": _runtime(state),
        "network": _network(proxy_source),
        "accounts": _accounts(state),
    }
