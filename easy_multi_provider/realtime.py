"""Bounded Codex Voice WebRTC call bridge for native ChatGPT subscriptions."""

from __future__ import annotations

from dataclasses import dataclass
from email.message import Message
import json
import re
from typing import Any, BinaryIO, Dict, Mapping, Optional
from urllib.error import HTTPError, URLError
from urllib.parse import urlparse
from urllib.request import Request

from . import __version__
from .accounts import AccountError, native_auth_headers
from .http_pool import open_request_status


MAX_REALTIME_REQUEST_BYTES = 256 * 1024
MAX_REALTIME_PART_BYTES = 128 * 1024
MAX_REALTIME_PART_HEADER_BYTES = 4096
MAX_REALTIME_RESPONSE_BYTES = 256 * 1024
REALTIME_UPSTREAM_TIMEOUT_SECONDS = 30

_BOUNDARY = re.compile(r"^[!#$%&'*+.^_`|~0-9A-Za-z-]{1,70}$")
_CALL_ID = re.compile(
    r"^(?:rtc_[A-Za-z0-9._~-]+|[0-9A-Fa-f]{8}-[0-9A-Fa-f]{4}-"
    r"[0-9A-Fa-f]{4}-[0-9A-Fa-f]{4}-[0-9A-Fa-f]{12})$"
)
_FORWARDED_HEADERS = {
    "openai-alpha": "OpenAI-Alpha",
    "originator": "originator",
    "session-id": "session-id",
    "thread-id": "thread-id",
    "x-session-id": "x-session-id",
    "x-codex-installation-id": "x-codex-installation-id",
    "x-codex-turn-metadata": "x-codex-turn-metadata",
    "x-oai-attestation": "x-oai-attestation",
}


def _reject_json_constant(value: str) -> None:
    raise ValueError("non-finite JSON value: %s" % value)


class RealtimeError(Exception):
    """A safe error that may cross the local HTTP boundary."""

    def __init__(self, status: int, code: str, message: str, *, close: bool = False):
        super().__init__(message)
        self.status = status
        self.code = code
        self.close = close


@dataclass(frozen=True)
class RealtimeCall:
    sdp: str
    session: Dict[str, Any]


@dataclass(frozen=True)
class RealtimeResponse:
    status: int
    content_type: str
    body: bytes
    location: Optional[str] = None


def _content_type(value: str) -> Message:
    message = Message()
    message["Content-Type"] = value
    return message


def _declared_length(headers: Mapping[str, str]) -> int:
    if headers.get("Transfer-Encoding"):
        raise RealtimeError(
            400,
            "realtime_invalid_request",
            "Transfer-Encoding is not supported for realtime calls",
            close=True,
        )
    value = headers.get("Content-Length")
    if value is None:
        raise RealtimeError(
            411,
            "realtime_length_required",
            "Content-Length is required for realtime calls",
            close=True,
        )
    try:
        length = int(value)
    except (TypeError, ValueError) as exc:
        raise RealtimeError(
            400, "realtime_invalid_request", "invalid Content-Length", close=True
        ) from exc
    if length < 0:
        raise RealtimeError(
            400,
            "realtime_invalid_request",
            "Content-Length cannot be negative",
            close=True,
        )
    if length > MAX_REALTIME_REQUEST_BYTES:
        raise RealtimeError(
            413,
            "realtime_request_too_large",
            "Realtime request exceeds the %d byte limit"
            % MAX_REALTIME_REQUEST_BYTES,
            close=True,
        )
    return length


def read_realtime_call(headers: Mapping[str, str], stream: BinaryIO) -> RealtimeCall:
    """Read and strictly parse Codex's two-part ``/v1/live`` request."""

    content_encoding = (headers.get("Content-Encoding") or "identity").strip().lower()
    if content_encoding != "identity":
        raise RealtimeError(
            415,
            "realtime_unsupported_encoding",
            "Realtime calls require identity Content-Encoding",
            close=True,
        )
    parsed_type = _content_type(headers.get("Content-Type", ""))
    if parsed_type.get_content_type().lower() != "multipart/form-data":
        raise RealtimeError(
            415,
            "realtime_invalid_content_type",
            "Content-Type must be multipart/form-data for /v1/live",
            close=True,
        )
    boundary = parsed_type.get_boundary()
    if not isinstance(boundary, str) or not _BOUNDARY.fullmatch(boundary):
        raise RealtimeError(
            400,
            "realtime_invalid_multipart",
            "Realtime multipart boundary is missing or invalid",
            close=True,
        )

    length = _declared_length(headers)
    raw = stream.read(length)
    if len(raw) != length:
        raise RealtimeError(
            400,
            "realtime_invalid_request",
            "Realtime request body is incomplete",
            close=True,
        )
    return parse_realtime_multipart(raw, boundary)


