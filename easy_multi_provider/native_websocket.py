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
from typing import Any, Callable, Dict, Iterator, Mapping, Optional, Tuple

from .transport_failures import PHASE_CONNECT, network_failure, status_error_class


MAX_NATIVE_WEBSOCKET_EVENT_BYTES = 64 * 1024 * 1024
# Keep the legacy uncompressed client as a compatibility path for Python 3.8.
# Packaged builds use ``websockets`` with permessage-deflate and can establish
# a native incremental chain even when the first turn contains a large history.
MAX_NATIVE_WEBSOCKET_UNCOMPRESSED_REQUEST_BYTES = 4 * 1024 * 1024
NATIVE_WEBSOCKET_CONNECT_TIMEOUT = 15
NATIVE_WEBSOCKET_REUSE_PROBE_TIMEOUT = 2
NATIVE_WEBSOCKET_IDLE_TIMEOUT = 300
_RETRYABLE_UPGRADE_STATUSES = frozenset(
    {400, 404, 405, 415, 426, 501}
)


def _retryable_upgrade_status(status: int) -> bool:
    return status in _RETRYABLE_UPGRADE_STATUSES


def _http_fallback_before_request(status: int) -> bool:
    return _retryable_upgrade_status(status) or 500 <= status <= 599


def compressed_native_websocket_available() -> bool:
    try:
        from websockets.sync.client import connect as _connect
    except ImportError:
        return False
    return callable(_connect)


