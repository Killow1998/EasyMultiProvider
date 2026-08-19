"""Small dependency-free Web UI and HTTP router server."""

from __future__ import annotations

import ast
import base64
import json
import hmac
import os
import secrets
import subprocess
import sys
import threading
from http.cookies import SimpleCookie
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import parse_qs, unquote, urlparse
from urllib.request import getproxies

from .accounts import account_root, import_account, public_accounts, valid_caller_authorization
from .catalog import (
    build_catalog,
    generated_catalog_path,
    integration_info,
    write_catalog,
    write_codex_profile,
)
from .config import ConfigError, config_path, load, merge_web_update, public_config, save
from .migration import export_bundle, import_bundle
from .quota import QuotaError, account_refresh_lock, refresh_account_quota
from .router import RouterError, discover_models, model_metadata, proxy


WEB_FILE = Path(__file__).with_name("web").joinpath("index.html")
PROXY_ENV_KEYS = (
    "HTTP_PROXY",
    "HTTPS_PROXY",
    "ALL_PROXY",
    "http_proxy",
    "https_proxy",
    "all_proxy",
)


def _gsettings_value(schema: str, key: str) -> Any:
    try:
        result = subprocess.run(
            ["gsettings", "get", schema, key],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=2,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired):
        return None
    if result.returncode:
        return None
    value = result.stdout.strip()
    if value == "true":
        return True
    if value == "false":
        return False
    try:
        return ast.literal_eval(value)
    except (SyntaxError, ValueError):
        return None


def _proxy_url(host: Any, port: Any, scheme: str) -> str:
    if not isinstance(host, str) or not host.strip():
        return ""
    host = host.strip()
    if any(character.isspace() or character in "/@?#" for character in host):
        return ""
    if not isinstance(port, int) or isinstance(port, bool) or not 1 <= port <= 65535:
        return ""
    if ":" in host and not host.startswith("["):
        host = "[" + host + "]"
    return "%s://%s:%d" % (scheme, host, port)


def _gnome_proxy_settings() -> Dict[str, Any]:
    if not sys.platform.startswith("linux"):
        return {}
    root = "org.gnome.system.proxy"
    if _gsettings_value(root, "mode") != "manual":
        return {}
    http = _proxy_url(
        _gsettings_value(root + ".http", "host"),
        _gsettings_value(root + ".http", "port"),
        "http",
    )
    https = _proxy_url(
        _gsettings_value(root + ".https", "host"),
        _gsettings_value(root + ".https", "port"),
        "http",
    )
    if not https and _gsettings_value(root, "use-same-proxy") is True:
        https = http
    socks = _proxy_url(
        _gsettings_value(root + ".socks", "host"),
        _gsettings_value(root + ".socks", "port"),
        "socks5",
    )
    ignored = _gsettings_value(root, "ignore-hosts")
    return {
        "http": http,
        "https": https,
        "all": socks,
        "no": ",".join(item for item in ignored if isinstance(item, str))
        if isinstance(ignored, list)
        else "",
    }


def _apply_proxy_settings(settings: Dict[str, Any]) -> bool:
    applied = False
    for scheme in ("http", "https", "all"):
        value = settings.get(scheme)
        if not isinstance(value, str) or not value:
            continue
        parsed = urlparse(value)
        try:
            valid = parsed.scheme in ("http", "https", "socks5", "socks5h") and bool(
                parsed.hostname and parsed.port
            )
        except ValueError:
            valid = False
        if not valid:
            continue
        os.environ.setdefault(scheme + "_proxy", value)
        os.environ.setdefault(scheme.upper() + "_PROXY", value)
        applied = True
    ignored = settings.get("no")
    if applied and isinstance(ignored, str) and ignored:
        os.environ.setdefault("no_proxy", ignored)
        os.environ.setdefault("NO_PROXY", ignored)
    return applied


def configure_proxy_environment() -> str:
    """Prefer explicit environment proxies, then safe operating-system settings."""
    if any(os.environ.get(key) for key in PROXY_ENV_KEYS):
        return "environment"
    if _apply_proxy_settings(getproxies()):
        return "system"
    if _apply_proxy_settings(_gnome_proxy_settings()):
        return "system"
    return "direct"


