"""Per-request size growth with shared memory reservations and safe notices."""

from collections import deque
import threading
import time
import uuid

import psutil

from .transport import MAX_PROXY_REQUEST_BYTES, RequestBodyTooLarge


MAX_EXPANDED_REQUEST_BYTES = 1024 * 1024 * 1024
# JSON parsing, UTF-8 strings, routing copies and compression can coexist.
MEMORY_RESERVATION_FACTOR = 8


class RequestBudget:
    def __init__(self, owner, transport):
        self.owner = owner
        self.transport = transport
        self.limit = owner.baseline
        self.reserved = 0
        self.closed = False

    def ensure(self, size):
        self.owner.ensure(self, size)

    def release(self):
        with self.owner.lock:
            if not self.closed:
                self.owner.reserved -= self.reserved
                self.reserved = 0
                self.closed = True


class RequestLimits:
    def __init__(self, journal=None, *, baseline=MAX_PROXY_REQUEST_BYTES,
                 maximum=MAX_EXPANDED_REQUEST_BYTES, available_memory=None):
        if baseline <= 0 or maximum < baseline:
            raise ValueError("invalid request growth limits")
        self.baseline = baseline
        self.maximum = maximum
        self.available_memory = available_memory or (lambda: psutil.virtual_memory().available)
        self.journal = journal
        self.lock = threading.Lock()
        self.reserved = 0
        self.notices = deque(maxlen=20)
        self.sequence = 0
        self.run_id = uuid.uuid4().hex

    def request(self, transport):
        return RequestBudget(self, transport)

    def ensure(self, budget, size):
        with self.lock:
            if budget.closed:
                raise RuntimeError("request budget is already released")
            if size <= budget.limit:
                return
            previous = budget.limit
            target = previous
            while target < size and target < self.maximum:
                target = min(target * 2, self.maximum)
            reason = "hard_limit" if size > self.maximum else ""
            if not reason:
                try:
                    available = max(0, int(self.available_memory()))
                except (OSError, ValueError, TypeError, psutil.Error):
                    available = 0
                needed = target * MEMORY_RESERVATION_FACTOR
                # Leave half of current free memory alone, and subtract other
                # expanded requests' reservations under the same lock.
                if needed > available // 2 - (self.reserved - budget.reserved):
                    reason = "memory_limit"
                else:
                    self.reserved += needed - budget.reserved
                    budget.reserved = needed
                    budget.limit = target
            self.sequence += 1
            notice = {
                "id": self.sequence, "timestamp": int(time.time()),
                "kind": "blocked" if reason else "expanded",
                "transport": budget.transport,
                "previous_bytes": previous,
                "limit_bytes": self.maximum if reason == "hard_limit" else budget.limit,
                "reason": reason,
            }
            self.notices.append(notice)
        # Diagnostic failures must not change request admission or reservations.
        try:
            if self.journal is not None:
                self.journal.event("warning", "request_capacity", **notice)
        except Exception:
            pass
        try:
            print("EMP large request: %s %d -> %d MiB (%s%s)" % (
                notice["kind"], previous // (1024 * 1024), notice["limit_bytes"] // (1024 * 1024),
                budget.transport, ", " + reason if reason else "",
            ), flush=True)
        except (OSError, UnicodeError, ValueError):
            pass
        if reason:
            raise RequestBodyTooLarge(notice["limit_bytes"], reason=reason)

    def snapshot(self):
        with self.lock:
            return {
                "run_id": self.run_id, "baseline_bytes": self.baseline,
                "maximum_bytes": self.maximum,
                "reserved_bytes": self.reserved,
                "notices": [dict(item) for item in self.notices],
            }
