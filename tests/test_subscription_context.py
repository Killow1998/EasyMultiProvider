"""Subscription context limits, account isolation and migration contracts."""
import copy
import json
import shutil
import subprocess
import tempfile
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest.mock import Mock, patch

from tests.support import ensure_test_master_key
from easy_multi_provider.accounts import import_account, load_auth
from easy_multi_provider.catalog import build_catalog, subscription_model_options
from easy_multi_provider.config import load, normalize, save
from easy_multi_provider.migration import export_bundle, import_bundle
from easy_multi_provider.route_plan import resolve_route
from easy_multi_provider.router import RouterError, fetch_subscription_catalog
from easy_multi_provider.server import AppState
from tests.test_diagnostic_journal_integration import request, running_server

ensure_test_master_key()


class SubscriptionContextTests(unittest.TestCase):
    def fixture(self, root):
        catalog = {"models": [{"slug": "gpt-current", "display_name": "Current", "context_window": 272000,
            "max_context_window": 1000000, "effective_context_window_percent": 95, "auto_compact_token_limit": 244800}]}
        native = root / "native.json"
        native.write_text(json.dumps(catalog))
        path = root / "config.json"
        config = normalize({"native_catalog_path": str(native)})
        config["accounts"] = [import_account(config, {"id": name, "prefix": name},
            {"tokens": {"access_token": name + "-token", "account_id": name}}, path) for name in ("a", "b")]
        save(config, path)
        return path, load(path), catalog

    def test_catalog_and_forwarding_use_same_context_without_affecting_other_accounts(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, _ = self.fixture(Path(temporary))
            config["accounts"][0]["model_context_windows"] = {"gpt-current": 1000000}
            config["native_model_context_windows"] = {"gpt-current": 500000}
            models = {m["slug"]: m for m in build_catalog(config)["models"]}
            self.assertEqual(models["a/gpt-current"]["context_window"], 1000000)
            self.assertIn("950K", models["a/gpt-current"]["display_name"])
            self.assertEqual(models["b/gpt-current"]["context_window"], 272000)
            self.assertEqual(models["gpt-current"]["context_window"], 500000)
            for slug in ("gpt-current", "a/gpt-current", "b/gpt-current"):
                self.assertEqual(resolve_route(config, slug).model["context_window"], models[slug]["context_window"])
                self.assertEqual(models[slug]["effective_context_window_percent"], 95)
            self.assertNotIn("auto_compact_token_limit", models["a/gpt-current"])
            self.assertEqual(models["b/gpt-current"]["auto_compact_token_limit"], 244800)

    def test_upper_bound_comes_from_catalog_and_reduced_limits_clamp_existing_settings(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            state = AppState(path)
            candidate = state.snapshot()
            candidate["accounts"][0]["model_context_windows"] = {"gpt-current": 1000000}
            state.update(candidate)
            before = path.read_bytes()
            for invalid in (1000001, 1.5, True, -1):
                candidate = state.snapshot()
                candidate["accounts"][0]["model_context_windows"] = {"gpt-current": invalid}
                with self.assertRaises(ValueError): state.update(candidate)
                self.assertEqual(path.read_bytes(), before)
            catalog["models"][0]["max_context_window"] = 700000
            Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
            self.assertEqual(resolve_route(state.snapshot(), "a/gpt-current").model["context_window"], 700000)
            state.update(state.snapshot())  # Unrelated saves still work after a catalog limit changes.
            candidate = state.snapshot(); candidate["accounts"][0]["model_context_windows"] = {}
            state.update(candidate)
            self.assertEqual(resolve_route(state.snapshot(), "a/gpt-current").model["context_window"], 272000)

    def test_missing_and_invalid_maxima_do_not_invent_a_one_million_limit(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            del catalog["models"][0]["max_context_window"]
            Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
            self.assertEqual(subscription_model_options(config)[0]["max_context_window"], 272000)
            catalog["models"][0]["max_context_window"] = "1000000"
            Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
            self.assertEqual(subscription_model_options(config)[0]["max_context_window"], 0)

    def test_omitted_effective_percentage_matches_codex_default_and_preserves_explicit_values(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            for percentage, expected in ((None, 258400), (90, 244800)):
                catalog["models"][0]["effective_context_window_percent"] = percentage
                Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
                model = resolve_route(config, "a/gpt-current").model
                self.assertEqual(round(model["context_window"] * model["effective_context_window_percent"] / 100), expected)
                self.assertEqual(subscription_model_options(config)[0]["context_window"], expected)

    def test_account_refresh_uses_own_credentials_and_cannot_change_others(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            state = AppState(path)
            refreshed = copy.deepcopy(catalog); refreshed["models"][0]["max_context_window"] = 800000
            with patch("easy_multi_provider.server.fetch_subscription_catalog", return_value=refreshed) as fetch:
                result = state.subscription_models("a", refresh=True)
            self.assertEqual(fetch.call_args.args[1]["Authorization"], "Bearer a-token")
            self.assertEqual(fetch.call_args.args[1]["chatgpt-account-id"], "a")
            self.assertEqual(result["models"][0]["max_context_window"], 800000)
            self.assertEqual(state.subscription_models("b")["models"][0]["max_context_window"], 1000000)
            cached = Path(config["accounts"][0]["auth_file"]).parent / "models_cache.json"
            self.assertNotIn("a-token", cached.read_text())
            # A removed/replaced login with a reused local ID must not inherit old entitlements.
            replacement = import_account(config, {"id": "a", "prefix": "a"}, {"tokens": {"access_token": "new", "account_id": "different"}}, path)
            self.assertEqual(subscription_model_options(config, replacement)[0]["max_context_window"], 1000000)

    def test_account_refresh_rejects_credential_change_before_cache_write(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            state = AppState(path)
            cache = Path(config["accounts"][0]["auth_file"]).parent / "models_cache.json"
            old_headers = {
                "Authorization": "Bearer old",
                "chatgpt-account-id": "account-a",
            }
            new_headers = {
                "Authorization": "Bearer new",
                "chatgpt-account-id": "account-b",
            }
            with patch(
                "easy_multi_provider.accounts.auth_headers",
                side_effect=[old_headers, new_headers],
            ), patch(
                "easy_multi_provider.server.fetch_subscription_catalog",
                return_value=copy.deepcopy(catalog),
            ) as fetch:
                with self.assertRaisesRegex(ValueError, "changed during model refresh"):
                    state.subscription_models("a", refresh=True)

            self.assertEqual(fetch.call_args.args[1], old_headers)
            self.assertFalse(cache.exists())

    def test_direct_account_import_is_atomic_across_metadata_and_config_failures(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, _, _ = self.fixture(Path(temporary))
            state = AppState(path)
            current = next(item for item in state.config["accounts"] if item["id"] == "a")
            auth_path = Path(current["auth_file"])
            account_config_path = auth_path.parent / "config.toml"
            original_main = path.read_bytes()
            original_auth = auth_path.read_bytes()
            original_account_config = account_config_path.read_bytes()
            original_loaded_auth = load_auth(current)

            with self.assertRaises(ValueError):
                state.import_account(
                    {
                        "id": "a",
                        "prefix": "a",
                        "model_context_windows": {"gpt-current": -1},
                    },
                    {"tokens": {"access_token": "replacement", "account_id": "new"}},
                )

            self.assertEqual(path.read_bytes(), original_main)
            self.assertEqual(auth_path.read_bytes(), original_auth)
            self.assertEqual(account_config_path.read_bytes(), original_account_config)
            self.assertEqual(load_auth(current), original_loaded_auth)
            self.assertEqual(state.snapshot(), load(path))

            with patch(
                "easy_multi_provider.server.save",
                side_effect=OSError("simulated config write failure"),
            ):
                with self.assertRaisesRegex(OSError, "simulated config write failure"):
                    state.import_account(
                        {"id": "a", "prefix": "a"},
                        {"tokens": {"access_token": "replacement", "account_id": "new"}},
                    )

            self.assertEqual(path.read_bytes(), original_main)
            self.assertEqual(auth_path.read_bytes(), original_auth)
            self.assertEqual(account_config_path.read_bytes(), original_account_config)
            self.assertEqual(load_auth(current), original_loaded_auth)
            self.assertEqual(state.snapshot(), load(path))

    def test_reimport_duplicate_decision_uses_replacement_credential(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path, _, _ = self.fixture(root)
            state = AppState(path)
            state.config["accounts"][0]["hidden_models"] = ["gpt-current"]
            save(state.config, path)
            native_auth = root / "native-auth.json"
            native_auth.write_text(json.dumps({
                "tokens": {"access_token": "a-token", "account_id": "a"}
            }))

            with patch(
                "easy_multi_provider.accounts.codex_auth_path",
                return_value=native_auth,
            ):
                state.import_account(
                    {"id": "a", "prefix": "a", "hidden_models": ["gpt-current"]},
                    {"tokens": {"access_token": "replacement", "account_id": "different"}},
                )

            account = next(item for item in state.config["accounts"] if item["id"] == "a")
            self.assertEqual(account["hidden_models"], ["gpt-current"])
            self.assertNotIn("gpt-current", state.config.get("native_hidden_models", []))

    def test_same_backend_legacy_forward_route_keeps_native_context_metadata(self):
        with tempfile.TemporaryDirectory() as temporary:
            _, config, _ = self.fixture(Path(temporary))
            config["native_model_context_windows"] = {"gpt-current": 872000}
            config["codex_base_url"] = "https://native.example/backend-api/codex"
            config["providers"] = [
                {
                    "id": "legacy-native",
                    "base_url": config["codex_base_url"],
                    "protocol": "responses",
                    "auth_mode": "forward",
                }
            ]

            native_route = resolve_route(config, "gpt-current")
            self.assertEqual(native_route.model["context_window"], 872000)
            self.assertEqual(
                native_route.model["effective_context_window_percent"], 95
            )

            config["providers"][0]["base_url"] = "https://gateway.example/v1"
            gateway_route = resolve_route(config, "gpt-current")
            self.assertNotIn("context_window", gateway_route.model)
            self.assertEqual(gateway_route.provider["base_url"], "https://gateway.example/v1")

    def test_catalog_snapshot_reuses_unchanged_sources_and_tracks_external_changes(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            state = AppState(path)
            with patch(
                "easy_multi_provider.server.build_catalog", wraps=build_catalog
            ) as builder:
                first = state.catalog_etag()
                self.assertEqual(state.catalog_etag(), first)
                self.assertEqual(builder.call_count, 1)

                catalog["models"].append(
                    {"slug": "gpt-added", "context_window": 128000}
                )
                Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
                second = state.catalog_etag()
                self.assertNotEqual(second, first)
                self.assertEqual(builder.call_count, 2)

                state.config["native_hidden_models"] = ["gpt-added"]
                third = state.catalog_etag()
                self.assertNotEqual(third, second)
                self.assertEqual(builder.call_count, 3)

                cache = (
                    Path(state.config["accounts"][0]["auth_file"]).parent
                    / "models_cache.json"
                )
                cache.write_text(json.dumps({"models": []}), encoding="utf-8")
                state.catalog_etag()
                self.assertEqual(builder.call_count, 4)

    def test_refresh_endpoints_require_management_session_and_reject_over_limit_save(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            state = AppState(path)
            headers = {"Cookie": "emp_session=" + state.session_token, "Content-Type": "application/json"}
            with running_server(state) as server, patch("easy_multi_provider.server.fetch_subscription_catalog", return_value=catalog) as fetch:
                self.assertEqual(request(server, "POST", "/api/accounts/a/models/refresh", b"{}")[0], 401)
                fetch.assert_not_called()
                self.assertEqual(request(server, "POST", "/api/accounts/a/models/refresh", b"{}", headers)[0], 200)
                status, raw = request(server, "GET", "/api/accounts/a/models", headers=headers)
                self.assertEqual(status, 200); self.assertEqual(json.loads(raw)["models"][0]["max_context_window"], 1000000)
                candidate = state.snapshot(); candidate["accounts"][0]["model_context_windows"] = {"gpt-current": 1000001}
                self.assertEqual(request(server, "POST", "/api/config", json.dumps(candidate).encode(), headers)[0], 400)

    def test_curl_context_save_refresh_and_reset_over_real_http(self):
        curl = shutil.which("curl.exe") or shutil.which("curl")
        if not curl:
            self.skipTest("curl is unavailable")
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            path, config, catalog = self.fixture(root)
            catalog["models"][0]["max_context_window"] = 872000
            catalog["models"].append({"slug": "gpt-5.5", "context_window": 272000})
            Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
            observed = []

            class CatalogUpstream(BaseHTTPRequestHandler):
                def do_GET(self):
                    observed.append((self.path, self.headers.get("Authorization"), self.headers.get("chatgpt-account-id")))
                    body = json.dumps(catalog).encode()
                    self.send_response(200)
                    self.send_header("Content-Type", "application/json")
                    self.send_header("Content-Length", str(len(body)))
                    self.end_headers()
                    self.wfile.write(body)

                def log_message(self, *_):
                    pass

            upstream = ThreadingHTTPServer(("127.0.0.1", 0), CatalogUpstream)
            worker = threading.Thread(target=upstream.serve_forever, daemon=True)
            worker.start()
            try:
                config["codex_base_url"] = "http://127.0.0.1:%d" % upstream.server_port
                save(config, path)
                state = AppState(path)
                with running_server(state) as server:
                    def http(method, endpoint, body=None):
                        command = [curl, "--silent", "--show-error", "--noproxy", "*", "--max-time", "10",
                            "--request", method, "--header", "Cookie: emp_session=" + state.session_token,
                            "--write-out", "\n%{http_code}", "http://127.0.0.1:%d%s" % (server.server_port, endpoint)]
                        if body is not None:
                            payload = root / "curl-request.json"
                            payload.write_text(json.dumps(body), encoding="utf-8")
                            command.extend(["--header", "Content-Type: application/json", "--data-binary", "@" + str(payload)])
                        result = subprocess.run(command, capture_output=True, text=True, check=True, timeout=15)
                        raw, status = result.stdout.rsplit("\n", 1)
                        return int(status), json.loads(raw)

                    status, limits = http("POST", "/api/accounts/a/models/refresh", {})
                    self.assertEqual(status, 200)
                    self.assertEqual(limits["models"][0]["max_context_window"], 872000)
                    self.assertEqual(observed[0][1:], ("Bearer a-token", "a"))
                    self.assertTrue(observed[0][0].startswith("/models?client_version="))
                    self.assertNotIn("gpt-5.5", {m["id"] for m in limits["models"]})

                    candidate = state.snapshot()
                    candidate["accounts"][0]["model_context_windows"] = {"gpt-current": 872000}
                    self.assertEqual(http("POST", "/api/config", candidate)[0], 200)
                    self.assertEqual(http("POST", "/api/catalog/refresh", {})[0], 200)
                    status, directory = http("GET", "/v1/models?client_version=0.154.0")
                    self.assertEqual(status, 200)
                    models = {m["slug"]: m for m in directory["models"]}
                    selected = models["a/gpt-current"]
                    self.assertEqual(selected["context_window"], 872000)
                    self.assertEqual(selected["effective_context_window_percent"], 95)
                    self.assertEqual(round(selected["context_window"] * .95), 828400)
                    self.assertEqual(resolve_route(state.snapshot(), "a/gpt-current").model["context_window"], 872000)
                    self.assertEqual(models["gpt-current"]["context_window"], 272000)
                    self.assertEqual(models["b/gpt-current"]["context_window"], 272000)
                    self.assertFalse(any(slug == "gpt-5.5" or slug.endswith("/gpt-5.5") for slug in models))
                    self.assertEqual(AppState(path).snapshot()["accounts"][0]["model_context_windows"], {"gpt-current": 872000})

                    before = path.read_bytes()
                    for invalid in (872001, -1, 1.5):
                        candidate["accounts"][0]["model_context_windows"] = {"gpt-current": invalid}
                        self.assertEqual(http("POST", "/api/config", candidate)[0], 400)
                        self.assertEqual(path.read_bytes(), before)
                    candidate["accounts"][0]["model_context_windows"] = {}
                    self.assertEqual(http("POST", "/api/config", candidate)[0], 200)
                    _, directory = http("GET", "/v1/models?client_version=0.154.0")
                    restored = next(m for m in directory["models"] if m["slug"] == "a/gpt-current")
                    self.assertEqual(restored["context_window"], 272000)
                    self.assertEqual(resolve_route(state.snapshot(), restored["slug"]).model["context_window"], 272000)
                    candidate["native_model_context_windows"] = {"gpt-current": 872000}
                    self.assertEqual(http("POST", "/api/config", candidate)[0], 200)
                    _, directory = http("GET", "/v1/models?client_version=0.154.0")
                    models = {m["slug"]: m for m in directory["models"]}
                    self.assertEqual(models["gpt-current"]["context_window"], 872000)
                    self.assertEqual(resolve_route(state.snapshot(), "gpt-current").model["context_window"], 872000)
                    self.assertEqual(models["a/gpt-current"]["context_window"], 272000)
                    self.assertEqual(models["b/gpt-current"]["context_window"], 272000)
            finally:
                upstream.shutdown()
                upstream.server_close()
                worker.join(timeout=3)

    def test_retired_subscription_model_hidden_but_external_and_old_routes_preserved(self):
        with tempfile.TemporaryDirectory() as temporary:
            path, config, catalog = self.fixture(Path(temporary))
            catalog["models"].append({"slug": "gpt-5.5", "context_window": 272000})
            Path(config["native_catalog_path"]).write_text(json.dumps(catalog))
            config["providers"] = [{"id": "external", "base_url": "https://example.test/v1"}]
            config["models"] = [{"id": "external/gpt-5.5", "upstream_id": "gpt-5.5", "provider": "external", "context_window": 1000000}]
            config = normalize(config)
            visible = {m["slug"] for m in build_catalog(config)["models"]}
            self.assertNotIn("gpt-5.5", visible); self.assertNotIn("a/gpt-5.5", visible)
            self.assertIn("external/gpt-5.5", visible)
            self.assertNotIn("gpt-5.5", {m["id"] for m in subscription_model_options(config)})
            self.assertEqual(resolve_route(config, "a/gpt-5.5").upstream_model, "gpt-5.5")

    def test_native_export_import_keeps_context_with_imported_account_not_destination_login(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary); source = root / "source"; source.mkdir()
            path, config, _ = self.fixture(source)
            config["native_model_context_windows"] = {"gpt-current": 1000000}
            native_auth = source / "auth.json"; native_auth.write_text(json.dumps({"tokens": {"access_token": "native", "account_id": "native"}}))
            bundle = export_bundle(config, path, "password", ["native"], native_auth)
            target = root / "target/config.json"
            destination = normalize({"native_model_context_windows": {"destination-model": 100000}})
            result, _ = import_bundle(destination, bundle, "password", target)
            self.assertEqual(result["native_model_context_windows"], {"destination-model": 100000})
            self.assertEqual(result["accounts"][0]["model_context_windows"], {"gpt-current": 1000000})

    def test_fetch_accepts_only_rich_model_catalog_and_bounds_response(self):
        response = Mock(status=200, headers={}, read=Mock(side_effect=[b'{"models":[{"slug":"gpt-current","max_context_window":1000000}]}', b'']))
        response.__enter__ = Mock(return_value=response); response.__exit__ = Mock(return_value=False)
        with patch("easy_multi_provider.router.urlopen", return_value=response) as open_url:
            result = fetch_subscription_catalog("https://example.test/codex", {"Authorization": "Bearer private"}, "0.154.0")
        self.assertEqual(result["models"][0]["max_context_window"], 1000000)
        self.assertEqual(open_url.call_args.args[0].full_url, "https://example.test/codex/models?client_version=0.154.0")
        for data in (b'{"data":[]}', b'{"models":[null]}', b'not-json'):
            bad = Mock(status=200, headers={}, read=Mock(side_effect=[data, b'']))
            bad.__enter__ = Mock(return_value=bad); bad.__exit__ = Mock(return_value=False)
            with patch("easy_multi_provider.router.urlopen", return_value=bad):
                with self.assertRaises(RouterError): fetch_subscription_catalog("https://example.test", {}, "0.154.0")
        response.headers = {"Content-Length": str(4 * 1024 * 1024 + 1)}
        with patch("easy_multi_provider.router.urlopen", return_value=response):
            with self.assertRaisesRegex(RouterError, "too large"):
                fetch_subscription_catalog("https://example.test", {}, "0.154.0")
        with patch("easy_multi_provider.router.urlopen", side_effect=TimeoutError("network timeout")):
            with self.assertRaisesRegex(RouterError, "check the network proxy") as caught:
                fetch_subscription_catalog("https://example.test", {}, "0.154.0")
        self.assertEqual(caught.exception.status, 503)
