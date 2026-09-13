"""Daily public API prices and per-request token estimates (USD, not invoices)."""

from __future__ import annotations

import hashlib
import json
import re
import threading
import time
from decimal import Decimal, ROUND_HALF_UP
from pathlib import Path
from urllib.request import ProxyHandler, Request, build_opener

from .integration import atomic_write_text
from .network_proxy import current_proxies
from .performance import token_count

PRICE_URL = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json"
PRICE_INTERVAL = 24 * 60 * 60
_MAX_PRICE_BYTES = 16 * 1024 * 1024
_RATE = re.compile(r"^(?:input_cost_per_token|output_cost_per_token|output_cost_per_reasoning_token|cache_read_input_token_cost|cache_creation_input_token_cost)(?:_above_\d+k_tokens|_above_1hr)?(?:_priority|_flex)?$")


def normalize_prices(payload):
    if not isinstance(payload, dict):
        raise ValueError("Invalid price catalog")
    prices = {}
    for model, entry in payload.items():
        if not isinstance(model, str) or not isinstance(entry, dict):
            continue
        rates = {}
        for key, value in entry.items():
            if not _RATE.fullmatch(key):
                continue
            if type(value) not in (int, float, str):
                break
            try:
                number = Decimal(str(value))
            except ArithmeticError:
                break
            if not number.is_finite() or number < 0:
                break
            rates[key] = str(number)
        else:
            if "input_cost_per_token" in rates and "output_cost_per_token" in rates:
                prices[model] = rates
    if not prices:
        raise ValueError("No valid token prices")
    return prices


def model_price_key(model, prices):
    """Match real upstream IDs; never guess from display aliases or fuzzy names."""
    if model in prices:
        return model
    for prefix, family in (("gemini-", "gemini"), ("claude-", "anthropic"),
                           ("gpt-", "openai"), ("deepseek-", "deepseek"), ("grok-", "xai")):
        key = family + "/" + model
        if model.startswith(prefix) and key in prices:
            return key
    return None


