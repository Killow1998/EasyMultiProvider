"""Connection-local native Responses WebSocket forwarding.

The bridge deliberately stores no prompt or response history.  Codex owns the
logical request and decides when a request is a strict incremental extension;
EMP only keeps the matching upstream socket alive for that downstream socket.
"""

from __future__ import annotations

from dataclasses import dataclass
import json
import time
import uuid
from typing import Any, Callable, Dict, Iterator, Mapping, Optional


MAX_NATIVE_WEBSOCKET_EVENT_BYTES = 16 * 1024 * 1024
MAX_NATIVE_WEBSOCKET_RESPONSE_BYTES = 16 * 1024 * 1024
MAX_NATIVE_WEBSOCKET_EVENTS = 100_000
NATIVE_WEBSOCKET_CONNECT_TIMEOUT = 30
NATIVE_WEBSOCKET_IDLE_TIMEOUT = 180
NATIVE_WEBSOCKET_REQUEST_TIMEOUT = 180
_RETRYABLE_UPGRADE_STATUSES = frozenset({404, 405, 415, 426, 501, 502, 503, 504})


def _retryable_upgrade_status(status: int) -> bool:
    return status in _RETRYABLE_UPGRADE_STATUSES


class NativeWebSocketError(RuntimeError):
    """A bounded transport failure that never includes URLs or credentials."""

    def __init__(
        self, message: str, status: int = 502, retryable: Optional[bool] = None
    ):
        super().__init__(message)
        self.status = status if isinstance(status, int) else 502
        self.retryable = (
            _retryable_upgrade_status(self.status)
            if retryable is None
            else bool(retryable)
        )


@dataclass(frozen=True)
class NativeWebSocketTarget:
    """Transient upstream connection data; never serialize this object."""

    url: str
    headers: Mapping[str, str]
    connection_key: str


def _default_connector(target: NativeWebSocketTarget):
    try:
        from websocket import create_connection
    except ImportError as exc:  # pragma: no cover - packaging regression guard
        raise NativeWebSocketError(
            "native upstream websocket support is not installed", 503, False
        ) from exc

    # Redirects are forbidden because these headers contain credentials.
    return create_connection(
        target.url,
        timeout=NATIVE_WEBSOCKET_CONNECT_TIMEOUT,
        header=dict(target.headers),
        suppress_origin=True,
        redirect_limit=0,
        enable_multithread=False,
    )


def _handshake_status(connection: Any) -> int:
    response = getattr(connection, "handshake_response", None)
    status = getattr(response, "status", 101)
    return status if isinstance(status, int) and not isinstance(status, bool) else 502


def terminal_observation(event: Mapping[str, Any]) -> Optional[Dict[str, Any]]:
    """Normalize a native terminal without trusting a contradictory wrapper."""

    event_type = str(event.get("type") or "")
    response = event.get("response")
    response = response if isinstance(response, Mapping) else {}
    nested_status = response.get("status")
    nested_error = response.get("error")
    if event_type == "error":
        return {"status": 502, "success": False, "error_class": "stream_error"}
    if event_type == "response.failed":
        return {"status": 502, "success": False, "error_class": "stream_error"}
    if event_type == "response.incomplete":
        return {"status": 422, "success": False, "error_class": "stream_incomplete"}
    if event_type != "response.completed":
        return None
    if nested_status not in (None, "completed") or nested_error not in (None, {}):
        return {
            "status": 422 if nested_status == "incomplete" else 502,
            "success": False,
            "error_class": "stream_incomplete"
            if nested_status == "incomplete"
            else "stream_error",
        }
    return {"status": 200, "success": True, "error_class": "none"}


def _safe_terminal_event(
    event: Mapping[str, Any], observation: Optional[Mapping[str, Any]]
) -> Dict[str, Any]:
    """Replace a contradictory completion without forwarding upstream detail."""

    result = dict(event)
    if (
        str(event.get("type") or "") != "response.completed"
        or not isinstance(observation, Mapping)
        or observation.get("success") is not False
    ):
        return result
    response = event.get("response")
    response = response if isinstance(response, Mapping) else {}
    response_id = response.get("id")
    if not isinstance(response_id, str) or not response_id:
        response_id = "resp_" + uuid.uuid4().hex
    error_class = str(observation.get("error_class") or "stream_error")
    return {
        "type": "response.failed",
        "response": {
            "id": response_id,
            "object": "response",
            "status": "failed",
            "output": [],
            "error": {
                "code": error_class,
                "message": "native upstream returned a contradictory terminal event",
            },
        },
    }


