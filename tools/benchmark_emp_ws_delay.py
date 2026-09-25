#!/usr/bin/env python3
"""Measure scheduled upstream HTTP SSE to downstream WebSocket event delay."""

from __future__ import annotations

import argparse
import base64
from hashlib import sha1
import json
import os
from pathlib import Path
import secrets
import socket
import struct
import sys
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from concurrent.futures import ThreadPoolExecutor

try:
    from . import benchmark_emp_runtime as benchmark
    from . import benchmark_emp_stream_delay as stream_delay
except ImportError:  # Running as `python tools/benchmark_emp_ws_delay.py`.
    tools_dir = str(Path(__file__).resolve().parent)
    if tools_dir not in sys.path:
        sys.path.insert(0, tools_dir)
    import benchmark_emp_runtime as benchmark
    import benchmark_emp_stream_delay as stream_delay


WEBSOCKET_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_WEBSOCKET_MESSAGE_BYTES = 8 * 1024 * 1024
DEFAULT_ITERATIONS = 30
DEFAULT_WARMUP = 3
SINGLE_STREAM_P95_LIMIT_MS = 5.0
CONCURRENT_P95_LIMIT_MS = 20.0
SAFE_CONTROL_HEADER_VALUES = frozenset({
    "x-models-etag", "openai-model", "x-openai-model",
})


class WebSocketProtocolError(ValueError):
    pass