class AppState:
    def __init__(self, path: Optional[Path] = None):
        self.path = Path(path or config_path())
        self.lock = threading.RLock()
        self.bootstrap_token = secrets.token_urlsafe(32)
        self.bootstrap_used = False
        self.session_token = secrets.token_urlsafe(32)
        # Discovery is low-frequency and upstream-bound; one fixed lock avoids
        # retaining attacker-controlled provider IDs in process state.
        self.discovery_lock = threading.Lock()
        self.config = load(self.path)
        if any(
            provider.get("api_key") for provider in self.config.get("providers", [])
        ):
            save(self.config, self.path)
            self.config = load(self.path)

    def snapshot(self) -> Dict[str, Any]:
        with self.lock:
            return json.loads(json.dumps(self.config))

    def update(self, incoming: Dict[str, Any]) -> Dict[str, Any]:
        with self.lock:
            updated = merge_web_update(self.config, incoming, self.path)
            save(updated, self.path)
            self.config = load(self.path)
            return self.snapshot()

    def export_migration(self, password: str) -> bytes:
        with self.lock:
            return export_bundle(self.config, self.path, password)

    def import_migration(self, bundle: bytes, password: str) -> Dict[str, int]:
        with self.lock:
            self.config, summary = import_bundle(self.config, bundle, password, self.path)
            catalog_path = write_catalog(self.config, generated_catalog_path())
            summary["catalog_path"] = str(catalog_path.resolve())
            return summary

    def discover_provider_models(
        self, provider_id: str, selected: Optional[list] = None
    ) -> Dict[str, Any]:
        with self.lock:
            provider = next(
                (item for item in self.config.get("providers", []) if item.get("id") == provider_id),
                None,
            )
            if provider is None or not provider.get("enabled", True):
                raise ConfigError("provider is missing or disabled: %s" % provider_id)
            provider = dict(provider)

        with self.discovery_lock:
            discovered = discover_models(provider)
        with self.lock:
            for configured in self.config.get("providers", []):
                if configured.get("id") == provider_id and configured.get("protocol") != provider.get("protocol"):
                    configured["protocol"] = provider["protocol"]
                    save(self.config, self.path)
                    break
            if selected is None:
                return {
                    "provider": provider_id,
                    "protocol": provider.get("protocol"),
                    "available": len(discovered),
                    "models": discovered,
                    "added": 0,
                }
            if not isinstance(selected, list) or any(not isinstance(item, str) for item in selected):
                raise ConfigError("selected models must be a list of model IDs")
            selected_ids = set(selected)
            available_ids = {item.get("upstream_id") for item in discovered}
            unknown = selected_ids - available_ids
            if unknown:
                raise ConfigError("selected model is not in the discovered list")
            discovered = [item for item in discovered if item.get("upstream_id") in selected_ids]
            models = list(self.config.get("models", []))
            by_id = {item.get("id"): item for item in models}
            added = 0
            for item in discovered:
                upstream_id = item.get("upstream_id", "")
                model_id = provider_id + "/" + upstream_id
                if not upstream_id or model_id in by_id:
                    existing = by_id.get(model_id)
                    if existing and item.get("reasoning_levels") and existing.get("reasoning_levels") == ["medium"]:
                        existing["reasoning_levels"] = item["reasoning_levels"]
                    if existing and item.get("created_at") and not existing.get("created_at"):
                        existing["created_at"] = item["created_at"]
                    continue
                model = {
                    "id": model_id,
                    "provider": provider_id,
                    "upstream_id": upstream_id,
                    "display_name": item.get("display_name") or upstream_id,
                    "description": item.get("description", ""),
                    "reasoning_levels": item.get("reasoning_levels") or ["medium"],
                    "context_window": int(item.get("context_window", 0) or 0),
                    "created_at": int(item.get("created_at", 0) or 0),
                    "enabled": True,
                }
                models.append(model)
                by_id[model_id] = model
                added += 1
            updated = dict(self.config)
            updated["models"] = models
            self.config = load_from_value(updated)
            save(self.config, self.path)
            self.config = load(self.path)
            catalog_path = write_catalog(self.config, generated_catalog_path())
            return {
                "provider": provider_id,
                "protocol": provider.get("protocol"),
                "available": len(discovered),
                "added": added,
                "catalog_path": str(catalog_path.resolve()),
                "model_count": len(build_catalog(self.config)["models"]),
            }

    def import_account(self, metadata: Dict[str, Any], auth_json: Dict[str, Any]) -> Dict[str, Any]:
        with account_refresh_lock(metadata.get("id")):
            with self.lock:
                account_id = metadata.get("id")
                prefix = metadata.get("prefix")
                current_accounts = self.config.get("accounts", [])
                for account in current_accounts:
                    if account.get("id") != account_id and account.get("prefix") == prefix:
                        raise ConfigError("account prefix is already in use: %s" % prefix)
                account = import_account(self.config, metadata, auth_json, self.path)
                accounts = [item for item in current_accounts if item.get("id") != account["id"]]
                accounts.append(account)
                updated = dict(self.config)
                updated["accounts"] = accounts
                self.config = load_from_value(updated)
                save(self.config, self.path)
                return account

    def refresh_account(self, account_id: str) -> Dict[str, Any]:
        with self.lock:
            if not any(item.get("id") == account_id for item in self.config.get("accounts", [])):
                raise QuotaError("unknown account: %s" % account_id)
        with account_refresh_lock(account_id):
            with self.lock:
                account = next(
                    (item for item in self.config.get("accounts", []) if item.get("id") == account_id),
                    None,
                )
                if account is None:
                    raise QuotaError("unknown account: %s" % account_id)
                target = dict(account)
            quota = refresh_account_quota(target)
            with self.lock:
                for item in self.config.get("accounts", []):
                    if item.get("id") == account_id and item.get("auth_file") == target.get("auth_file"):
                        item["quota"] = quota
                        save(self.config, self.path)
                        return dict(item)
                raise QuotaError("account changed during quota refresh")

    def delete_account(self, account_id: str) -> None:
        with self.lock:
            if not any(item.get("id") == account_id for item in self.config.get("accounts", [])):
                raise ConfigError("unknown account: %s" % account_id)
        with account_refresh_lock(account_id):
            self._delete_account(account_id)

    def _delete_account(self, account_id: str) -> None:
        with self.lock:
            accounts = self.config.get("accounts", [])
            target = next((item for item in accounts if item.get("id") == account_id), None)
            if target is None:
                raise ConfigError("unknown account: %s" % account_id)

            root = account_root(self.config, self.path).resolve()
            account_dir = (root / account_id).resolve()
            auth_path = Path(target.get("auth_file", "")).expanduser()
            if not auth_path.is_absolute():
                auth_path = self.path.parent / auth_path
            if auth_path.resolve() != account_dir / "auth.json.enc":
                raise ConfigError("refusing to delete credentials outside the account store")

            updated = dict(self.config)
            updated["accounts"] = [item for item in accounts if item.get("id") != account_id]
            save(updated, self.path)
            self.config = load(self.path)

            for private_file in (auth_path, account_dir / "config.toml"):
                try:
                    private_file.unlink()
                except FileNotFoundError:
                    pass
            try:
                account_dir.rmdir()
            except OSError:
                pass


