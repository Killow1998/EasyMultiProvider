"""Bounded local time-series storage for safe Subscription quota metrics."""

from __future__ import annotations

import os
from pathlib import Path
import sqlite3
import threading
import time
from typing import Any, Dict, Iterable, List, Mapping, Optional, Tuple


SAMPLE_INTERVAL_SECONDS = 5 * 60
RETENTION_SECONDS = 15 * 24 * 60 * 60
RANGE_SECONDS = {
    "1h": 60 * 60,
    "1d": 24 * 60 * 60,
    "1w": 7 * 24 * 60 * 60,
    "all": RETENTION_SECONDS,
}


class QuotaHistoryError(ValueError):
    """Raised when quota history cannot be safely read or written."""


def quota_history_path(config_path: Path) -> Path:
    return Path(config_path).resolve().parent / "state" / "quota_history.sqlite3"


def _number(value: Any) -> Optional[float]:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        return None
    return float(value)


def _integer(value: Any) -> Optional[int]:
    number = _number(value)
    return int(number) if number is not None else None


def _rate_limit_buckets(snapshot: Mapping[str, Any]) -> Iterable[Tuple[str, Mapping[str, Any]]]:
    by_limit_id = snapshot.get("rate_limits_by_limit_id")
    if isinstance(by_limit_id, Mapping):
        valid = [
            (str(limit_id), value)
            for limit_id, value in by_limit_id.items()
            if isinstance(limit_id, str) and limit_id and isinstance(value, Mapping)
        ]
        if valid:
            return valid
    rate_limits = snapshot.get("rate_limits")
    if not isinstance(rate_limits, Mapping):
        return ()
    limit_id = rate_limits.get("limitId") or rate_limits.get("limit_id") or "codex"
    return ((str(limit_id), rate_limits),)


def _snapshot_rows(snapshot: Mapping[str, Any]) -> List[Tuple[str, str, Optional[int], float, Optional[int]]]:
    rows: List[Tuple[str, str, Optional[int], float, Optional[int]]] = []
    for limit_id, bucket in _rate_limit_buckets(snapshot):
        for window_kind in ("primary", "secondary"):
            window = bucket.get(window_kind)
            if not isinstance(window, Mapping):
                continue
            used_percent = _number(
                window.get("usedPercent", window.get("used_percent"))
            )
            if used_percent is None:
                continue
            rows.append(
                (
                    limit_id,
                    window_kind,
                    _integer(
                        window.get(
                            "windowDurationMins", window.get("window_minutes")
                        )
                    ),
                    min(100.0, max(0.0, used_percent)),
                    _integer(window.get("resetsAt", window.get("resets_at"))),
                )
            )
    return rows