def native_websocket_request_fits(request: Mapping[str, Any]) -> bool:
    # Codex's native WebSocket client doesn't impose an outbound message-size
    # limit.  Keep the small legacy cutoff only for the Python 3.8 fallback,
    # which cannot negotiate permessage-deflate.
    if compressed_native_websocket_available():
        return True
    try:
        encoded = json.dumps(
            dict(request), ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
    except (RecursionError, TypeError, ValueError, OverflowError):
        return False
    return len(encoded) <= MAX_NATIVE_WEBSOCKET_UNCOMPRESSED_REQUEST_BYTES


class NativeWebSocketError(RuntimeError):
    """A bounded transport failure that never includes URLs or credentials."""

    def __init__(
        self,
        message: str,
        status: int = 502,
        retryable: Optional[bool] = None,
        *,
        request_sent: bool = False,
        error_class: Optional[str] = None,
        failure_reason: Optional[str] = None,
    ):
        super().__init__(message)
        self.status = status if isinstance(status, int) else 502
        self.retryable = (
            _http_fallback_before_request(self.status)
            if retryable is None
            else bool(retryable)
        )
        self.request_sent = bool(request_sent)
        self.error_class = error_class
        self.failure_reason = failure_reason


@dataclass(frozen=True)
class NativeWebSocketTarget:
    """Transient upstream connection data; never serialize this object."""

    url: str
    headers: Mapping[str, str]
    connection_key: str


@dataclass(frozen=True)
class _HandshakeResponse:
    status: int


class _CompressedWebSocketConnection:
    """Adapt ``websockets`` to the small interface used by the bridge."""

    def __init__(self, connection: Any):
        self._connection = connection
        response = getattr(connection, "response", None)
        status = getattr(response, "status_code", 101)
        self.handshake_response = _HandshakeResponse(
            status if isinstance(status, int) and not isinstance(status, bool) else 101
        )
        self._timeout = float(NATIVE_WEBSOCKET_CONNECT_TIMEOUT)

    @property
    def connected(self) -> bool:
        state = getattr(self._connection, "state", None)
        name = getattr(state, "name", None)
        return True if name is None else name == "OPEN"

    def settimeout(self, value: Any) -> None:
        self._timeout = max(0.1, float(value))

    def send_text(self, value: str) -> None:
        self._connection.send(value)

    def send(self, value: str) -> None:
        self.send_text(value)

    def recv(self):
        return self._connection.recv(timeout=self._timeout)

    def probe(self, payload: str, timeout: float) -> bool:
        acknowledgement = self._connection.ping(payload)
        return bool(acknowledgement.wait(max(0.1, float(timeout))))

    def close(self) -> None:
        self._connection.close()


def _compressed_connector(target: NativeWebSocketTarget):
    from websockets.sync.client import connect

    try:
        connection = connect(
            target.url,
            compression="deflate",
            additional_headers=dict(target.headers),
            user_agent_header=None,
            proxy=True,
            open_timeout=NATIVE_WEBSOCKET_CONNECT_TIMEOUT,
            ping_interval=20,
            ping_timeout=20,
            close_timeout=5,
            max_size=MAX_NATIVE_WEBSOCKET_EVENT_BYTES,
            max_queue=16,
        )
    except Exception as exc:
        response = getattr(exc, "response", None)
        status = getattr(response, "status_code", None)
        if isinstance(status, int) and not isinstance(status, bool):
            raise NativeWebSocketError(
                "native upstream websocket upgrade was rejected",
                status,
                _http_fallback_before_request(status),
            ) from exc
        raise
    return _CompressedWebSocketConnection(connection)


def _legacy_connector(target: NativeWebSocketTarget):
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


def _default_connector(target: NativeWebSocketTarget):
    try:
        return _compressed_connector(target)
    except ImportError:
        return _legacy_connector(target)


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
        status = event.get("status", event.get("status_code"))
        if not isinstance(status, int) or isinstance(status, bool) or not 400 <= status <= 599:
            status = 502
        return {
            "status": status,
            "success": False,
            "error_class": status_error_class(status),
        }
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


def native_websocket_event_activity(event: Mapping[str, Any]) -> Tuple[bool, bool]:
    """Return content/tool activity without retaining any event payload."""

    event_type = str(event.get("type") or "")
    item = event.get("item")
    item_type = str(item.get("type") or "") if isinstance(item, Mapping) else ""
    message_content = (
        item.get("content")
        if isinstance(item, Mapping) and item_type == "message"
        else None
    )
    output = (
        "output_text" in event_type
        or "output_image" in event_type
        or "reasoning" in event_type
        or item_type == "reasoning"
        or (isinstance(message_content, list) and bool(message_content))
    )
    tool = (
        "function_call" in event_type
        or "custom_tool_call" in event_type
        or item_type in {"function_call", "custom_tool_call"}
    )
    return output, tool


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
        self._probed_connection = None

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

    def can_continue(self, target: NativeWebSocketTarget) -> bool:
        """Return whether incremental continuity can use the existing socket."""

        return self._usable(target) and self._probe_reused_connection()

    def _probe_reused_connection(self) -> bool:
        """Confirm that an idle upstream socket still answers before reuse."""

        connection = self._connection
        if connection is None:
            return False
        if self._probed_connection is connection:
            return True
        probe = getattr(connection, "probe", None)
        if callable(probe):
            try:
                if probe(
                    uuid.uuid4().hex[:16],
                    NATIVE_WEBSOCKET_REUSE_PROBE_TIMEOUT,
                ):
                    self._probed_connection = connection
                    return True
            except Exception:
                pass
            self.close()
            return False
        ping = getattr(connection, "ping", None)
        receive = getattr(connection, "recv_data_frame", None)
        setter = getattr(connection, "settimeout", None)
        # Lightweight test doubles and alternate clients may not expose control
        # frames. Their ordinary ``connected`` state remains the best signal.
        if not callable(ping) or not callable(receive):
            self._probed_connection = connection
            return True
        payload = uuid.uuid4().hex[:16]
        try:
            if callable(setter):
                setter(NATIVE_WEBSOCKET_REUSE_PROBE_TIMEOUT)
            ping(payload)
            for _ in range(3):
                opcode, frame = receive(control_frame=True)
                if opcode == 0xA:  # WebSocket pong
                    data = getattr(frame, "data", b"")
                    if isinstance(data, bytes):
                        data = data.decode("ascii", errors="ignore")
                    if data == payload:
                        self._probed_connection = connection
                        return True
                    break
                if opcode == 0x9:  # peer ping; websocket-client already replied
                    continue
                break
        except Exception:
            pass
        finally:
            if callable(setter):
                try:
                    setter(NATIVE_WEBSOCKET_CONNECT_TIMEOUT)
                except Exception:
                    pass
        self.close()
        return False

    def connect(self, target: NativeWebSocketTarget) -> bool:
        """Return True when an existing connection was reused."""

        if self._usable(target) and self._probe_reused_connection():
            self._last_connection_reused = True
            return True
        self.close()
        self._last_connection_reused = False
        try:
            connection = self._connector(target)
        except NativeWebSocketError:
            raise
        except Exception as exc:
            explicit_status = getattr(exc, "status_code", None)
            failure = network_failure(exc, PHASE_CONNECT)
            if (
                isinstance(explicit_status, int)
                and not isinstance(explicit_status, bool)
                and failure.error_class == "network"
            ):
                status = explicit_status
                error_class = None
                failure_reason = None
            else:
                status = failure.status
                error_class = failure.error_class
                failure_reason = failure.failure_reason
            raise NativeWebSocketError(
                "native upstream websocket connection failed",
                status,
                _http_fallback_before_request(status),
                error_class=error_class,
                failure_reason=failure_reason,
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
                _http_fallback_before_request(status),
            )
        self._connection = connection
        self._connection_key = target.connection_key
        self._last_connection_reused = False
        self._probed_connection = None
        return False

    def events(
        self,
        target: NativeWebSocketTarget,
        request: Mapping[str, Any],
    ) -> Iterator[Dict[str, Any]]:
        self.connect(target)
        connection = self._connection
        if connection is None:  # defensive; connect either succeeds or raises
            raise NativeWebSocketError("native upstream websocket is unavailable")
        try:
            request_sent = False
            substantive_activity = False
            setter = getattr(connection, "settimeout", None)
            payload = json.dumps(
                dict(request), ensure_ascii=False, separators=(",", ":")
            )
            sender = getattr(connection, "send_text", None)
            # Sending may partially succeed before the transport reports an
            # error. From this point onward replay is unsafe.
            request_sent = True
            if callable(sender):
                sender(payload)
            else:
                connection.send(payload)
            self._probed_connection = None
            while True:
                if callable(setter):
                    # Match Codex: apply one idle timeout to each incoming
                    # WebSocket message, including the first response event.
                    setter(NATIVE_WEBSOCKET_IDLE_TIMEOUT)
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
                if len(raw_bytes) > MAX_NATIVE_WEBSOCKET_EVENT_BYTES:
                    raise NativeWebSocketError(
                        "native upstream websocket event is too large", 502, False
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
                output, tool = native_websocket_event_activity(event)
                substantive_activity = substantive_activity or output or tool
                yield event
                if observation is not None:
                    return
        except GeneratorExit:
            raise
        except NativeWebSocketError as exc:
            self.close()
            if request_sent and not exc.request_sent:
                raise NativeWebSocketError(
                    str(exc),
                    exc.status,
                    False,
                    request_sent=True,
                    error_class=exc.error_class,
                    failure_reason=exc.failure_reason
                    or "transport_closed_after_send",
                ) from exc
            raise
        except Exception as exc:
            self.close()
            if isinstance(exc, TimeoutError) or "timeout" in exc.__class__.__name__.lower():
                error_class = (
                    "idle_after_output"
                    if substantive_activity
                    else "first_output_timeout"
                )
                raise NativeWebSocketError(
                    "native upstream websocket became idle after output"
                    if substantive_activity
                    else "native upstream websocket produced no output before the timeout",
                    504,
                    False,
                    request_sent=request_sent,
                    error_class=error_class,
                    failure_reason=error_class,
                ) from exc
            raise NativeWebSocketError(
                "native upstream websocket transport failed",
                502,
                False if request_sent else None,
                request_sent=request_sent,
                failure_reason=(
                    "transport_closed_after_send" if request_sent else None
                ),
            ) from exc

    def close(self) -> None:
        connection = self._connection
        self._connection = None
        self._connection_key = None
        self._probed_connection = None
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