def load_from_value(value: Dict[str, Any]) -> Dict[str, Any]:
    """Normalize an in-memory config without creating a second file format."""
    from .config import normalize

    return normalize(value)


def _json_bytes(value: Any) -> bytes:
    return json.dumps(value, ensure_ascii=False).encode("utf-8")


def make_handler(state: AppState):
    class Handler(BaseHTTPRequestHandler):
        server_version = "EasyMultiProvider/0.2"

        def setup(self) -> None:
            super().setup()
            self.connection.settimeout(30)

        def log_message(self, format: str, *args: Any) -> None:
            # The bootstrap URL contains a one-time secret; never log its query.
            message = format % args if args else format
            message = message.replace(self.path, urlparse(self.path).path)
            super().log_message("%s", message)

        def _same_origin(self) -> bool:
            host = self.headers.get("Host", "").lower().rstrip(".")
            port = self.server.server_address[1]
            allowed_hosts = {"127.0.0.1:%d" % port, "localhost:%d" % port}
            if host not in allowed_hosts:
                return False
            origin = self.headers.get("Origin", "")
            if not origin:
                return True
            try:
                parsed = urlparse(origin)
                return (
                    parsed.scheme in ("http", "https")
                    and not parsed.username
                    and not parsed.password
                    and parsed.hostname in ("127.0.0.1", "localhost")
                    and parsed.port == port
                )
            except ValueError:
                return False

        def _has_session(self) -> bool:
            cookie = SimpleCookie()
            try:
                cookie.load(self.headers.get("Cookie", ""))
            except (TypeError, ValueError):
                return False
            supplied = cookie.get("emp_session")
            return bool(
                supplied
                and hmac.compare_digest(supplied.value, state.session_token)
            )

        def _has_bootstrap(self) -> bool:
            query = parse_qs(urlparse(self.path).query, keep_blank_values=True)
            values = query.get("bootstrap", [])
            if len(values) != 1 or not hmac.compare_digest(values[0], state.bootstrap_token):
                return False
            with state.lock:
                if state.bootstrap_used:
                    return False
                state.bootstrap_used = True
                return True

        def _management_allowed(self) -> bool:
            return self._same_origin() and self._has_session()

        def _proxy_allowed(self) -> bool:
            return self._same_origin() and (
                self._has_session()
                or valid_caller_authorization(self.headers.get("Authorization", ""))
            )

        def _session_header(self) -> str:
            return "emp_session=%s; HttpOnly; SameSite=Strict; Path=/" % state.session_token

        def _send(
            self,
            status: int,
            body: bytes,
            content_type: str = "application/json",
            headers: Optional[Dict[str, str]] = None,
        ) -> None:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            if content_type == "application/json":
                self.send_header("Cache-Control", "no-store")
            for key, value in (headers or {}).items():
                self.send_header(key, value)
            self.end_headers()
            self.wfile.write(body)

        def _error(self, status: int, message: str) -> None:
            self._send(status, _json_bytes({"error": {"message": message}}))

        def _body(self, max_length: int = 5 * 1024 * 1024) -> Dict[str, Any]:
            if self.headers.get_content_type() != "application/json":
                raise ConfigError("Content-Type must be application/json")
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                raise ConfigError("invalid Content-Length")
            if length < 0:
                raise ConfigError("Content-Length cannot be negative")
            if length > max_length:
                raise ConfigError("request body is too large")
            raw = self.rfile.read(length)
            try:
                value = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, ValueError) as exc:
                raise ConfigError("request body must be valid JSON: %s" % exc)
            if not isinstance(value, dict):
                raise ConfigError("request body must be a JSON object")
            return value

        def do_GET(self) -> None:
            path = urlparse(self.path).path
            if path in ("/", "/index.html") and not self._same_origin():
                self._error(403, "cross-origin Web UI request rejected")
                return
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            if path in ("/", "/index.html"):
                if not self._has_session():
                    if not self._has_bootstrap():
                        self._error(401, "open the management URL printed by EasyMultiProvider")
                        return
                    self._send(
                        303,
                        b"",
                        "text/plain; charset=utf-8",
                        {
                            "Location": "/",
                            "Set-Cookie": self._session_header(),
                        },
                    )
                    return
                self._send(
                    200,
                    WEB_FILE.read_bytes(),
                    "text/html; charset=utf-8",
                    {"Set-Cookie": self._session_header()},
                )
                return
            if path == "/healthz":
                self._send(200, _json_bytes({"status": "ok"}))
                return
            if path == "/api/config":
                self._send(200, _json_bytes(public_config(state.snapshot())))
                return
            if path == "/api/accounts":
                self._send(200, _json_bytes({"accounts": public_accounts(state.snapshot().get("accounts", []))}))
                return
            if path == "/api/integration":
                self._send(
                    200,
                    _json_bytes(integration_info(state.snapshot(), generated_catalog_path())),
                )
                return
            if path == "/v1/models":
                catalog = build_catalog(state.snapshot())
                data = [
                    {
                        "id": model.get("slug"),
                        "object": "model",
                        "created": 0,
                        "owned_by": "easy-multi-provider",
                    }
                    for model in catalog["models"]
                    if model.get("visibility", "list") == "list"
                ]
                self._send(200, _json_bytes({"object": "list", "data": data}))
                return
            if path.startswith("/v1/models/"):
                model_id = unquote(path[len("/v1/models/"):])
                catalog = build_catalog(state.snapshot())
                if any(model.get("slug") == model_id for model in catalog["models"]):
                    self._send(200, _json_bytes({"id": model_id, "object": "model", "created": 0}))
                else:
                    self._error(404, "unknown model: %s" % model_id)
                return
            self._error(404, "not found")

        def do_POST(self) -> None:
            path = urlparse(self.path).path
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            if path == "/v1/responses" and not self._proxy_allowed():
                self._error(401 if self._same_origin() else 403, "proxy caller authentication is required")
                return
            try:
                body = self._body(32 * 1024 * 1024 if path == "/api/migration/import" else 5 * 1024 * 1024)
                if path == "/api/config":
                    updated = state.update(body)
                    self._send(200, _json_bytes(public_config(updated)))
                    return
                if path == "/api/providers/discover":
                    provider_id = body.get("provider")
                    if not isinstance(provider_id, str) or not provider_id:
                        raise ConfigError("provider is required")
                    selected = body.get("selected") if "selected" in body else None
                    result = state.discover_provider_models(provider_id, selected)
                    self._send(200, _json_bytes(result))
                    return
                if path == "/api/accounts/import":
                    auth_json = body.pop("auth_json", None)
                    account = state.import_account(body, auth_json)
                    self._send(200, _json_bytes({"account": public_accounts([account])[0]}))
                    return
                if path == "/api/migration/export":
                    bundle = state.export_migration(body.get("password"))
                    self._send(
                        200,
                        bundle,
                        "application/octet-stream",
                        {
                            "Cache-Control": "no-store",
                            "Content-Disposition": 'attachment; filename="easy-multi-provider-0.2.0.emp"',
                        },
                    )
                    return
                if path == "/api/migration/import":
                    encoded = body.get("bundle")
                    if not isinstance(encoded, str) or not encoded:
                        raise ConfigError("migration bundle is required")
                    try:
                        bundle = base64.b64decode(encoded.encode("ascii"), validate=True)
                    except (UnicodeEncodeError, ValueError) as exc:
                        raise ConfigError("migration bundle is not valid base64") from exc
                    summary = state.import_migration(bundle, body.get("password"))
                    self._send(200, _json_bytes({"status": "ok", **summary}))
                    return
                if path.startswith("/api/accounts/") and path.endswith("/quota"):
                    account_id = unquote(path[len("/api/accounts/") : -len("/quota")].rstrip("/"))
                    account = state.refresh_account(account_id)
                    self._send(200, _json_bytes({"account": public_accounts([account])[0]}))
                    return
                if path == "/api/catalog/refresh":
                    catalog_path = write_catalog(state.snapshot(), generated_catalog_path())
                    self._send(
                        200,
                        _json_bytes(
                            {
                                "status": "ok",
                                "catalog_path": str(catalog_path.resolve()),
                                "model_count": len(build_catalog(state.snapshot())["models"]),
                            }
                        ),
                    )
                    return
                if path == "/api/integration/generate":
                    config = state.snapshot()
                    catalog_path = write_catalog(config, generated_catalog_path())
                    profile_path = write_codex_profile(config, catalog_path)
                    info = integration_info(config, catalog_path)
                    info["profile_path"] = str(profile_path)
                    self._send(200, _json_bytes(info))
                    return
                if path == "/api/models/metadata":
                    provider_id = body.get("provider")
                    upstream_model = body.get("model")
                    if not isinstance(provider_id, str) or not isinstance(upstream_model, str):
                        raise ConfigError("provider and model are required")
                    provider = next(
                        (item for item in state.snapshot().get("providers", []) if item.get("id") == provider_id),
                        None,
                    )
                    if provider is None or not provider.get("enabled", True):
                        raise ConfigError("provider is missing or disabled: %s" % provider_id)
                    prefix = provider_id + "/"
                    if upstream_model.startswith(prefix):
                        upstream_model = upstream_model[len(prefix):]
                    self._send(200, _json_bytes(model_metadata(provider, upstream_model)))
                    return
                if path == "/v1/responses":
                    metadata, result = proxy(
                        state.snapshot(), body, {key: value for key, value in self.headers.items()}
                    )
                    if metadata["kind"] == "stream":
                        self.send_response(200)
                        self.send_header("Content-Type", metadata["content_type"])
                        self.send_header("Cache-Control", "no-cache")
                        self.send_header("Connection", "keep-alive")
                        self.end_headers()
                        for chunk in result:
                            self.wfile.write(chunk)
                            self.wfile.flush()
                    elif metadata["kind"] == "raw_stream":
                        self.send_response(metadata.get("status", 200))
                        self.send_header("Content-Type", metadata["content_type"])
                        self.send_header("Cache-Control", "no-cache")
                        self.send_header("Connection", "keep-alive")
                        self.end_headers()
                        try:
                            while True:
                                chunk = result.read(8192)
                                if not chunk:
                                    break
                                self.wfile.write(chunk)
                                self.wfile.flush()
                        finally:
                            result.close()
                    else:
                        self._send(
                            metadata.get("status", 200),
                            result,
                            metadata.get("content_type", "application/json"),
                        )
                    return
                self._error(404, "not found")
            except (ConfigError, RouterError, QuotaError, ValueError) as exc:
                status = exc.status if isinstance(exc, RouterError) else 400
                if isinstance(exc, QuotaError):
                    status = 503
                self._error(status, str(exc))
            except Exception as exc:  # Keep server alive and avoid leaking request details.
                self._error(500, "internal server error: %s" % exc)

        def do_DELETE(self) -> None:
            path = urlparse(self.path).path
            if path.startswith("/api/") and not self._management_allowed():
                self._error(401 if self._same_origin() else 403, "management session is required")
                return
            try:
                if path.startswith("/api/accounts/"):
                    account_id = unquote(path[len("/api/accounts/") :].rstrip("/"))
                    state.delete_account(account_id)
                    self._send(200, _json_bytes({"status": "ok"}))
                    return
                self._error(404, "not found")
            except (ConfigError, QuotaError, ValueError) as exc:
                self._error(400, str(exc))
            except Exception as exc:  # Keep server alive and avoid leaking request details.
                self._error(500, "internal server error: %s" % exc)

    return Handler