class NativeWebSocketBridge:
    """Reuse one upstream native socket while its route identity is unchanged."""

    def __init__(
        self,
        connector: Optional[Callable[[NativeWebSocketTarget], Any]] = None,
    ):
        self._connector = connector or _default_connector
        self._connection = None
        self._connection_key: Optional[str] = None
        self._last_connection_reused = False

    @property
    def connection_key(self) -> Optional[str]:
        return self._connection_key

    @property
    def last_connection_reused(self) -> bool:
        return self._last_connection_reused

    def _usable(self, target: NativeWebSocketTarget) -> bool:
        if self._connection is None or self._connection_key != target.connection_key:
            return False
        connected = getattr(self._connection, "connected", True)
        return connected is not False

    def connect(self, target: NativeWebSocketTarget) -> bool:
        """Return True when an existing connection was reused."""

        if self._usable(target):
            self._last_connection_reused = True
            return True
        self.close()
        self._last_connection_reused = False
        try:
            connection = self._connector(target)
        except NativeWebSocketError:
            raise
        except Exception as exc:
            status = getattr(exc, "status_code", 502)
            if not isinstance(status, int) or isinstance(status, bool):
                status = 502
            raise NativeWebSocketError(
                "native upstream websocket connection failed",
                status,
                _retryable_upgrade_status(status),
            ) from exc
        status = _handshake_status(connection)
        if status != 101:
            try:
                connection.close()
            except Exception:
                pass
            raise NativeWebSocketError(
                "native upstream websocket upgrade was rejected",
                status,
                _retryable_upgrade_status(status),
            )
        self._connection = connection
        self._connection_key = target.connection_key
        self._last_connection_reused = False
        return False

    def events(
        self,
        target: NativeWebSocketTarget,
        request: Mapping[str, Any],
    ) -> Iterator[Dict[str, Any]]:
        started = time.monotonic()
        self.connect(target)
        connection = self._connection
        if connection is None:  # defensive; connect either succeeds or raises
            raise NativeWebSocketError("native upstream websocket is unavailable")
        try:
            response_bytes = 0
            setter = getattr(connection, "settimeout", None)
            payload = json.dumps(
                dict(request), ensure_ascii=False, separators=(",", ":")
            )
            if len(payload.encode("utf-8")) > MAX_NATIVE_WEBSOCKET_EVENT_BYTES:
                raise NativeWebSocketError(
                    "native upstream websocket request is too large", 413, False
                )
            sender = getattr(connection, "send_text", None)
            if callable(sender):
                sender(payload)
            else:
                connection.send(payload)
            for _ in range(MAX_NATIVE_WEBSOCKET_EVENTS):
                remaining = NATIVE_WEBSOCKET_REQUEST_TIMEOUT - (
                    time.monotonic() - started
                )
                if remaining <= 0:
                    raise NativeWebSocketError(
                        "native upstream websocket exceeded the request deadline",
                        504,
                        False,
                    )
                if callable(setter):
                    setter(min(NATIVE_WEBSOCKET_IDLE_TIMEOUT, remaining))
                raw = connection.recv()
                if raw in (None, "", b""):
                    raise NativeWebSocketError(
                        "native upstream websocket closed before a terminal event"
                    )
                if isinstance(raw, bytes):
                    raw_bytes = raw
                    if len(raw_bytes) > MAX_NATIVE_WEBSOCKET_EVENT_BYTES:
                        raise NativeWebSocketError(
                            "native upstream websocket event is too large", 502, False
                        )
                    try:
                        raw = raw_bytes.decode("utf-8", errors="strict")
                    except UnicodeDecodeError as exc:
                        raise NativeWebSocketError(
                            "native upstream websocket event is not UTF-8", 502, False
                        ) from exc
                elif isinstance(raw, str):
                    raw_bytes = raw.encode("utf-8")
                else:
                    raise NativeWebSocketError(
                        "native upstream websocket returned an unsupported frame", 502, False
                    )
                response_bytes += len(raw_bytes)
                if len(raw_bytes) > MAX_NATIVE_WEBSOCKET_EVENT_BYTES:
                    raise NativeWebSocketError(
                        "native upstream websocket event is too large", 502, False
                    )
                if response_bytes > MAX_NATIVE_WEBSOCKET_RESPONSE_BYTES:
                    raise NativeWebSocketError(
                        "native upstream websocket response is too large", 502, False
                    )
                try:
                    event = json.loads(raw)
                except ValueError as exc:
                    raise NativeWebSocketError(
                        "native upstream websocket event is not valid JSON", 502, False
                    ) from exc
                if not isinstance(event, dict):
                    raise NativeWebSocketError(
                        "native upstream websocket event must be an object", 502, False
                    )
                observation = terminal_observation(event)
                event = _safe_terminal_event(event, observation)
                yield event
                if observation is not None:
                    return
            raise NativeWebSocketError(
                "native upstream websocket exceeded the event limit", 502, False
            )
        except GeneratorExit:
            raise
        except NativeWebSocketError:
            self.close()
            raise
        except Exception as exc:
            self.close()
            raise NativeWebSocketError(
                "native upstream websocket transport failed"
            ) from exc

    def close(self) -> None:
        connection = self._connection
        self._connection = None
        self._connection_key = None
        if connection is None:
            return
        try:
            shutdown = getattr(connection, "shutdown", None)
            if callable(shutdown):
                shutdown()
            else:
                connection.close()
        except Exception:
            pass
