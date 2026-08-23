"""Bounded Codex HTTP compression and Responses WebSocket framing."""

from __future__ import annotations

import base64
import hashlib
import io
import json
import struct
import zlib
from typing import Any, Dict, Iterable, Iterator, Optional

import zstandard


WEBSOCKET_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"
MAX_WEBSOCKET_MESSAGE_BYTES = 16 * 1024 * 1024
MAX_SSE_EVENT_BYTES = 1024 * 1024


class TransportError(ValueError):
    pass


class WebSocketProtocolError(TransportError):
    def __init__(self, message: str, code: int = 1002):
        super().__init__(message)
        self.code = code


def zstd_encode(value: bytes) -> bytes:
    try:
        return zstandard.ZstdCompressor().compress(value)
    except zstandard.ZstdError as exc:
        raise TransportError("zstd request compression failed") from exc


def _zlib_decode(value: bytes, wbits: int, max_length: int) -> bytes:
    decoder = zlib.decompressobj(wbits)
    decoded = bytearray()
    pending = value
    while pending:
        piece = decoder.decompress(pending, max_length + 1 - len(decoded))
        decoded.extend(piece)
        if len(decoded) > max_length or decoder.unconsumed_tail:
            if len(decoded) > max_length:
                raise TransportError("decoded request body is too large")
            pending = decoder.unconsumed_tail
            continue
        pending = b""
    decoded.extend(decoder.flush(max_length + 1 - len(decoded)))
    if len(decoded) > max_length:
        raise TransportError("decoded request body is too large")
    if not decoder.eof:
        raise TransportError("compressed request body is incomplete")
    return bytes(decoded)


def _zstd_decode(value: bytes, max_length: int) -> bytes:
    try:
        with zstandard.ZstdDecompressor().stream_reader(io.BytesIO(value)) as reader:
            decoded = reader.read(max_length + 1)
            extra = reader.read(1)
    except zstandard.ZstdError as exc:
        raise TransportError("invalid zstd request body: %s" % exc) from exc
    if len(decoded) > max_length or extra:
        raise TransportError("decoded request body is too large")
    return decoded


def decode_content(value: bytes, content_encoding: str, max_length: int) -> bytes:
    encodings = [
        item.strip().lower()
        for item in str(content_encoding or "").split(",")
        if item.strip() and item.strip().lower() != "identity"
    ]
    decoded = value
    for encoding in reversed(encodings):
        if encoding == "zstd":
            decoded = _zstd_decode(decoded, max_length)
        elif encoding in ("gzip", "x-gzip"):
            decoded = _zlib_decode(decoded, zlib.MAX_WBITS | 16, max_length)
        elif encoding == "deflate":
            decoded = _zlib_decode(decoded, zlib.MAX_WBITS, max_length)
        else:
            raise TransportError("unsupported Content-Encoding: %s" % encoding)
    if len(decoded) > max_length:
        raise TransportError("decoded request body is too large")
    return decoded


def websocket_accept(key: str) -> str:
    try:
        raw = base64.b64decode(key.encode("ascii"), validate=True)
    except (UnicodeEncodeError, ValueError) as exc:
        raise TransportError("invalid Sec-WebSocket-Key") from exc
    if len(raw) != 16:
        raise TransportError("invalid Sec-WebSocket-Key")
    digest = hashlib.sha1((key + WEBSOCKET_GUID).encode("ascii")).digest()
    return base64.b64encode(digest).decode("ascii")


def _read_exact(stream: Any, length: int) -> bytes:
    value = bytearray()
    while len(value) < length:
        chunk = stream.read(length - len(value))
        if not chunk:
            raise EOFError("websocket closed")
        value.extend(chunk)
    return bytes(value)