def parse_realtime_multipart(raw: bytes, boundary: str) -> RealtimeCall:
    """Parse the bounded multipart body without retaining or logging its secrets."""

    if len(raw) > MAX_REALTIME_REQUEST_BYTES:
        raise RealtimeError(
            413,
            "realtime_request_too_large",
            "Realtime request exceeds the %d byte limit"
            % MAX_REALTIME_REQUEST_BYTES,
        )
    if not _BOUNDARY.fullmatch(boundary):
        raise RealtimeError(
            400,
            "realtime_invalid_multipart",
            "Realtime multipart boundary is invalid",
        )

    delimiter = b"--" + boundary.encode("ascii")
    pieces = raw.split(delimiter)
    if len(pieces) != 4 or pieces[0] != b"" or pieces[-1] not in (b"--", b"--\r\n"):
        raise RealtimeError(
            400,
            "realtime_invalid_multipart",
            "Realtime request must contain exactly two multipart fields",
        )

    fields: Dict[str, bytes] = {}
    for piece in pieces[1:-1]:
        if not piece.startswith(b"\r\n") or not piece.endswith(b"\r\n"):
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart framing is invalid",
            )
        part = piece[2:-2]
        header_end = part.find(b"\r\n\r\n")
        if header_end < 0 or header_end > MAX_REALTIME_PART_HEADER_BYTES:
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart headers are invalid",
            )
        header_block = part[:header_end]
        payload = part[header_end + 4 :]
        if len(payload) > MAX_REALTIME_PART_BYTES:
            raise RealtimeError(
                413,
                "realtime_part_too_large",
                "Realtime multipart field exceeds the %d byte limit"
                % MAX_REALTIME_PART_BYTES,
            )

        part_headers: Dict[str, str] = {}
        lines = header_block.split(b"\r\n")
        if len(lines) > 4:
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart field has too many headers",
            )
        for line in lines:
            if b":" not in line or line[:1] in b" \t":
                raise RealtimeError(
                    400,
                    "realtime_invalid_multipart",
                    "Realtime multipart header is malformed",
                )
            name, value = line.split(b":", 1)
            try:
                name_text = name.decode("ascii").strip().lower()
                value_text = value.decode("ascii").strip()
            except UnicodeDecodeError as exc:
                raise RealtimeError(
                    400,
                    "realtime_invalid_multipart",
                    "Realtime multipart headers must be ASCII",
                ) from exc
            if name_text not in {"content-disposition", "content-type"}:
                raise RealtimeError(
                    400,
                    "realtime_invalid_multipart",
                    "Realtime multipart field contains an unsupported header",
                )
            if name_text in part_headers:
                raise RealtimeError(
                    400,
                    "realtime_invalid_multipart",
                    "Realtime multipart field contains a duplicate header",
                )
            part_headers[name_text] = value_text

        disposition = Message()
        disposition["Content-Disposition"] = part_headers.get("content-disposition", "")
        if disposition.get_content_disposition() != "form-data":
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart field requires form-data disposition",
            )
        field_name = disposition.get_param("name", header="content-disposition")
        filename = disposition.get_param("filename", header="content-disposition")
        disposition_params = disposition.get_params(
            header="content-disposition", failobj=[]
        )
        if (
            field_name not in {"sdp", "session"}
            or filename is not None
            or len(disposition_params) != 2
        ):
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart field name is not allowed",
            )
        if field_name in fields:
            raise RealtimeError(
                400,
                "realtime_invalid_multipart",
                "Realtime multipart fields must not be duplicated",
            )
        expected_type = "application/sdp" if field_name == "sdp" else "application/json"
        field_type = _content_type(part_headers.get("content-type", "")).get_content_type()
        if field_type.lower() != expected_type:
            raise RealtimeError(
                415,
                "realtime_invalid_part_content_type",
                "Realtime %s field must use %s" % (field_name, expected_type),
            )
        fields[field_name] = payload

    if set(fields) != {"sdp", "session"}:
        raise RealtimeError(
            400,
            "realtime_invalid_multipart",
            "Realtime request requires sdp and session fields",
        )
    try:
        sdp = fields["sdp"].decode("utf-8")
        session = json.loads(
            fields["session"].decode("utf-8"),
            parse_constant=_reject_json_constant,
        )
    except (UnicodeDecodeError, ValueError, RecursionError) as exc:
        raise RealtimeError(
            400,
            "realtime_invalid_multipart",
            "Realtime sdp and session fields must contain valid UTF-8 data",
        ) from exc
    if not sdp.startswith("v=0") or "\x00" in sdp:
        raise RealtimeError(
            400, "realtime_invalid_sdp", "Realtime SDP offer is empty or invalid"
        )
    if not isinstance(session, dict):
        raise RealtimeError(
            400, "realtime_invalid_session", "Realtime session must be a JSON object"
        )
    return RealtimeCall(sdp=sdp, session=session)


def _safe_forwarded_headers(incoming: Mapping[str, str]) -> Dict[str, str]:
    lower = {str(key).lower(): value for key, value in incoming.items()}
    result: Dict[str, str] = {}
    for source, target in _FORWARDED_HEADERS.items():
        value = lower.get(source)
        if not isinstance(value, str) or not value or len(value) > 8192:
            continue
        if (
            not value.isascii()
            or any(ord(character) < 0x20 or ord(character) == 0x7F for character in value)
        ):
            raise RealtimeError(
                400,
                "realtime_invalid_header",
                "Realtime request contains an invalid forwarded header",
            )
        result[target] = value
    return result


