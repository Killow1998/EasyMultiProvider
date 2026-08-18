"""Small dependency-free Web UI and HTTP router server."""

from __future__ import annotations

import json
import threading
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, Dict, Optional
from urllib.parse import unquote, urlparse

from .accounts import account_root, import_account, public_accounts
from .catalog import build_catalog, generated_catalog_path, integration_info, write_catalog
from .config import ConfigError, config_path, load, merge_web_update, public_config, save
from .quota import QuotaError, read_account_quota
from .router import RouterError, discover_models, model_metadata, proxy


WEB_FILE = Path(__file__).with_name("web").joinpath("index.html")


class AppState:
    def __init__(self, path: Optional[Path] = None):
        self.path = Path(path or config_path())
        self.lock = threading.RLock()
        self.account_refresh_locks = {}
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
            updated = merge_web_update(self.config, incoming)
            save(updated, self.path)
            self.config = load(self.path)
            return self.snapshot()

    def discover_provider_models(self, provider_id: str) -> Dict[str, Any]:
        with self.lock:
            provider = next(
                (item for item in self.config.get("providers", []) if item.get("id") == provider_id),
                None,
            )
            if provider is None or not provider.get("enabled", True):
                raise ConfigError("provider is missing or disabled: %s" % provider_id)
            provider = dict(provider)

        discovered = discover_models(provider)
        with self.lock:
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
                    continue
                model = {
                    "id": model_id,
                    "provider": provider_id,
                    "upstream_id": upstream_id,
                    "display_name": item.get("display_name") or upstream_id,
                    "description": item.get("description", ""),
                    "reasoning_levels": item.get("reasoning_levels") or ["medium"],
                    "context_window": int(item.get("context_window", 0) or 0),
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
                "available": len(discovered),
                "added": added,
                "catalog_path": str(catalog_path.resolve()),
                "model_count": len(build_catalog(self.config)["models"]),
            }

    def import_account(self, metadata: Dict[str, Any], auth_json: Dict[str, Any]) -> Dict[str, Any]:
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
            account = next(
                (item for item in self.config.get("accounts", []) if item.get("id") == account_id),
                None,
            )
            if account is None:
                raise QuotaError("unknown account: %s" % account_id)
            target = dict(account)
            refresh_lock = self.account_refresh_locks.setdefault(account_id, threading.Lock())
        with refresh_lock:
            quota = read_account_quota(target)
            with self.lock:
                for item in self.config.get("accounts", []):
                    if item.get("id") == account_id and item.get("auth_file") == target.get("auth_file"):
                        item["quota"] = quota
                        save(self.config, self.path)
                        return dict(item)
            raise QuotaError("account changed during quota refresh")

    def delete_account(self, account_id: str) -> None:
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
        server_version = "EasyMultiProvider/0.1"

        def log_message(self, format: str, *args: Any) -> None:
            # Request paths never contain credentials; bodies are never logged.
            super().log_message(format, *args)

        def _same_origin(self) -> bool:
            origin = self.headers.get("Origin", "")
            if not origin:
                return True
            parsed = urlparse(origin)
            return parsed.scheme in ("http", "https") and parsed.netloc == self.headers.get("Host", "")

        def _send(self, status: int, body: bytes, content_type: str = "application/json") -> None:
            self.send_response(status)
            self.send_header("Content-Type", content_type)
            self.send_header("Content-Length", str(len(body)))
            if content_type == "application/json":
                self.send_header("Cache-Control", "no-store")
            self.end_headers()
            self.wfile.write(body)

        def _error(self, status: int, message: str) -> None:
            self._send(status, _json_bytes({"error": {"message": message}}))

        def _body(self) -> Dict[str, Any]:
            try:
                length = int(self.headers.get("Content-Length", "0"))
            except ValueError:
                raise ConfigError("invalid Content-Length")
            if length > 5 * 1024 * 1024:
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
            if path.startswith("/api/") and not self._same_origin():
                self._error(403, "cross-origin management request rejected")
                return
            if path in ("/", "/index.html"):
                self._send(200, WEB_FILE.read_bytes(), "text/html; charset=utf-8")
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
            if path.startswith("/api/") and not self._same_origin():
                self._error(403, "cross-origin management request rejected")
                return
            try:
                body = self._body()
                if path == "/api/config":
                    updated = state.update(body)
                    self._send(200, _json_bytes(public_config(updated)))
                    return
                if path == "/api/providers/discover":
                    provider_id = body.get("provider")
                    if not isinstance(provider_id, str) or not provider_id:
                        raise ConfigError("provider is required")
                    result = state.discover_provider_models(provider_id)
                    self._send(200, _json_bytes(result))
                    return
                if path == "/api/accounts/import":
                    auth_json = body.pop("auth_json", None)
                    account = state.import_account(body, auth_json)
                    self._send(200, _json_bytes({"account": public_accounts([account])[0]}))
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
            if path.startswith("/api/") and not self._same_origin():
                self._error(403, "cross-origin management request rejected")
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


def serve(path: Optional[Path] = None, host: Optional[str] = None, port: Optional[int] = None) -> None:
    if host and host != "127.0.0.1":
        raise ConfigError("host must be 127.0.0.1 for local-only management")
    state = AppState(path)
    if host:
        state.config["host"] = host
    if port:
        state.config["port"] = port
    config = state.snapshot()
    bind_host = host or config["host"]
    bind_port = port or config["port"]
    server = ThreadingHTTPServer((bind_host, bind_port), make_handler(state))
    print("EasyMultiProvider listening on http://%s:%d" % (bind_host, bind_port), flush=True)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()
