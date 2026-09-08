"""Local providers remain reachable while a proxy changes routing modes."""
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from unittest.mock import patch
from urllib.request import Request

from easy_multi_provider.router import urlopen
from easy_multi_provider.server import configure_proxy_environment


class LoopbackProxyTests(unittest.TestCase):
    def test_running_client_keeps_local_provider_direct_after_proxy_change(self):
        proxied = []
        class Local(BaseHTTPRequestHandler):
            def do_GET(self):
                self.send_response(200)
                self.end_headers()
                self.wfile.write(b"local")
            def log_message(self, *args):
                pass
        class GlobalProxy(Local):
            def do_GET(self):
                proxied.append(self.path)
                self.send_error(502)
        local = ThreadingHTTPServer(("127.0.0.1", 0), Local)
        proxy = ThreadingHTTPServer(("127.0.0.1", 0), GlobalProxy)
        threads = [threading.Thread(target=s.serve_forever, daemon=True) for s in (local, proxy)]
        for thread in threads:
            thread.start()
        try:
            with patch.dict(os.environ, {}, clear=True):
                request = Request("http://127.0.0.1:%d/v1/models" % local.server_port)
                with urlopen(request, timeout=2) as response:
                    self.assertEqual(response.read(), b"local")
                os.environ["http_proxy"] = "http://127.0.0.1:%d" % proxy.server_port
                with urlopen(request, timeout=2) as response:
                    self.assertEqual(response.read(), b"local")
                self.assertEqual(proxied, [])
                with self.assertRaises(Exception):
                    urlopen(Request("http://remote.invalid/v1/models"), timeout=2)
                self.assertEqual(len(proxied), 1)
        finally:
            for server in (local, proxy):
                server.shutdown()
                server.server_close()
            for thread in threads:
                thread.join()

    def test_explicit_proxy_retains_custom_bypass_and_adds_loopback(self):
        with patch.dict(os.environ, {"HTTPS_PROXY": "http://127.0.0.1:7890", "NO_PROXY": "internal.example"}, clear=True):
            self.assertEqual(configure_proxy_environment(), "environment")
            self.assertEqual(set(os.environ["no_proxy"].split(",")),
                             {"localhost", "127.0.0.1", "::1", "internal.example"})
            self.assertEqual(os.environ["NO_PROXY"], os.environ["no_proxy"])


class LiveSystemProxyTests(unittest.TestCase):
    def tearDown(self):
        from easy_multi_provider.network_proxy import follow_system_proxy
        follow_system_proxy(None)

    def test_new_connections_follow_system_proxy_changes_without_restarting(self):
        from easy_multi_provider.network_proxy import follow_system_proxy, current_proxies, proxy_identity
        settings = {"https": "http://127.0.0.1:7890"}
        follow_system_proxy(lambda: settings)
        before = proxy_identity("wss://upstream.invalid/v1/responses")
        self.assertEqual(current_proxies()["https"], settings["https"])
        settings["https"] = "http://127.0.0.1:7897"
        self.assertEqual(current_proxies()["https"], settings["https"])
        self.assertNotEqual(before, proxy_identity("wss://upstream.invalid/v1/responses"))
        settings.clear()
        self.assertNotIn("https", current_proxies())

    def test_internal_request_id_is_forwarded_to_provider(self):
        from easy_multi_provider.router import _headers
        with patch("easy_multi_provider.router.api_key", return_value="fixture"):
            headers = _headers({"auth_mode": "api_key", "id": "na2h"}, {"X-EMP-Request-ID": "0123456789abcdef"}, False)
            self.assertEqual(headers["X-EMP-Request-ID"], "0123456789abcdef")
            headers = _headers({"auth_mode": "api_key", "id": "na2h"}, {"X-EMP-Request-ID": "invalid-user-value"}, False)
            self.assertNotIn("X-EMP-Request-ID", headers)