def _upstream_endpoint(base_url: str) -> str:
    parsed = urlparse(base_url)
    if (
        parsed.scheme not in ("http", "https")
        or not parsed.netloc
        or parsed.username
        or parsed.password
        or parsed.query
        or parsed.fragment
    ):
        raise RealtimeError(
            503,
            "native_realtime_unavailable",
            "Native subscription backend is not configured for realtime calls",
        )
    return base_url.rstrip("/") + "/realtime/calls?intent=quicksilver&architecture=avas"


def _header_value(headers: Mapping[str, str], name: str) -> Optional[str]:
    value = headers.get(name)
    if value is None:
        value = next(
            (item for key, item in headers.items() if str(key).lower() == name.lower()),
            None,
        )
    if not isinstance(value, str) or not value or len(value) > 2048:
        return None
    if (
        not value.isascii()
        or any(ord(character) < 0x20 or ord(character) == 0x7F for character in value)
    ):
        return None
    return value


def _location_has_call_id(location: str) -> bool:
    path = urlparse(location).path
    return any(_CALL_ID.fullmatch(segment) for segment in path.split("/") if segment)


def _read_upstream_body(response: Any) -> bytes:
    body = response.read(MAX_REALTIME_RESPONSE_BYTES + 1)
    if len(body) > MAX_REALTIME_RESPONSE_BYTES:
        raise RealtimeError(
            502,
            "realtime_upstream_invalid",
            "Native realtime response exceeds the allowed size",
        )
    return body


def _response_from_upstream(status: int, headers: Mapping[str, str], body: bytes) -> RealtimeResponse:
    content_type = _header_value(headers, "Content-Type")
    if content_type is None:
        content_type = "application/sdp" if 200 <= status < 300 else "application/json"
    location = _header_value(headers, "Location")
    if 200 <= status < 300:
        if not location or not _location_has_call_id(location):
            raise RealtimeError(
                502,
                "realtime_upstream_invalid",
                "Native realtime response is missing a valid call Location",
            )
        if not body:
            raise RealtimeError(
                502,
                "realtime_upstream_invalid",
                "Native realtime response is missing the SDP answer",
            )
    return RealtimeResponse(status, content_type, body, location)


def _clarify_empty_upstream_error(response: RealtimeResponse) -> RealtimeResponse:
    if response.body or response.status not in (401, 403, 404, 405, 501):
        return response
    if response.status in (401, 403):
        code = "native_subscription_auth_failed"
        message = "Native subscription authentication failed for Codex Voice"
    else:
        code = "native_realtime_unsupported"
        message = "The native subscription backend does not support Codex Voice"
    media_type = response.content_type.split(";", 1)[0].strip().lower()
    if media_type == "application/json" or media_type.endswith("+json"):
        body = json.dumps({"error": {"code": code, "message": message}}).encode(
            "utf-8"
        )
    else:
        body = (code + ": " + message).encode("utf-8")
    return RealtimeResponse(
        response.status,
        response.content_type,
        body,
        response.location,
    )


def forward_native_realtime_call(
    base_url: str,
    auth_path: Any,
    incoming_headers: Mapping[str, str],
    call: RealtimeCall,
) -> RealtimeResponse:
    """Create the call through ChatGPT backend and preserve its handshake response."""

    try:
        headers = native_auth_headers(auth_path)
    except AccountError as exc:
        raise RealtimeError(
            401,
            "native_subscription_unavailable",
            "A current native ChatGPT subscription login is required for Codex Voice",
        ) from exc
    headers.update(_safe_forwarded_headers(incoming_headers))
    headers.update(
        {
            "Content-Type": "application/json",
            "Accept": "application/sdp",
            "User-Agent": "EMP/%s" % __version__,
        }
    )
    body = json.dumps(
        {"sdp": call.sdp, "session": call.session},
        ensure_ascii=False,
        allow_nan=False,
        separators=(",", ":"),
    ).encode("utf-8")
    request = Request(_upstream_endpoint(base_url), data=body, headers=headers, method="POST")

    response = None
    try:
        response = open_request_status(
            request, timeout=REALTIME_UPSTREAM_TIMEOUT_SECONDS
        )
        raw = _read_upstream_body(response)
        result = _response_from_upstream(response.status, response.headers, raw)
        return _clarify_empty_upstream_error(result)
    except HTTPError as exc:
        response = exc
        raw = _read_upstream_body(exc)
        result = _response_from_upstream(exc.code, exc.headers or {}, raw)
        return _clarify_empty_upstream_error(result)
    except TimeoutError as exc:
        raise RealtimeError(
            504,
            "native_realtime_timeout",
            "Native realtime call creation timed out",
        ) from exc
    except URLError as exc:
        raise RealtimeError(
            502,
            "native_realtime_transport_error",
            "Native realtime call creation could not reach the subscription backend",
        ) from exc
    except OSError as exc:
        raise RealtimeError(
            502,
            "native_realtime_transport_error",
            "Native realtime call creation could not reach the subscription backend",
        ) from exc
    finally:
        if response is not None:
            try:
                response.close()
            except OSError:
                pass
