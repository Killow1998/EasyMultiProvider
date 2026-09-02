"""Bound active subscription generations without retaining account data."""

from __future__ import annotations

from collections import deque
from dataclasses import dataclass
import threading
import time
from typing import Deque, Dict, Optional

from .router_errors import RouterError


DEFAULT_SUBSCRIPTION_CONCURRENCY = 4
DEFAULT_SUBSCRIPTION_QUEUE_TIMEOUT = 45.0
MAX_SUBSCRIPTION_WAITERS = 32
MAX_SUBSCRIPTION_IDENTITIES = 64


class UpstreamAdmissionError(RouterError):
    """A local capacity rejection made before an upstream request is sent."""

    def __init__(self, reason: str = "local_concurrency_limit"):
        self.error_class = "upstream_capacity"
        self.failure_reason = reason
        super().__init__("subscription upstream is busy; retry shortly", 503)


@dataclass(frozen=True)
class AdmissionSnapshot:
    wait_ms: int
    active: int
    limit: int


@dataclass
class _IdentityState:
    active: int
    waiters: Deque[object]


class UpstreamAdmissionLease:
    def __init__(
        self,
        owner: "UpstreamAdmissionController",
        identity: str,
        snapshot: AdmissionSnapshot,
    ) -> None:
        self._owner = owner
        self._identity = identity
        self.snapshot = snapshot
        self._released = False

    def release(self) -> None:
        if self._released:
            return
        self._released = True
        self._owner._release(self._identity)

    def __enter__(self) -> "UpstreamAdmissionLease":
        return self

    def __exit__(self, *_args) -> None:
        self.release()


class UpstreamAdmissionController:
    """A small FIFO gate shared by all active requests for one account identity."""

    def __init__(
        self,
        per_identity_limit: int = DEFAULT_SUBSCRIPTION_CONCURRENCY,
        queue_timeout: float = DEFAULT_SUBSCRIPTION_QUEUE_TIMEOUT,
        max_waiters: int = MAX_SUBSCRIPTION_WAITERS,
        max_identities: int = MAX_SUBSCRIPTION_IDENTITIES,
        clock=time.monotonic,
    ) -> None:
        self.limit = max(1, int(per_identity_limit))
        self.queue_timeout = max(0.0, float(queue_timeout))
        self.max_waiters = max(1, int(max_waiters))
        self.max_identities = max(1, int(max_identities))
        self._clock = clock
        self._condition = threading.Condition()
        self._states: Dict[str, _IdentityState] = {}

    def acquire(
        self, identity: str, timeout: Optional[float] = None
    ) -> UpstreamAdmissionLease:
        if not isinstance(identity, str) or not identity or len(identity) > 512:
            raise UpstreamAdmissionError("invalid_concurrency_identity")
        timeout = self.queue_timeout if timeout is None else max(0.0, float(timeout))
        queued_at = self._clock()
        deadline = queued_at + timeout
        ticket = object()
        with self._condition:
            state = self._states.get(identity)
            if state is None:
                if len(self._states) >= self.max_identities:
                    raise UpstreamAdmissionError("concurrency_identity_limit")
                state = _IdentityState(0, deque())
                self._states[identity] = state
            if len(state.waiters) >= self.max_waiters:
                self._cleanup(identity, state)
                raise UpstreamAdmissionError("concurrency_queue_full")
            state.waiters.append(ticket)
            while state.active >= self.limit or state.waiters[0] is not ticket:
                remaining = deadline - self._clock()
                if remaining <= 0:
                    state.waiters.remove(ticket)
                    self._cleanup(identity, state)
                    self._condition.notify_all()
                    raise UpstreamAdmissionError("concurrency_queue_timeout")
                self._condition.wait(remaining)
            state.waiters.popleft()
            state.active += 1
            snapshot = AdmissionSnapshot(
                wait_ms=max(0, int(round((self._clock() - queued_at) * 1000))),
                active=state.active,
                limit=self.limit,
            )
            return UpstreamAdmissionLease(self, identity, snapshot)

    def _release(self, identity: str) -> None:
        with self._condition:
            state = self._states.get(identity)
            if state is None or state.active <= 0:
                return
            state.active -= 1
            self._cleanup(identity, state)
            self._condition.notify_all()

    def _cleanup(self, identity: str, state: _IdentityState) -> None:
        if state.active == 0 and not state.waiters:
            self._states.pop(identity, None)