def estimate_tokens(usage, rates, service_tier="default"):
    """Input includes cache reads/writes; output includes reasoning.

    Context tiers apply to the whole request. Missing cache reports or prices
    stay unknown instead of silently charging cached tokens at the input rate.
    """
    total, output = token_count(usage.get("input_tokens")), token_count(usage.get("output_tokens"))
    if total is None or output is None:
        return None, "missing_usage", {}
    if not isinstance(service_tier, str):
        return None, "unknown_tier", {}
    suffix = {"default": "", "auto": "", "standard": "", "fast": "_priority",
              "priority": "_priority", "flex": "_flex"}.get(service_tier)
    if suffix is None:
        return None, "unknown_tier", {}
    def rate(key):
        # Each component can have a different context threshold.
        pattern = re.compile(re.escape(key) + r"_above_(\d+)k_tokens" + re.escape(suffix))
        thresholds = [int(match.group(1)) * 1000 for candidate in rates
                      for match in [pattern.fullmatch(candidate)] if match]
        threshold = max((value for value in thresholds if total > value), default=0)
        context = "_above_%dk_tokens" % (threshold // 1000) if threshold else ""
        value = rates.get(key + context + suffix, rates.get(key + suffix))
        return Decimal(value) if value is not None else None

    cached = token_count(usage.get("cached_input_tokens"))
    written = token_count(usage.get("cache_write_tokens", 0))
    written_hour = token_count(usage.get("cache_write_1h_tokens", 0))
    reasoning = token_count(usage.get("reasoning_tokens", 0))
    if (written is None or written_hour is None or reasoning is None
            or written_hour > written or reasoning > output):
        return None, "invalid_usage", {}
    if cached is None:
        if total and rate("cache_read_input_token_cost") is not None:
            return None, "missing_cache_usage", {}
        cached = 0
    if cached + written > total:
        return None, "invalid_usage", {}
    reasoning_rate = rate("output_cost_per_reasoning_token")
    if (output and "reasoning_tokens" not in usage and reasoning_rate is not None
            and reasoning_rate != rate("output_cost_per_token")):
        return None, "missing_reasoning_usage", {}
    components = (
        ("input", total - cached - written, rate("input_cost_per_token")),
        ("cache_read", cached, rate("cache_read_input_token_cost")),
        ("cache_write", written - written_hour, rate("cache_creation_input_token_cost")),
        ("cache_write_1h", written_hour, rate("cache_creation_input_token_cost_above_1hr")),
        ("output", output - reasoning, rate("output_cost_per_token")),
        ("reasoning", reasoning, reasoning_rate if reasoning_rate is not None else rate("output_cost_per_token")),
    )
    if any(count and price is None for _, count, price in components):
        return None, "missing_rate", {}
    cost = sum((Decimal(count) * (price or Decimal(0)) for _, count, price in components), Decimal(0))
    applied = {key: str(price) for key, _, price in components if price is not None}
    return int((cost * 1_000_000_000).quantize(Decimal(1), rounding=ROUND_HALF_UP)), None, applied


class PriceCatalog:
    def __init__(self, path: Path, journal):
        self.path, self.journal = Path(path), journal
        self._lock = threading.RLock()
        self._stop = threading.Event()
        self._thread = None
        self._on_update = None
        self.prices, self.fetched_at, self.revision = {}, 0, ""
        self.error = None
        try:
            saved = json.loads(self.path.read_text(encoding="utf-8"))
            fetched = saved["fetched_at"]
            if type(fetched) not in (int, float) or not 0 < fetched <= time.time():
                raise ValueError("Invalid price timestamp")
            self._set(normalize_prices(saved["prices"]), fetched)
        except (OSError, ValueError, KeyError, TypeError):
            pass

    def _set(self, prices, fetched_at):
        encoded = json.dumps(prices, sort_keys=True, separators=(",", ":"))
        with self._lock:
            self.prices, self.fetched_at = prices, fetched_at
            self.revision = hashlib.sha256(encoded.encode()).hexdigest()
            self.error = None

    def refresh(self):
        """Download prices only; no model usage or account data leaves EMP."""
        try:
            request = Request(PRICE_URL, headers={"User-Agent": "EMP-price-catalog", "Accept": "application/json"})
            with build_opener(ProxyHandler(current_proxies())).open(request, timeout=20) as response:
                raw = response.read(_MAX_PRICE_BYTES + 1)
            if len(raw) > _MAX_PRICE_BYTES:
                raise ValueError("Price catalog is too large")
            prices = normalize_prices(json.loads(raw))
            fetched_at = time.time()
            atomic_write_text(self.path, json.dumps({"fetched_at": fetched_at, "prices": prices}))
            self._set(prices, fetched_at)
            return True
        except Exception as exc:
            with self._lock:
                self.error = "refresh_failed"
            self.journal.event("warning", "usage_price_refresh_failed", exception_type=type(exc).__name__)
            return False

    def quote(self, event):
        with self._lock:
            key = model_price_key(event.get("upstream_model", ""), self.prices)
            fetched, revision = self.fetched_at, self.revision
            rates = self.prices.get(key)
        if rates is None:
            _, issue, _ = estimate_tokens(event, {}, event.get("service_tier") or "default")
            return {"cost_nanos": None, "price_issue": issue if issue in ("missing_usage", "invalid_usage") else "unknown_model", "price_key": None,
                    "price_fetched_at": fetched, "price_revision": revision, "rates": {}}
        cost, issue, applied = estimate_tokens(event, rates, event.get("service_tier") or "default")
        return {"cost_nanos": cost, "price_issue": issue, "price_key": key,
                "price_fetched_at": fetched, "price_revision": revision, "rates": applied}

    def snapshot(self):
        with self._lock:
            return {"source": "LiteLLM", "url": PRICE_URL, "fetched_at": self.fetched_at or None,
                    "stale": time.time() - self.fetched_at >= PRICE_INTERVAL,
                    "error": self.error, "model_count": len(self.prices), "interval_hours": 24}

    def _run(self):
        if self.prices and self._on_update is not None:
            self._on_update()
        while not self._stop.is_set():
            delay = max(0, self.fetched_at + PRICE_INTERVAL - time.time())
            if self._stop.wait(delay):
                return
            if self.refresh():
                if self._on_update is not None:
                    self._on_update()
            elif self._stop.wait(3600):
                return

    def start(self, on_update=None):
        if self._thread is None:
            self._on_update = on_update
            self._stop.clear()
            self._thread = threading.Thread(target=self._run, daemon=True, name="emp-api-prices")
            self._thread.start()

    def stop(self):
        self._stop.set()
        if self._thread is not None:
            self._thread.join(timeout=1)
