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
REQUEST_GROWTH_QUANTUM = 16 * 1024 * 1024
MIN_MEMORY_HEADROOM_BYTES = 512 * 1024 * 1024


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
                 maximum=MAX_EXPANDED_REQUEST_BYTES, available_memory=None,
                 growth_quantum=REQUEST_GROWTH_QUANTUM,
                 memory_headroom=MIN_MEMORY_HEADROOM_BYTES,
                 memory_status=None):
        if baseline <= 0 or maximum < baseline:
            raise ValueError("invalid request growth limits")
        if growth_quantum <= 0 or memory_headroom < 0:
            raise ValueError("invalid request memory limits")
        self.baseline = baseline
        self.maximum = maximum
        self.growth_quantum = growth_quantum
        self.memory_headroom = memory_headroom
        if memory_status is not None:
            self.memory_status = memory_status
        elif available_memory is not None:
            self.memory_status = lambda: {"available": available_memory()}
        else:
            self.memory_status = self._system_memory_status
        self.journal = journal
        self.lock = threading.Lock()
        self.reserved = 0
        self.notices = deque(maxlen=20)
        self.sequence = 0
        self.run_id = uuid.uuid4().hex

    @staticmethod
    def _system_memory_status():
        memory = psutil.virtual_memory()
        return {
            "available": memory.available,
            "total": memory.total,
            "used": memory.used,
            "used_percent": memory.percent,
        }

    def request(self, transport):
        return RequestBudget(self, transport)

    def ensure(self, budget, size):
        with self.lock:
            if budget.closed:
                raise RuntimeError("request budget is already released")
            if size <= budget.limit:
                return
            previous = budget.limit
            target = min(
                self.maximum,
                max(self.baseline, ((size + self.growth_quantum - 1) // self.growth_quantum)
                    * self.growth_quantum),
            )
            reason = "hard_limit" if size > self.maximum else ""
            available = 0
            needed = 0
            total = 0
            used = 0
            used_percent = None
            required = 0
            if not reason:
                try:
                    memory = self.memory_status()
                    available = max(0, int(memory.get("available", 0)))
                    total = max(0, int(memory.get("total", 0)))
                    used = max(0, int(memory.get("used", max(0, total - available))))
                    percent = memory.get("used_percent")
                    if isinstance(percent, (int, float)) and not isinstance(percent, bool):
                        used_percent = max(0.0, min(100.0, round(float(percent), 1)))
                except (OSError, ValueError, TypeError, psutil.Error):
                    available = 0
                needed = target * MEMORY_RESERVATION_FACTOR
                # A fixed headroom avoids the discontinuity caused by reserving
                # half of all currently free memory. Other expanded requests
                # remain part of the same atomic admission decision.
                other_reserved = self.reserved - budget.reserved
                required = needed + other_reserved + self.memory_headroom
                if required > available:
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
                "requested_bytes": size,
                "target_bytes": self.maximum if reason == "hard_limit" else target,
                "limit_bytes": self.maximum if reason == "hard_limit" else budget.limit,
                "reservation_bytes": needed,
                "required_memory_bytes": required,
                "available_bytes": available,
                "memory_total_bytes": total,
                "memory_used_bytes": used,
                "memory_used_percent": used_percent,
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
            raise RequestBodyTooLarge(
                notice["target_bytes"],
                reason=reason,
                available_bytes=available,
                required_memory_bytes=required,
                memory_total_bytes=total,
                memory_used_bytes=used,
                memory_used_percent=used_percent,
            )

    def snapshot(self):
        with self.lock:
            return {
                "run_id": self.run_id, "baseline_bytes": self.baseline,
                "maximum_bytes": self.maximum,
                "reserved_bytes": self.reserved,
                "notices": [dict(item) for item in self.notices],
            }
