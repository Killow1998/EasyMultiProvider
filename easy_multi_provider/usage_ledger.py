"""Persistent content-free usage, independent of the rolling diagnostic log."""

from __future__ import annotations

import hashlib
import json
import math
import os
import sqlite3
import threading
import time
import uuid
from pathlib import Path

from .performance import token_count
from .route_plan import resolved_upstream_model
from .codex_history import HistoryError
from .history_continuity import request_history_anchor

TOKEN_FIELDS = ("input_tokens", "output_tokens", "cached_input_tokens", "cache_write_tokens",
                "cache_write_1h_tokens", "reasoning_tokens")
CATEGORIES = ("native", "subscription", "external", "unknown")


def usage_context(body, headers):
    try:
        anchor = request_history_anchor(body, headers)
        return {"usage_turn": anchor.turn_id or "", "route_model": str(body.get("model") or "")}
    except HistoryError:
        return {}  # unknown correlation must never break model forwarding


def usage_account_owner(headers):
    """Opaque identity from the credentials actually selected for this request."""
    lower = {key.lower(): value for key, value in headers.items() if isinstance(key, str)}
    account = lower.get("chatgpt-account-id")
    if isinstance(account, str) and account.strip() and len(account) <= 512:
        return "account:" + hashlib.sha256(b"emp-usage-account\0" + account.strip().encode()).hexdigest()
    authorization = lower.get("authorization")
    if isinstance(authorization, str) and authorization.strip():
        return "credential:" + hashlib.sha256(b"emp-usage-credential\0" + authorization.strip().encode()).hexdigest()
    return ""


def usage_identity(provider, model, headers=None):
    mode = provider.get("auth_mode")
    category = "subscription" if mode == "account" else "native" if mode in ("forward", "native") else "external"
    owner = str(provider.get("id") or "")
    if category in ("native", "subscription"):
        owner = usage_account_owner(headers) if headers is not None else provider.get("_usage_owner", "")
        owner = owner or "unconfirmed:" + str(provider.get("id") or "")
    return {"usage_category": category, "usage_owner": owner,
            "upstream_model": resolved_upstream_model(provider, model, str(model.get("id") or ""))}