class WebSocketConnection:
    def __init__(self, reader: Any, writer: Any):
        self.reader = reader
        self.writer = writer
        self.closed = False
        self.peer_close_code: Optional[int] = None

    def _send_frame(self, opcode: int, payload: bytes = b"") -> None:
        if self.closed:
            return
        length = len(payload)
        if length < 126:
            header = bytes((0x80 | opcode, length))
        elif length <= 0xFFFF:
            header = bytes((0x80 | opcode, 126)) + struct.pack("!H", length)
        else:
            header = bytes((0x80 | opcode, 127)) + struct.pack("!Q", length)
        self.writer.write(header + payload)
        self.writer.flush()

    def send_json(self, value: Dict[str, Any]) -> None:
        payload = json.dumps(value, ensure_ascii=False, separators=(",", ":")).encode("utf-8")
        if len(payload) > MAX_WEBSOCKET_MESSAGE_BYTES:
            raise WebSocketProtocolError("websocket response is too large", 1009)
        self._send_frame(1, payload)

    def close(self, code: int = 1000, reason: str = "") -> None:
        if self.closed:
            return
        reason_bytes = reason.encode("utf-8")[:123]
        try:
            self._send_frame(8, struct.pack("!H", code) + reason_bytes)
        except OSError:
            pass
        finally:
            self.closed = True

    def receive_text(self) -> Optional[str]:
        message = bytearray()
        started = False
        while True:
            first, second = _read_exact(self.reader, 2)
            final = bool(first & 0x80)
            if first & 0x70:
                raise WebSocketProtocolError("unsupported websocket extension")
            opcode = first & 0x0F
            masked = bool(second & 0x80)
            length = second & 0x7F
            if length == 126:
                length = struct.unpack("!H", _read_exact(self.reader, 2))[0]
            elif length == 127:
                length = struct.unpack("!Q", _read_exact(self.reader, 8))[0]
            if opcode >= 8 and (not final or length > 125):
                raise WebSocketProtocolError("invalid websocket control frame")
            if not masked:
                raise WebSocketProtocolError("client websocket frames must be masked")
            if len(message) + length > MAX_WEBSOCKET_MESSAGE_BYTES:
                raise WebSocketProtocolError("websocket request is too large", 1009)
            mask = _read_exact(self.reader, 4)
            encoded = _read_exact(self.reader, length)
            payload = bytes(byte ^ mask[index % 4] for index, byte in enumerate(encoded))
            if opcode == 8:
                if len(payload) == 1:
                    raise WebSocketProtocolError("invalid websocket close payload")
                self.peer_close_code = (
                    struct.unpack("!H", payload[:2])[0] if len(payload) >= 2 else 1005
                )
                self._send_frame(8, payload[:125])
                self.closed = True
                return None
            if opcode == 9:
                self._send_frame(10, payload)
                continue
            if opcode == 10:
                continue
            if opcode == 1:
                if started:
                    raise WebSocketProtocolError("unexpected websocket text frame")
                started = True
            elif opcode != 0 or not started:
                raise WebSocketProtocolError("only websocket text messages are supported", 1003)
            message.extend(payload)
            if final:
                try:
                    return bytes(message).decode("utf-8")
                except UnicodeDecodeError as exc:
                    raise WebSocketProtocolError("websocket text must be valid UTF-8", 1007) from exc


def sse_json_events(chunks: Iterable[bytes]) -> Iterator[Dict[str, Any]]:
    pending = bytearray()
    data_lines = []

    def finish_event() -> Optional[Dict[str, Any]]:
        if not data_lines:
            return None
        raw = b"\n".join(data_lines)
        data_lines[:] = []
        if raw == b"[DONE]":
            return None
        try:
            value = json.loads(raw.decode("utf-8"))
        except (UnicodeDecodeError, ValueError) as exc:
            raise TransportError("upstream SSE event is not valid JSON") from exc
        if not isinstance(value, dict):
            raise TransportError("upstream SSE event must be a JSON object")
        return value

    for chunk in chunks:
        if not isinstance(chunk, (bytes, bytearray)):
            raise TransportError("upstream stream returned non-bytes data")
        pending.extend(chunk)
        if len(pending) > MAX_SSE_EVENT_BYTES:
            raise TransportError("upstream SSE event is too large")
        while b"\n" in pending:
            raw_line, _, remainder = pending.partition(b"\n")
            pending = bytearray(remainder)
            line = raw_line.rstrip(b"\r")
            if not line:
                event = finish_event()
                if event is not None:
                    yield event
            elif line.startswith(b"data:"):
                data_lines.append(line[5:].lstrip())
    if pending:
        line = bytes(pending).rstrip(b"\r")
        if line.startswith(b"data:"):
            data_lines.append(line[5:].lstrip())
    event = finish_event()
    if event is not None:
        yield event