def websocket_accept(key: str) -> str:
    try:
        decoded = base64.b64decode(key.encode("ascii"), validate=True)
    except (UnicodeEncodeError, ValueError) as exc:
        raise WebSocketProtocolError("invalid_websocket_key") from exc
    if len(decoded) != 16:
        raise WebSocketProtocolError("invalid_websocket_key")
    digest = sha1((key + WEBSOCKET_GUID).encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def validate_parameters(iterations: int, warmup: int, concurrency: int) -> None:
    if iterations < 1 or warmup < 0:
        raise benchmark.BenchmarkError("invalid_iteration_count")
    if not 1 <= concurrency <= 128:
        raise benchmark.BenchmarkError("invalid_concurrency")


def p95_limit_for_concurrency(concurrency: int) -> float:
    return CONCURRENT_P95_LIMIT_MS if concurrency == 128 else SINGLE_STREAM_P95_LIMIT_MS


def _validate_upgrade_response(status: int, headers: dict, key: str) -> bool:
    if status != 101:
        return False
    upgrade_tokens = {
        token.strip().lower()
        for value in headers.get("upgrade", [])
        for token in value.split(",")
    }
    connection_tokens = {
        token.strip().lower()
        for value in headers.get("connection", [])
        for token in value.split(",")
    }
    accepts = headers.get("sec-websocket-accept", [])
    return (
        "websocket" in upgrade_tokens
        and "upgrade" in connection_tokens
        and accepts == [websocket_accept(key)]
        and not headers.get("sec-websocket-extensions")
    )


def encode_client_frame(opcode: int, payload: bytes, *, mask_key: bytes | None = None,
                        fin: bool = True) -> bytes:
    if not 0 <= opcode <= 0x0F or opcode in (0x3, 0x4, 0x5, 0x6, 0x7, 0xB, 0xC, 0xD, 0xE, 0xF):
        raise WebSocketProtocolError("unsupported_frame_opcode")
    if opcode >= 0x8 and (not fin or len(payload) > 125):
        raise WebSocketProtocolError("invalid_control_frame")
    if len(payload) > MAX_WEBSOCKET_MESSAGE_BYTES:
        raise WebSocketProtocolError("websocket_message_too_large")
    key = mask_key if mask_key is not None else secrets.token_bytes(4)
    if len(key) != 4:
        raise WebSocketProtocolError("invalid_mask_key")
    first = (0x80 if fin else 0) | opcode
    length = len(payload)
    if length < 126:
        header = bytes((first, 0x80 | length))
    elif length <= 0xFFFF:
        header = bytes((first, 0x80 | 126)) + struct.pack("!H", length)
    else:
        header = bytes((first, 0x80 | 127)) + struct.pack("!Q", length)
    masked = bytes(value ^ key[index & 3] for index, value in enumerate(payload))
    return header + key + masked


def _read_exact(reader, length: int) -> bytes:
    result = bytearray()
    while len(result) < length:
        chunk = reader.read(length - len(result))
        if not chunk:
            raise EOFError("websocket_eof")
        result.extend(chunk)
    return bytes(result)


def read_server_frame(reader) -> tuple[bool, int, bytes]:
    first, second = _read_exact(reader, 2)
    fin = bool(first & 0x80)
    opcode = first & 0x0F
    if first & 0x70:
        raise WebSocketProtocolError("server_rsv_bits_set")
    if second & 0x80:
        raise WebSocketProtocolError("server_frame_masked")
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", _read_exact(reader, 2))[0]
        if length < 126:
            raise WebSocketProtocolError("nonminimal_frame_length")
    elif length == 127:
        extended = struct.unpack("!Q", _read_exact(reader, 8))[0]
        if extended < 65536 or extended >> 63:
            raise WebSocketProtocolError("invalid_extended_frame_length")
        length = extended
    if opcode >= 0x8 and (not fin or length > 125):
        raise WebSocketProtocolError("invalid_control_frame")
    if length > MAX_WEBSOCKET_MESSAGE_BYTES:
        raise WebSocketProtocolError("websocket_message_too_large")
    return fin, opcode, _read_exact(reader, length)


def read_server_message(reader, send_control) -> tuple[int, bytes]:
    message_opcode = None
    fragments = bytearray()
    while True:
        fin, opcode, payload = read_server_frame(reader)
        if opcode == 0x9:  # ping
            send_control(0xA, payload)
            continue
        if opcode == 0xA:  # pong
            continue
        if opcode == 0x8:  # close
            return opcode, payload
        if opcode in (0x1, 0x2):
            if message_opcode is not None:
                raise WebSocketProtocolError("interleaved_data_frames")
            message_opcode = opcode
            fragments.extend(payload)
        elif opcode == 0x0:
            if message_opcode is None:
                raise WebSocketProtocolError("unexpected_continuation_frame")
            fragments.extend(payload)
        else:
            raise WebSocketProtocolError("unsupported_server_opcode")
        if len(fragments) > MAX_WEBSOCKET_MESSAGE_BYTES:
            raise WebSocketProtocolError("websocket_message_too_large")
        if fin:
            return message_opcode, bytes(fragments)


def _client_control_sender(stream: socket.socket):
    def send_control(opcode: int, payload: bytes) -> None:
        stream.sendall(encode_client_frame(opcode, payload))
    return send_control


def _parse_close(payload: bytes) -> dict:
    if not payload:
        return {"code": None, "reason_length": 0}
    if len(payload) == 1:
        raise WebSocketProtocolError("invalid_close_payload")
    code = struct.unpack("!H", payload[:2])[0]
    try:
        payload[2:].decode("utf-8")
    except UnicodeDecodeError as exc:
        raise WebSocketProtocolError("invalid_close_reason") from exc
    return {"code": code, "reason_length": len(payload) - 2}


def _websocket_handshake(port: int, cookie: str):
    stream = socket.create_connection(("127.0.0.1", port), timeout=15)
    try:
        return _websocket_handshake_on_stream(stream, port, cookie)
    except BaseException:
        stream.close()
        raise


def _websocket_handshake_on_stream(stream: socket.socket, port: int, cookie: str):
    stream.settimeout(15)
    reader = stream.makefile("rb")
    key = base64.b64encode(secrets.token_bytes(16)).decode("ascii")
    request = (
        f"GET /v1/responses HTTP/1.1\r\n"
        f"Host: 127.0.0.1:{port}\r\n"
        "Upgrade: websocket\r\n"
        "Connection: keep-alive, Upgrade\r\n"
        f"Origin: http://127.0.0.1:{port}\r\n"
        "Sec-WebSocket-Version: 13\r\n"
        f"Sec-WebSocket-Key: {key}\r\n"
        "Authorization: Bearer " + benchmark.FIXTURE_CALLER_KEY + "\r\n"
        f"Cookie: {cookie}\r\n\r\n"
    ).encode("ascii")
    stream.sendall(request)
    status_line = reader.readline(8193)
    if not status_line or len(status_line) > 8192:
        raise WebSocketProtocolError("invalid_handshake_status_line")
    try:
        parts = status_line.decode("latin-1").rstrip("\r\n").split(" ", 2)
        if len(parts) < 2 or not parts[0].startswith("HTTP/"):
            raise ValueError
        status = int(parts[1])
    except (UnicodeDecodeError, ValueError) as exc:
        raise WebSocketProtocolError("invalid_handshake_status_line") from exc
    headers = {}
    total = len(status_line)
    while True:
        line = reader.readline(8193)
        total += len(line)
        if not line or total > 32768 or len(line) > 8192:
            raise WebSocketProtocolError("invalid_handshake_headers")
        if line in (b"\r\n", b"\n"):
            break
        if line[:1] in (b" ", b"\t") or b":" not in line:
            raise WebSocketProtocolError("invalid_handshake_headers")
        name, value = line.split(b":", 1)
        header = name.decode("ascii", errors="strict").strip().lower()
        value_text = value.decode("latin-1").strip()
        headers.setdefault(header, []).append(value_text)
    if status != 101:
        return stream, reader, {"status": status, "accept_valid": False, "error": "upgrade_rejected"}
    accept_valid = _validate_upgrade_response(status, headers, key)
    return stream, reader, {
        "status": status,
        "accept_valid": accept_valid,
        "error": None if accept_valid else "invalid_upgrade_response",
    }


def _control_signature(event: dict) -> dict:
    headers = event.get("headers")
    normalized_headers = {}
    if isinstance(headers, dict):
        normalized_headers = {
            key.lower(): (
                value if key.lower() in SAFE_CONTROL_HEADER_VALUES else "<redacted>"
            )
            for key, value in headers.items()
            if isinstance(key, str) and isinstance(value, (str, int, float, bool))
        }
    return {
        "type": event.get("type"),
        "headers": normalized_headers,
        "other_fields": sorted(
            key for key in event
            if key not in {"type", "headers"} and isinstance(key, str)
        ),
    }


def _metadata_precedes_response_created(transcript_types: list[str]) -> bool:
    metadata = [
        index for index, event_type in enumerate(transcript_types)
        if event_type == "codex.response.metadata"
    ]
    created = [
        index for index, event_type in enumerate(transcript_types)
        if event_type == "response.created"
    ]
    return len(metadata) == 1 and len(created) == 1 and metadata[0] < created[0]


def _is_invalid_post_terminal_event(event_type: str) -> bool:
    return event_type in set(stream_delay.EXPECTED_TYPES)


def _read_ws_turn(service, nonce: str) -> dict:
    stream = None
    reader = None
    scheduled = []
    controls = []
    observed_ns = []
    transcript_types = []
    protocol_errors = []
    handshake = {"status": None, "accept_valid": False, "error": None}
    close = None
    terminal_seen = False
    try:
        stream, reader, handshake = _websocket_handshake(service.port, service.cookie)
        if handshake["status"] != 101 or not handshake["accept_valid"]:
            return {
                "handshake": handshake,
                "scheduled": [],
                "observed_ns": [],
                "controls": [],
                "transcript_types": [],
                "protocol_errors": [],
                "close": None,
                "error": handshake["error"] or "invalid_handshake",
            }
        payload = json.dumps({
            "type": "response.create",
            "model": "benchmark/model",
            "input": "ws-scheduled-" + nonce,
            "stream": True,
            "metadata": {"benchmark_nonce": nonce},
        }, separators=(",", ":")).encode("utf-8")
        stream.sendall(encode_client_frame(0x1, payload))
        send_control = _client_control_sender(stream)
        expected_types = set(stream_delay.EXPECTED_TYPES)
        while True:
            opcode, raw = read_server_message(reader, send_control)
            received_ns = time.monotonic_ns()
            if opcode == 0x8:
                close = _parse_close(raw)
                break
            if opcode != 0x1:
                raise WebSocketProtocolError("expected_text_message")
            try:
                event = json.loads(raw.decode("utf-8"))
            except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                raise WebSocketProtocolError("invalid_json_event") from exc
            if not isinstance(event, dict):
                raise WebSocketProtocolError("non_object_event")
            event_type = event.get("type")
            if not isinstance(event_type, str):
                raise WebSocketProtocolError("invalid_event_type")
            transcript_types.append(event_type)
            if event_type in expected_types:
                scheduled.append(event)
                observed_ns.append(received_ns)
            else:
                controls.append(_control_signature(event))
            if event_type in {"response.completed", "response.failed", "response.incomplete"}:
                terminal_seen = True
                stream.sendall(encode_client_frame(0x8, struct.pack("!H", 1000)))
                stream.settimeout(1)
            if terminal_seen:
                try:
                    opcode, raw = read_server_message(reader, send_control)
                except (OSError, EOFError, TimeoutError, socket.timeout):
                    break
                if opcode == 0x8:
                    close = _parse_close(raw)
                    break
                if opcode == 0x1:
                    try:
                        extra = json.loads(raw.decode("utf-8"))
                    except (UnicodeDecodeError, json.JSONDecodeError) as exc:
                        raise WebSocketProtocolError("invalid_post_terminal_event") from exc
                    if isinstance(extra, dict):
                        extra_type = extra.get("type")
                        if not isinstance(extra_type, str):
                            raise WebSocketProtocolError("invalid_post_terminal_event")
                        transcript_types.append(extra_type)
                        if _is_invalid_post_terminal_event(extra_type):
                            protocol_errors.append("invalid_post_terminal_event")
                        else:
                            controls.append(_control_signature(extra))
                    else:
                        raise WebSocketProtocolError("non_object_post_terminal_event")
        semantics = stream_delay._event_semantics(scheduled)
        control_types = [event.get("type") for event in controls]
        metadata_order_valid = _metadata_precedes_response_created(transcript_types)
        valid = (
            handshake["status"] == 101
            and handshake["accept_valid"]
            and semantics["ordered"]
            and semantics["reasoning_present"]
            and semantics["text_present"]
            and semantics["tool_arguments_present"]
            and semantics["completed"]
            and semantics["output_pair_present"]
            and close is not None
            and close["code"] in (None, 1000)
            and control_types.count("codex.response.metadata") == 1
            and metadata_order_valid
            and not protocol_errors
        )
        return {
            "handshake": handshake,
            "scheduled": scheduled,
            "observed_ns": observed_ns,
            "semantics": semantics,
            "controls": controls,
            "control_types": control_types,
            "transcript_types": transcript_types,
            "protocol_errors": protocol_errors,
            "close": close,
            "terminal_seen": terminal_seen,
            "valid": valid,
            "error": None,
        }
    except (OSError, EOFError, TimeoutError, WebSocketProtocolError):
        return {
            "handshake": handshake,
            "scheduled": scheduled,
            "observed_ns": observed_ns,
            "semantics": stream_delay._event_semantics(scheduled),
            "controls": controls,
            "control_types": [event.get("type") for event in controls],
            "transcript_types": transcript_types,
            "protocol_errors": protocol_errors + ["websocket_io_or_protocol_error"],
            "close": close,
            "terminal_seen": terminal_seen,
            "valid": False,
            "error": "websocket_io_or_protocol_error",
        }
    finally:
        if reader is not None:
            try:
                reader.close()
            except OSError:
                pass
        if stream is not None:
            try:
                stream.close()
            except OSError:
                pass


NONCE_PREFIX = "ws-scheduled-"
SAFE_INPUT_ITEM_TYPES = frozenset({
    "message", "input_text", "input_image", "input_file", "function_call_output",
})


def _find_input_nonce(value) -> str | None:
    if isinstance(value, str):
        if value.startswith(NONCE_PREFIX) and value[len(NONCE_PREFIX):]:
            return value[len(NONCE_PREFIX):]
        return None
    if isinstance(value, list):
        found = [_find_input_nonce(item) for item in value]
    elif isinstance(value, dict):
        found = [_find_input_nonce(item) for item in value.values()]
    else:
        return None
    values = [item for item in found if item is not None]
    return values[0] if values and len(set(values)) == 1 else None


def _safe_input_item_type(value) -> str:
    if not isinstance(value, dict):
        return type(value).__name__
    kind = value.get("type")
    return kind if isinstance(kind, str) and kind in SAFE_INPUT_ITEM_TYPES else "other"


def _request_shape(path: str, authorization: str | None, body) -> tuple[dict, str]:
    body_is_object = isinstance(body, dict)
    input_value = body.get("input") if body_is_object else None
    input_kind = (
        "missing" if input_value is None else
        "string" if isinstance(input_value, str) else
        "array" if isinstance(input_value, list) else
        "object" if isinstance(input_value, dict) else
        type(input_value).__name__
    )
    input_nonce = _find_input_nonce(input_value)
    metadata = body.get("metadata") if body_is_object else None
    raw_metadata_nonce = metadata.get("benchmark_nonce") if isinstance(metadata, dict) else None
    metadata_nonce = (
        raw_metadata_nonce
        if isinstance(raw_metadata_nonce, str) and raw_metadata_nonce
        else None
    )
    nonce = metadata_nonce or input_nonce
    nonce_consistent = not (metadata_nonce and input_nonce and metadata_nonce != input_nonce)
    top_level_types = []
    nested_content_types = []
    if isinstance(input_value, list):
        top_level_types = sorted({
            _safe_input_item_type(item) for item in input_value
        })
        for item in input_value:
            if isinstance(item, dict) and isinstance(item.get("content"), list):
                nested_content_types.extend(
                    _safe_input_item_type(part) for part in item["content"]
                )
    top_level_types = top_level_types[:8]
    nested_content_types = sorted(set(nested_content_types))[:8]
    stream_value = body.get("stream") if body_is_object else None
    shape = {
        "path_matches": path == "/v1/responses",
        "authorization_matches_fixture": (
            authorization == "Bearer " + benchmark.FIXTURE_PROVIDER_KEY
        ),
        "body_type": "object" if body_is_object else type(body).__name__,
        "model_matches_fixture": (
            body_is_object and body.get("model") == benchmark.UPSTREAM_MODEL
        ),
        "stream_type": "missing" if body_is_object and "stream" not in body else type(stream_value).__name__,
        "stream_is_true": stream_value is True,
        "input_type": input_kind,
        "input_array_item_types": top_level_types,
        "input_content_item_types": nested_content_types,
        "metadata_nonce_present": metadata_nonce is not None,
        "input_nonce_present": input_nonce is not None,
        "nonce_consistent": nonce_consistent,
    }
    if path != "/v1/responses":
        rejection = "upstream_path_mismatch"
    elif authorization != "Bearer " + benchmark.FIXTURE_PROVIDER_KEY:
        rejection = "upstream_authorization_mismatch"
    elif not body_is_object:
        rejection = "upstream_body_not_object"
    elif body.get("model") != benchmark.UPSTREAM_MODEL:
        rejection = "upstream_model_mismatch"
    elif stream_value is not True:
        rejection = "upstream_stream_not_true"
    elif not nonce:
        rejection = "benchmark_nonce_missing"
    elif not nonce_consistent:
        rejection = "benchmark_nonce_mismatch"
    else:
        rejection = ""
    return shape, rejection


class _ScheduledHttpSseHandler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def do_POST(self):
        server = self.server
        with server._lock:
            server.request_count += 1
        try:
            length = int(self.headers.get("Content-Length", "0"))
            body = json.loads(self.rfile.read(length))
        except (ValueError, UnicodeDecodeError, json.JSONDecodeError):
            server._record_error("upstream_invalid_json")
            self.send_error(400)
            return
        request_shape, rejection = _request_shape(
            self.path, self.headers.get("Authorization"), body
        )
        metadata = body.get("metadata") if isinstance(body, dict) else None
        metadata_nonce = metadata.get("benchmark_nonce") if isinstance(metadata, dict) else None
        input_nonce = _find_input_nonce(body.get("input")) if isinstance(body, dict) else None
        nonce = metadata_nonce or input_nonce or ""
        status = benchmark._fake_upstream_status(
            self.path, self.headers.get("Authorization"), body
        )
        with server._lock:
            server.request_shapes.append(request_shape)
        if status != 200 or body.get("stream") is not True or not nonce:
            server._record_error(rejection or "upstream_request_rejected")
            self.send_error(status if status != 200 else 400)
            return
        with server._lock:
            server.model_counts[body.get("model", "")] = (
                server.model_counts.get(body.get("model", ""), 0) + 1
            )
            server.nonce_counts[nonce] = server.nonce_counts.get(nonce, 0) + 1
        events = stream_delay.scheduled_events()
        frames = [
            ("event: " + event["type"] + "\ndata: "
             + json.dumps(event, separators=(",", ":")) + "\n\n").encode()
            for event in events
        ]
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream")
        self.send_header("Content-Length", str(sum(map(len, frames))))
        self.send_header("Connection", "close")
        self.end_headers()
        self.wfile.flush()
        start_ns = time.monotonic_ns()
        for event, frame in zip(events, frames, strict=True):
            sequence = event["sequence_number"]
            due_ns = start_ns + sequence * stream_delay.EVENT_SPACING_MS * 1_000_000
            delay_ns = due_ns - time.monotonic_ns()
            if delay_ns > 0:
                time.sleep(delay_ns / 1_000_000_000)
            sent_ns = time.monotonic_ns()
            with server._lock:
                server.due_times.setdefault(nonce, {})[sequence] = due_ns
                server.send_times.setdefault(nonce, {})[sequence] = sent_ns
            try:
                self.wfile.write(frame)
                self.wfile.flush()
            except (BrokenPipeError, ConnectionResetError):
                return

    def log_message(self, *_args):
        pass


class ScheduledHttpSseUpstream(ThreadingHTTPServer):
    daemon_threads = True
    request_queue_size = 256

    def __init__(self):
        super().__init__(("127.0.0.1", 0), _ScheduledHttpSseHandler)
        self._lock = threading.Lock()
        self.request_count = 0
        self.error_count = 0
        self.rejection_counts = {}
        self.model_counts = {}
        self.nonce_counts = {}
        self.request_shapes = []
        self.due_times = {}
        self.send_times = {}
        self.thread = threading.Thread(target=self.serve_forever, daemon=True)
        self.thread.start()

    @property
    def base_url(self) -> str:
        return f"http://127.0.0.1:{self.server_port}/v1"

    def reset_counts(self):
        with self._lock:
            self.request_count = 0
            self.error_count = 0
            self.rejection_counts.clear()
            self.model_counts.clear()
            self.nonce_counts.clear()
            self.request_shapes.clear()
            self.due_times.clear()
            self.send_times.clear()

    def _record_error(self, reason: str):
        with self._lock:
            self.error_count += 1
            self.rejection_counts[reason] = self.rejection_counts.get(reason, 0) + 1

    def snapshot(self):
        with self._lock:
            return {
                "requests": self.request_count,
                "errors": self.error_count,
                "rejections": dict(self.rejection_counts),
                "models": dict(self.model_counts),
                "nonce_counts": dict(self.nonce_counts),
                "request_shapes": list(self.request_shapes),
                "due_times": {key: dict(value) for key, value in self.due_times.items()},
                "send_times": {key: dict(value) for key, value in self.send_times.items()},
            }

    def close(self):
        self.shutdown()
        self.server_close()
        self.thread.join(timeout=3)


def _run_batch(service, upstream: ScheduledHttpSseUpstream, marker: str,
               concurrency: int, collect_timings: bool) -> dict:
    upstream.reset_counts()
    nonces = [f"{marker}-{index}" for index in range(concurrency)]
    with ThreadPoolExecutor(max_workers=concurrency) as executor:
        results = list(executor.map(
            lambda nonce: _read_ws_turn(service, nonce), nonces
        ))
    snapshot = upstream.snapshot()
    semantic_results = [
        {
            "scheduled": result.get("semantics"),
            "controls": result.get("controls", []),
            "transcript_types": result.get("transcript_types", []),
            "protocol_errors": result.get("protocol_errors", []),
            "handshake": result.get("handshake"),
            "close": result.get("close"),
        }
        for result in results
    ]
    representative = semantic_results[0] if semantic_results else {}
    errors = []
    if any(not result.get("valid") for result in results):
        errors.append("websocket_semantic_or_handshake")
    if any(value != representative for value in semantic_results[1:]):
        errors.append("connection_semantics_differed_within_runtime")
    if snapshot["requests"] != concurrency:
        errors.append("upstream_request_count")
    if snapshot["errors"]:
        errors.append("upstream_rejected_request")
    if snapshot["models"] != {benchmark.UPSTREAM_MODEL: concurrency}:
        errors.append("upstream_model_count")
    expected_nonce_counts = {nonce: 1 for nonce in nonces}
    if snapshot["nonce_counts"] != expected_nonce_counts:
        errors.append("upstream_nonce_count")

    timings = []
    if collect_timings:
        for nonce, result in zip(nonces, results, strict=True):
            timing_snapshot = {
                "due_times": snapshot["due_times"].get(nonce, {}),
                "send_times": snapshot["send_times"].get(nonce, {}),
            }
            client_timings = stream_delay._event_timings(
                result.get("scheduled", []),
                result.get("observed_ns", []),
                timing_snapshot,
            )
            if len(client_timings) != len(stream_delay.EXPECTED_TYPES):
                errors.append("downstream_activity_timing_missing")
            timings.extend(client_timings)

    return {
        "valid": not errors,
        "errors": sorted(set(errors)),
        "semantics": representative,
        "timings": timings,
        "handshake": {
            "status_counts": {
                str(status): sum(
                    result.get("handshake", {}).get("status") == status
                    for result in results
                )
                for status in sorted({
                    result.get("handshake", {}).get("status") for result in results
                }, key=lambda value: -1 if value is None else value)
            },
            "accept_valid_count": sum(
                result.get("handshake", {}).get("accept_valid") is True
                for result in results
            ),
        },
        "upstream": {
            "requests": snapshot["requests"],
            "expected_requests": concurrency,
            "errors": snapshot["errors"],
            "rejections": snapshot["rejections"],
            "models": snapshot["models"],
            "request_shapes": snapshot["request_shapes"],
        },
    }


def _runtime_preflight(name: str, command: list[str], cwd: Path, config: bytes,
                       work_root: Path, key_file: Path, base_env: dict,
                       python_root: Path, upstream: ScheduledHttpSseUpstream,
                       concurrency: int) -> dict:
    config_path = work_root / (name + "-ws-delay-preflight.json")
    config_path.write_bytes(config)
    home = work_root / (name + "-ws-delay-preflight-home")
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    service = benchmark.EmpService(command, cwd, config_path, home, env, name)
    with service:
        result = _run_batch(
            service, upstream, name + "-preflight", concurrency, collect_timings=False
        )
    return {
        "valid": result["valid"],
        "errors": result["errors"],
        "handshake": result["handshake"],
        "semantics": result["semantics"],
        "upstream": result["upstream"],
    }


def _runtime_measurement(name: str, command: list[str], cwd: Path, config: bytes,
                         work_root: Path, key_file: Path, base_env: dict,
                         python_root: Path, upstream: ScheduledHttpSseUpstream,
                         concurrency: int, iterations: int, warmup: int) -> dict:
    config_path = work_root / (name + "-ws-delay.json")
    config_path.write_bytes(config)
    home = work_root / (name + "-ws-delay-home")
    env = benchmark._service_environment(base_env, home, key_file, python_root)
    service = benchmark.EmpService(command, cwd, config_path, home, env, name)
    errors = []
    semantics = None
    handshake = None
    all_timings = []
    upstream_total = 0
    upstream_errors = 0
    upstream_rejections = {}
    with service:
        for index in range(warmup + iterations):
            result = _run_batch(
                service,
                upstream,
                f"{name}-{index}",
                concurrency,
                collect_timings=index >= warmup,
            )
            if not result["valid"]:
                errors.extend(result["errors"])
            if semantics is None:
                semantics = result["semantics"]
                handshake = result["handshake"]
            elif result["semantics"] != semantics:
                errors.append("stream_semantics_changed_between_iterations")
            elif result["handshake"] != handshake:
                errors.append("handshake_semantics_changed_between_iterations")
            all_timings.extend(result["timings"])
            upstream_total += result["upstream"]["requests"]
            upstream_errors += result["upstream"]["errors"]
            for reason, count in result["upstream"]["rejections"].items():
                upstream_rejections[reason] = upstream_rejections.get(reason, 0) + count
    expected_requests = (warmup + iterations) * concurrency
    if upstream_total != expected_requests:
        errors.append("upstream_request_count")
    if upstream_errors:
        errors.append("upstream_rejected_request")
    delay_values = [timing["added_delay_ms"] for timing in all_timings]
    delay_by_type = {}
    lateness_values = []
    lateness_by_type = {}
    for timing in all_timings:
        event_type = timing["event_type"]
        delay_by_type.setdefault(event_type, []).append(timing["added_delay_ms"])
        lateness_values.append(timing["schedule_lateness_ms"])
        lateness_by_type.setdefault(event_type, []).append(timing["schedule_lateness_ms"])
    return {
        "iterations": iterations,
        "warmup": warmup,
        "concurrency": concurrency,
        "semantics": semantics,
        "handshake": handshake,
        "event_count": len(all_timings),
        "activity_events": stream_delay._activity_event_summary(all_timings),
        "added_delay": {
            "all_events": stream_delay._latency_summary(delay_values),
            "by_event_type": {
                event_type: stream_delay._latency_summary(values)
                for event_type, values in sorted(delay_by_type.items())
            },
        },
        "schedule_lateness": {
            "all_events": stream_delay._latency_summary(lateness_values),
            "by_event_type": {
                event_type: stream_delay._latency_summary(values)
                for event_type, values in sorted(lateness_by_type.items())
            },
        },
        "upstream": {
            "requests": upstream_total,
            "expected_requests": expected_requests,
            "errors": upstream_errors,
            "rejections": upstream_rejections,
        },
        "errors": sorted(set(errors)),
    }


def _parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--python-root", type=Path, default=benchmark.DEFAULT_PYTHON_ROOT)
    parser.add_argument("--python", dest="python", type=Path)
    parser.add_argument("--rust-binary", type=Path, required=True)
    parser.add_argument("--output", type=Path, default=Path("-"))
    parser.add_argument("--expected-version", default="0.12.1")
    parser.add_argument("--iterations", type=int, default=DEFAULT_ITERATIONS)
    parser.add_argument("--warmup", type=int, default=DEFAULT_WARMUP)
    parser.add_argument("--concurrency", type=int, default=1)
    parser.add_argument("--order", choices=("python-first", "rust-first"), default="python-first")
    return parser.parse_args(argv)


def run(args) -> tuple[dict, int]:
    if benchmark.psutil is None:
        raise benchmark.BenchmarkError("psutil_required")
    validate_parameters(args.iterations, args.warmup, args.concurrency)
    p95_limit = p95_limit_for_concurrency(args.concurrency)
    python_root = args.python_root.resolve(strict=True)
    python = benchmark._python_launcher(args.python, python_root)
    rust_binary = args.rust_binary.resolve(strict=True)
    python_env = dict(os.environ)
    python_env["PYTHONPATH"] = str(python_root)
    python_version = benchmark._python_version(python, python_root, python_env)
    if python_version != args.expected_version:
        raise benchmark.BenchmarkError("python_version_mismatch")
    version_result = subprocess.run(
        [str(rust_binary), "--version"], check=True, capture_output=True,
        text=True, timeout=10,
    )
    rust_version = benchmark._clean_version(args.expected_version, version_result.stdout)
    report = {
        "schema": 1,
        "status": "incomparable",
        "comparison_enabled": False,
        "environment": benchmark._environment(),
        "versions": {
            "python_emp": python_version,
            "python_executable": str(python),
            "python_root": str(python_root),
            "rust_emp": rust_version,
            "rust_binary": str(rust_binary),
        },
        "parameters": {
            "iterations": args.iterations,
            "warmup": args.warmup,
            "concurrency": args.concurrency,
            "event_spacing_ms": stream_delay.EVENT_SPACING_MS,
            "p95_limit_ms": p95_limit,
            "measurement_order": args.order,
            "client_stream": True,
            "transport": "downstream_websocket_upstream_http_sse",
        },
        "semantics": {},
        "preflight": {},
        "measurements": None,
        "errors": [],
    }
    with tempfile.TemporaryDirectory(prefix="emp-ws-delay-") as directory:
        work_root = Path(directory)
        key_file = work_root / "master.key"
        key_file.write_bytes(base64.urlsafe_b64encode(os.urandom(32)))
        os.chmod(key_file, 0o600)
        base_env = dict(os.environ)
        for key in benchmark.EXTERNAL_CREDENTIAL_ENV:
            base_env.pop(key, None)
        base_env["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(key_file)
        upstream = ScheduledHttpSseUpstream()
        try:
            config = stream_delay._normalized_config_with_reasoning(
                python, python_root, python_env, upstream.base_url, work_root
            )
            runtimes = [
                ("python", [str(python), "-m", "easy_multi_provider"], python_root),
                ("rust", [str(rust_binary)], Path.cwd()),
            ]
            if args.order == "rust-first":
                runtimes.reverse()
            for name, command, cwd in runtimes:
                result = _runtime_preflight(
                    name, command, cwd, config, work_root, key_file,
                    base_env, python_root, upstream, args.concurrency,
                )
                report["preflight"][name] = result
                report["semantics"][name] = result["semantics"]
                if not result["valid"]:
                    report["errors"].append(name + "_websocket_semantic_preflight_failed")
            differences = benchmark.semantic_diff(
                report["semantics"]["python"], report["semantics"]["rust"]
            )
            if differences:
                report["semantic_difference_paths"] = differences
                report["errors"].append("runtime_semantic_mismatch")
            if report["errors"]:
                return report, 2
            measurements = {}
            for name, command, cwd in runtimes:
                measurements[name] = _runtime_measurement(
                    name, command, cwd, config, work_root, key_file,
                    base_env, python_root, upstream, args.concurrency,
                    args.iterations, args.warmup,
                )
                if measurements[name]["semantics"] != report["semantics"][name]:
                    measurements[name]["errors"].append("measurement_semantics_changed")
                report["errors"].extend(
                    [name + "_" + error for error in measurements[name]["errors"]]
                )
            report["measurements"] = measurements
            if report["errors"]:
                return report, 2
            report["comparison_enabled"] = True
            report["status"] = "measured"
            for name, values in measurements.items():
                p95 = values["activity_events"]["added_delay"]["p95_ms"]
                if p95 is None or p95 > p95_limit:
                    report["errors"].append(name + "_activity_added_delay_p95_gate")
            if report["errors"]:
                report["status"] = "invalid_measurement"
                report["comparison_enabled"] = False
                return report, 2
            return report, 0
        finally:
            upstream.close()


def main(argv=None) -> int:
    args = _parse_args(argv)
    try:
        report, exit_code = run(args)
    except Exception as exc:
        report = {
            "schema": 1,
            "status": "incomparable",
            "comparison_enabled": False,
            "errors": [benchmark.safe_error_name(exc)],
        }
        exit_code = 2
    output = json.dumps(report, sort_keys=True, indent=2)
    if args.output == Path("-"):
        print(output)
    else:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(output + "\n", encoding="utf-8")
    return exit_code


if __name__ == "__main__":
    raise SystemExit(main())