class QuotaHistoryStore:
    """One bounded SQLite store; never contains credentials or request content."""

    def __init__(self, path: Path):
        self.path = Path(path)
        self._lock = threading.RLock()

    def _connect(self) -> sqlite3.Connection:
        self.path.parent.mkdir(mode=0o700, parents=True, exist_ok=True)
        if self.path.is_symlink():
            raise QuotaHistoryError("quota history path must not be a symlink")
        try:
            connection = sqlite3.connect(str(self.path), timeout=5)
        except sqlite3.Error as exc:
            raise QuotaHistoryError("quota history is unavailable") from exc
        try:
            connection.execute(
                """
                CREATE TABLE IF NOT EXISTS quota_samples (
                    account_key TEXT NOT NULL,
                    observed_at INTEGER NOT NULL,
                    limit_id TEXT NOT NULL,
                    window_kind TEXT NOT NULL,
                    window_minutes INTEGER,
                    used_percent REAL NOT NULL,
                    resets_at INTEGER,
                    PRIMARY KEY (account_key, observed_at, limit_id, window_kind)
                )
                """
            )
            connection.execute(
                "CREATE INDEX IF NOT EXISTS quota_samples_lookup "
                "ON quota_samples(account_key, observed_at)"
            )
            connection.commit()
            try:
                os.chmod(self.path, 0o600)
            except OSError:
                pass
            return connection
        except sqlite3.Error as exc:
            connection.close()
            raise QuotaHistoryError("quota history is unavailable") from exc
        except Exception:
            connection.close()
            raise

    def append_snapshot(
        self,
        account_key: str,
        snapshot: Mapping[str, Any],
        *,
        observed_at: Optional[int] = None,
    ) -> int:
        if not isinstance(account_key, str) or not account_key:
            raise QuotaHistoryError("quota history account is required")
        rows = _snapshot_rows(snapshot)
        if not rows:
            return 0
        timestamp = int(time.time() if observed_at is None else observed_at)
        timestamp -= timestamp % SAMPLE_INTERVAL_SECONDS
        cutoff = timestamp - RETENTION_SECONDS
        with self._lock:
            connection = self._connect()
            try:
                connection.executemany(
                    "INSERT OR REPLACE INTO quota_samples "
                    "(account_key, observed_at, limit_id, window_kind, "
                    "window_minutes, used_percent, resets_at) "
                    "VALUES (?, ?, ?, ?, ?, ?, ?)",
                    [
                        (account_key, timestamp, limit_id, kind, minutes, used, reset)
                        for limit_id, kind, minutes, used, reset in rows
                    ],
                )
                connection.execute(
                    "DELETE FROM quota_samples WHERE observed_at < ?", (cutoff,)
                )
                connection.commit()
            except sqlite3.Error as exc:
                raise QuotaHistoryError("quota history is unavailable") from exc
            finally:
                connection.close()
        return len(rows)

    def query(
        self,
        account_key: str,
        range_name: str,
        *,
        now: Optional[int] = None,
    ) -> Dict[str, Any]:
        if range_name not in RANGE_SECONDS:
            raise QuotaHistoryError("unsupported quota history range")
        timestamp = int(time.time() if now is None else now)
        cutoff = timestamp - RANGE_SECONDS[range_name]
        grouped: Dict[Tuple[str, str], Dict[str, Any]] = {}
        if self.path.exists():
            with self._lock:
                if self.path.is_symlink():
                    raise QuotaHistoryError("quota history path must not be a symlink")
                try:
                    connection = sqlite3.connect(str(self.path), timeout=5)
                except sqlite3.Error as exc:
                    raise QuotaHistoryError("quota history is unavailable") from exc
                try:
                    rows = connection.execute(
                        "SELECT observed_at, limit_id, window_kind, window_minutes, "
                        "used_percent, resets_at FROM quota_samples "
                        "WHERE account_key = ? AND observed_at >= ? "
                        "ORDER BY observed_at, limit_id, window_kind",
                        (account_key, cutoff),
                    ).fetchall()
                except sqlite3.Error as exc:
                    raise QuotaHistoryError("quota history is unavailable") from exc
                finally:
                    connection.close()
            for observed_at, limit_id, kind, minutes, used, resets_at in rows:
                key = (str(limit_id), str(kind))
                series = grouped.setdefault(
                    key,
                    {
                        "limit_id": key[0],
                        "window_kind": key[1],
                        "window_minutes": minutes,
                        "points": [],
                    },
                )
                series["points"].append(
                    {
                        "observed_at": int(observed_at),
                        "remaining_percent": round(
                            100.0 - min(100.0, max(0.0, float(used))), 2
                        ),
                        "resets_at": int(resets_at) if resets_at is not None else None,
                    }
                )
        return {
            "range": range_name,
            "sample_interval_seconds": SAMPLE_INTERVAL_SECONDS,
            "retention_days": RETENTION_SECONDS // (24 * 60 * 60),
            "series": list(grouped.values()),
        }

    def delete_account(self, account_key: str) -> None:
        if not self.path.exists():
            return
        with self._lock:
            if self.path.is_symlink():
                raise QuotaHistoryError("quota history path must not be a symlink")
            try:
                connection = sqlite3.connect(str(self.path), timeout=5)
            except sqlite3.Error as exc:
                raise QuotaHistoryError("quota history is unavailable") from exc
            try:
                connection.execute(
                    "DELETE FROM quota_samples WHERE account_key = ?", (account_key,)
                )
                connection.commit()
            except sqlite3.Error as exc:
                raise QuotaHistoryError("quota history is unavailable") from exc
            finally:
                connection.close()


__all__ = [
    "QuotaHistoryError",
    "QuotaHistoryStore",
    "RANGE_SECONDS",
    "RETENTION_SECONDS",
    "SAMPLE_INTERVAL_SECONDS",
    "quota_history_path",
]