class UsageLedger:
    def __init__(self, path: Path, prices, journal):
        self.path, self.prices, self.journal = Path(path), prices, journal
        self._lock = threading.RLock()
        self.write_error = False
        self._initialized = False

    def _connect(self):
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if self.path.is_symlink():
            raise OSError("Usage ledger must not be a symlink")
        connection = sqlite3.connect(str(self.path), timeout=5)
        connection.row_factory = sqlite3.Row
        try:
            if self._initialized:
                return connection
            connection.execute("""CREATE TABLE IF NOT EXISTS usage_events (
                id TEXT PRIMARY KEY, observed_at REAL NOT NULL,
                category TEXT NOT NULL, owner TEXT NOT NULL, model TEXT NOT NULL,
                service_tier TEXT NOT NULL, operation TEXT NOT NULL,
                input_tokens INTEGER, output_tokens INTEGER, cached_input_tokens INTEGER,
                cache_write_tokens INTEGER, cache_write_1h_tokens INTEGER, reasoning_tokens INTEGER,
                cost_nanos INTEGER, price_issue TEXT, price_key TEXT, price_fetched_at REAL,
                price_revision TEXT, rates TEXT NOT NULL
            )""")
            connection.execute("CREATE INDEX IF NOT EXISTS usage_period ON usage_events(observed_at)")
            columns = {row[1] for row in connection.execute("PRAGMA table_info(usage_events)")}
            if "origin" not in columns:
                if connection.execute("SELECT 1 FROM usage_events LIMIT 1").fetchone():
                    # SQLite backup also captures committed WAL data. Never
                    # overwrite the pre-upgrade copy on a retry.
                    backup_path = self.path.with_suffix(".pre-v011.sqlite3")
                    if not backup_path.exists():
                        backup = sqlite3.connect(str(backup_path))
                        try:
                            connection.backup(backup)
                        finally:
                            backup.close()
                        if os.name != "nt":
                            os.chmod(backup_path, 0o600)
                with connection:
                    for name, default in (("origin", "realtime"), ("usage_turn", ""),
                                          ("route_model", ""), ("match_key", "")):
                        connection.execute("ALTER TABLE usage_events ADD COLUMN %s TEXT NOT NULL DEFAULT '%s'" % (name, default))
            connection.execute("CREATE INDEX IF NOT EXISTS usage_match ON usage_events(match_key, origin)")
            connection.execute("CREATE INDEX IF NOT EXISTS usage_turn ON usage_events(usage_turn, route_model, origin)")
            connection.execute("CREATE TABLE IF NOT EXISTS usage_files (path TEXT PRIMARY KEY, checkpoint TEXT NOT NULL)")
            connection.execute("DROP VIEW IF EXISTS accounted_usage")
            connection.execute("""CREATE VIEW accounted_usage AS
                SELECT h.* FROM usage_events h WHERE h.origin = 'realtime' OR h.match_key = ''
                OR NOT EXISTS (SELECT 1 FROM usage_events r WHERE r.origin='realtime' AND r.match_key=h.match_key)
                OR (SELECT COUNT(*) FROM usage_events r WHERE r.origin='realtime' AND r.match_key=h.match_key)
                 < (SELECT COUNT(*) FROM usage_events p WHERE p.origin='history' AND p.match_key=h.match_key
                    AND (p.observed_at<h.observed_at OR (p.observed_at=h.observed_at AND p.id<=h.id)))
                """)
            connection.commit()
            if os.name != "nt":
                os.chmod(self.path, 0o600)
            self._initialized = True
            return connection
        except Exception:
            connection.close()
            raise

    def _values(self, event, observed_at=None):
        category, owner, model = event["usage_category"], event.get("usage_owner", ""), event.get("upstream_model", "")
        response_id = event.get("usage_response_id")
        identity = json.dumps([category, owner, model, event.get("route"), response_id])
        row_id = hashlib.sha256(identity.encode()).hexdigest() if response_id else str(event.get("observation_id") or uuid.uuid4().hex)
        quote = self.prices.quote(event)
        if event.get("usage_issue"):
            quote.update(cost_nanos=None, price_issue=event["usage_issue"], rates={})
        values = [row_id, time.time() if observed_at is None else observed_at, category, owner, model,
                  str(event.get("service_tier") or "default"), event["route"]]
        values += [token_count(event.get(field)) for field in TOKEN_FIELDS]
        values += [quote[key] for key in ("cost_nanos", "price_issue", "price_key", "price_fetched_at", "price_revision")]
        values.append(json.dumps(quote["rates"], sort_keys=True))
        turn, route_model = event.get("usage_turn", ""), event.get("route_model", "")
        counts = [token_count(event.get(key)) for key in ("input_tokens", "output_tokens", "cached_input_tokens")]
        match = hashlib.sha256(json.dumps([turn, route_model, counts]).encode()).hexdigest() if turn and route_model and None not in counts else ""
        return values + [event.get("origin", "realtime"), turn, route_model, match]

    @staticmethod
    def _insert(connection, values):
        connection.execute("INSERT OR IGNORE INTO usage_events VALUES (" + ",".join("?" for _ in values) + ")", values)

    def record(self, event, observed_at=None):
        """One final observation per upstream response, including failed paid work."""
        if event.get("route") not in ("responses", "compact") or event.get("usage_category") not in CATEGORIES:
            return
        if event.get("recovery_mode") == "native_http_fallback":
            return  # handshake failure before request send, followed by HTTP
        try:
            values = self._values(event, observed_at)
            with self._lock:
                connection = self._connect()
                try:
                    with connection:
                        self._insert(connection, values)
                finally:
                    connection.close()
        except (OSError, sqlite3.Error, ValueError, TypeError, ArithmeticError):
            if not self.write_error:
                self.journal.event("error", "usage_ledger_write_failed")
            self.write_error = True

    def history_checkpoint(self, path):
        with self._lock:
            connection = self._connect()
            try:
                row = connection.execute("SELECT checkpoint FROM usage_files WHERE path=?", (path,)).fetchone()
                return json.loads(row[0]) if row else None
            finally:
                connection.close()

    def record_history(self, events, path, checkpoint):
        values = [self._values(event, observed_at) for event, observed_at in events]
        with self._lock:
            connection = self._connect()
            try:
                with connection:
                    for row in values:
                        self._insert(connection, row)
                    connection.execute("INSERT OR REPLACE INTO usage_files VALUES (?,?)", (path, json.dumps(checkpoint)))
            finally:
                connection.close()

    def query(self, start, end, category="all"):
        start, end = float(start), float(end)
        if not (math.isfinite(start) and math.isfinite(end) and 0 <= start < end):
            raise ValueError("Invalid usage period")
        if category not in ("all",) + CATEGORIES:
            raise ValueError("Invalid usage category")
        where = "observed_at >= ? AND observed_at < ?"
        parameters = [start, end]
        if category != "all":
            where += " AND category = ?"
            parameters.append(category)
        aggregates = ", ".join("SUM(COALESCE(%s,0)) AS %s" % (field, field) for field in TOKEN_FIELDS)
        aggregates += (", COUNT(*) AS requests, SUM(input_tokens IS NOT NULL AND output_tokens IS NOT NULL) AS reported_requests, "
                       "SUM(input_tokens IS NOT NULL) AS input_reports, SUM(output_tokens IS NOT NULL) AS output_reports, "
                       "SUM(cached_input_tokens IS NOT NULL) AS cache_reports, SUM(reasoning_tokens IS NOT NULL) AS reasoning_reports, "
                       "SUM(cost_nanos IS NOT NULL) AS priced_requests, SUM(COALESCE(cost_nanos,0)) AS cost_nanos")
        with self._lock:
            connection = self._connect()
            try:
                # Reconcile once for this query, not once per chart/table. This
                # temporary numeric projection disappears with the connection.
                fields = "observed_at,category,owner,model,service_tier,origin,usage_turn,route_model,price_issue,cost_nanos," + ",".join(TOKEN_FIELDS)
                connection.execute("PRAGMA temp_store=MEMORY")
                connection.execute("CREATE TEMP TABLE selected_usage AS SELECT " + fields + " FROM accounted_usage WHERE " + where, parameters)
                def rows(group):
                    return [dict(row) for row in connection.execute(
                        "SELECT " + group + ", " + aggregates + " FROM selected_usage WHERE " + where + " GROUP BY " + group, parameters)]
                groups = rows("category, owner, model, service_tier")
                # Minute bins preserve local hour/day boundaries even in
                # half-hour and quarter-hour timezones. The UI groups these.
                periods = [dict(row) for row in connection.execute(
                    "SELECT CAST(observed_at / 60 AS INTEGER) * 60 AS start, "
                    "SUM(COALESCE(input_tokens,0)) AS input_tokens, "
                    "SUM(COALESCE(output_tokens,0)) AS output_tokens, "
                    "SUM(COALESCE(cost_nanos,0)) AS cost_nanos, "
                    "COUNT(*) AS requests, SUM(cost_nanos IS NOT NULL) AS priced_requests "
                    "FROM selected_usage WHERE " + where + " GROUP BY start ORDER BY start", parameters)]
                issues = [dict(row) for row in connection.execute(
                    "SELECT price_issue, COUNT(*) AS requests FROM selected_usage WHERE " + where +
                    " AND price_issue IS NOT NULL GROUP BY price_issue", parameters)]
                first = connection.execute("SELECT MIN(observed_at) FROM usage_events").fetchone()[0]
                sources = rows("origin")
                uncorrelated = connection.execute("SELECT COUNT(*) FROM selected_usage WHERE origin='realtime' AND usage_turn='' ").fetchone()[0]
                unmatched = connection.execute("""SELECT COUNT(*) FROM selected_usage h
                    WHERE h.origin='history' AND h.observed_at>=? AND h.observed_at<?
                    AND h.usage_turn != '' AND EXISTS (SELECT 1 FROM usage_events r
                        WHERE r.origin='realtime' AND r.usage_turn=h.usage_turn AND r.route_model=h.route_model)
                    """, (start, end)).fetchone()[0]
            finally:
                connection.close()
        fields = TOKEN_FIELDS + ("requests", "reported_requests", "priced_requests", "cost_nanos", "input_reports", "output_reports")
        totals = {field: sum(row[field] for row in groups) for field in fields}
        return {"start": start, "end": end, "category": category, "totals": totals, "groups": groups,
                "periods": periods, "issues": issues, "first_record_at": first,
                "sources": sources, "unmatched_overlap": unmatched, "uncorrelated_realtime": uncorrelated,
                "write_error": self.write_error, "pricing": self.prices.snapshot(), "currency": "USD"}

    def price_pending(self):
        """Fill missing rates after a catalog update; never reprice known costs."""
        try:
            while True:
                with self._lock:
                    connection = self._connect()
                    try:
                        pending = connection.execute(
                            "SELECT * FROM usage_events WHERE cost_nanos IS NULL "
                            "AND price_issue IN ('unknown_model','missing_rate','unknown_tier') "
                            "AND input_tokens IS NOT NULL AND output_tokens IS NOT NULL "
                            "AND price_revision != ? LIMIT 200", (self.prices.revision,)).fetchall()
                        if not pending:
                            return
                        with connection:
                            for row in pending:
                                event = {**dict(row), "upstream_model": row["model"]}
                                # NULL represents an unreported optional subset, not
                                # an invalid explicitly reported count on this path.
                                for field in ("cache_write_tokens", "cache_write_1h_tokens", "reasoning_tokens"):
                                    if event[field] is None:
                                        event.pop(field)
                                quote = self.prices.quote(event)
                                connection.execute(
                                    "UPDATE usage_events SET cost_nanos=?, price_issue=?, price_key=?, "
                                    "price_fetched_at=?, price_revision=?, rates=? WHERE id=? AND cost_nanos IS NULL",
                                    [quote[key] for key in ("cost_nanos", "price_issue", "price_key", "price_fetched_at", "price_revision")]
                                    + [json.dumps(quote["rates"], sort_keys=True), row["id"]])
                    finally:
                        connection.close()
        except (OSError, sqlite3.Error, ValueError, TypeError, ArithmeticError):
            self.write_error = True
            self.journal.event("error", "usage_pending_price_failed")
