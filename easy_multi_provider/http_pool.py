"""HTTP connection reuse without automatic request replay or redirects."""

import atexit
import threading
import time
from collections import OrderedDict
from urllib.error import HTTPError, URLError
from urllib.parse import unquote, urlsplit, urlunsplit

import urllib3

from .network_proxy import proxy_for_url
from .router_errors import RouterError


MAX_SSE_LINE_BYTES = 1024 * 1024


_lock = threading.Lock()
_managers = OrderedDict()


def close_pools():
    with _lock:
        managers = list(_managers.values())
        _managers.clear()
    for manager in managers:
        manager.clear()


atexit.register(close_pools)


def _manager(proxy):
    with _lock:
        manager = _managers.get(proxy)
        if manager is None:
            settings = dict(
                num_pools=16, maxsize=32, block=False,
                # urllib3 creates a verified context for each new connection.
                # A shared truststore context has mutable handshake state on
                # Windows; simultaneous handshakes must not share that state.
                cert_reqs="CERT_REQUIRED", retries=False,
            )
            if proxy and proxy.startswith(("socks4", "socks5")):
                from urllib3.contrib.socks import SOCKSProxyManager
                manager = SOCKSProxyManager(proxy, **settings)
            elif proxy:
                parsed = urlsplit(proxy)
                proxy_url = proxy
                proxy_headers = {}
                if parsed.username is not None:
                    credentials = unquote(parsed.username) + ":" + unquote(parsed.password or "")
                    proxy_headers = urllib3.make_headers(proxy_basic_auth=credentials)
                    # Userinfo belongs to the proxy only, never the origin URL.
                    proxy_url = urlunsplit(parsed._replace(netloc=parsed.netloc.rsplit("@", 1)[1]))
                manager = urllib3.ProxyManager(proxy_url, proxy_headers=proxy_headers, **settings)
            else:
                manager = urllib3.PoolManager(**settings)
            _managers[proxy] = manager
            if len(_managers) > 4:
                _, old = _managers.popitem(last=False)
                old.clear()
        _managers.move_to_end(proxy)
        return manager


class Response:
    """Keep the router's file-like API and yield SSE lines as they arrive."""

    def __init__(self, response):
        self._response = response
        self.status = response.status
        self.headers = response.headers
        self._buffer = bytearray()

    def settimeout(self, timeout):
        connection = self._response.connection
        if connection is not None and connection.sock is not None:
            connection.sock.settimeout(timeout)

    def read(self, size=-1):
        try:
            return self._response.read(None if size < 0 else size)
        except urllib3.exceptions.TimeoutError as exc:
            raise TimeoutError("upstream read timed out") from exc
        except urllib3.exceptions.HTTPError as exc:
            raise URLError(exc) from exc

    def __iter__(self):
        return self

    def __next__(self):
        scanned = 0
        while True:
            end = self._buffer.find(b"\n", scanned)
            if end >= 0:
                if end + 1 > MAX_SSE_LINE_BYTES:
                    self.close()
                    self._buffer.clear()
                    raise RouterError("upstream SSE line is too large", 502)
                line = bytes(self._buffer[:end + 1])
                del self._buffer[:end + 1]
                return line
            if len(self._buffer) > MAX_SSE_LINE_BYTES:
                self.close()
                self._buffer.clear()
                raise RouterError("upstream SSE line is too large", 502)
            scanned = len(self._buffer)
            try:
                chunk = self._response.read1(min(8192, MAX_SSE_LINE_BYTES + 1 - scanned))
            except urllib3.exceptions.TimeoutError as exc:
                self.close()
                raise TimeoutError("upstream read timed out") from exc
            except urllib3.exceptions.HTTPError as exc:
                self.close()
                raise URLError(exc) from exc
            if not chunk:
                if not self._buffer:
                    raise StopIteration
                line = bytes(self._buffer)
                self._buffer.clear()
                return line
            self._buffer.extend(chunk)

    def close(self):
        # Unread/cancelled responses are discarded, never given to another request.
        self._response.close()
        self._response.release_conn()

    def finish(self):
        # A validated SSE terminal can precede HTTP's final chunk. Consume only
        # a small trailer, with a short cleanup deadline after output delivery.
        # A peer that leaves the body open loses reuse after this bounded cleanup.
        deadline = time.monotonic() + 0.1
        remaining = 64 * 1024
        try:
            while self._response.connection is not None and remaining > 0:
                budget = deadline - time.monotonic()
                if budget <= 0:
                    break
                self.settimeout(budget)
                chunk = self._response.read1(min(8192, remaining))
                if not chunk:
                    break
                remaining -= len(chunk)
        except (OSError, urllib3.exceptions.HTTPError):
            pass
        finally:
            self.close()

    def __enter__(self):
        return self

    def __exit__(self, *args):
        self.close()


def open_request(request, timeout):
    manager = _manager(proxy_for_url(request.full_url))
    headers = dict(request.header_items())
    headers.setdefault("Accept-Encoding", "identity")
    try:
        response = manager.request(
            request.get_method(), request.full_url, body=request.data, headers=headers,
            timeout=timeout, retries=False, redirect=False,
            preload_content=False, decode_content=False,
        )
    except urllib3.exceptions.TimeoutError as exc:
        raise TimeoutError("upstream connection timed out") from exc
    except urllib3.exceptions.HTTPError as exc:
        raise URLError(exc) from exc
    result = Response(response)
    if 300 <= result.status < 400:
        result.close()
        raise URLError("upstream redirects are disabled")
    if result.status >= 400:
        raise HTTPError(request.full_url, result.status, response.reason, result.headers, result)
    return result