class BoundedThreadingHTTPServer(ThreadingHTTPServer):
    daemon_threads = True
    request_limit = 32

    def __init__(self, server_address, handler_cls):
        super().__init__(server_address, handler_cls)
        self._request_slots = threading.BoundedSemaphore(self.request_limit)

    def process_request(self, request, client_address):
        if not self._request_slots.acquire(blocking=False):
            request.close()
            return
        try:
            super().process_request(request, client_address)
        except BaseException:
            self._request_slots.release()
            raise

    def process_request_thread(self, request, client_address):
        try:
            super().process_request_thread(request, client_address)
        finally:
            self._request_slots.release()


def serve(path: Optional[Path] = None, host: Optional[str] = None, port: Optional[int] = None) -> None:
    if host and host != "127.0.0.1":
        raise ConfigError("host must be 127.0.0.1 for local-only management")
    proxy_source = configure_proxy_environment()
    state = AppState(path)
    if host:
        state.config["host"] = host
    if port:
        state.config["port"] = port
    config = state.snapshot()
    bind_host = host or config["host"]
    bind_port = port or config["port"]
    server = BoundedThreadingHTTPServer((bind_host, bind_port), make_handler(state))
    base_url = "http://%s:%d" % (bind_host, bind_port)
    print("EasyMultiProvider listening on %s" % base_url, flush=True)
    print("Network proxy: %s" % proxy_source, flush=True)
    print("Open in browser: %s/?bootstrap=%s" % (base_url, state.bootstrap_token), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
