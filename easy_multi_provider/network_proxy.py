"""Resolve the active proxy at connection time without changing global environment."""
import hashlib
import ipaddress
from urllib.parse import urlparse
from urllib.request import getproxies, proxy_bypass_environment

_system_reader = None


def follow_system_proxy(reader):
    global _system_reader
    _system_reader = reader


def current_proxies():
    settings = dict(_system_reader() if _system_reader is not None else getproxies())
    settings = {key: value for key, value in settings.items() if isinstance(value, str) and value}
    settings["no"] = ",".join(dict.fromkeys((settings.get("no", "") + ",localhost,127.0.0.1,::1").strip(",").split(",")))
    return settings


def is_loopback(url):
    host = (urlparse(url).hostname or "").rstrip(".").lower()
    try:
        return host == "localhost" or ipaddress.ip_address(host).is_loopback
    except ValueError:
        return False


def proxy_for_url(url):
    if is_loopback(url):
        return None
    settings = current_proxies()
    parsed = urlparse(url)
    if proxy_bypass_environment(parsed.netloc, settings):
        return None
    keys = ("wss", "socks", "https", "all") if parsed.scheme in ("wss", "https") else ("ws", "socks", "https", "http", "all")
    for key in keys:
        proxy = settings.get(key)
        if proxy:
            if key == "socks" and proxy.startswith("http://"):
                proxy = "socks5h://" + proxy[7:]
            return proxy
    return None


def proxy_identity(url):
    return hashlib.sha256((proxy_for_url(url) or "direct").encode()).hexdigest()
