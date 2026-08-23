"""Bounded, privacy-safe JSONL diagnostic journal for EMP."""

from __future__ import annotations

import datetime
import hashlib
import json
import os
import re
import secrets
import stat
import sys
import threading
import traceback

_MAX_RECORD_BYTES = 16 * 1024
_DEFAULT_MAX_PART_BYTES = 2 * 1024 * 1024
_DEFAULT_MAX_DIR_BYTES = 10 * 1024 * 1024
_MAX_STRING_CHARS = 2048
_MAX_COLLECTION_ITEMS = 128
_MAX_DEPTH = 6
_MAX_FRAMES = 24

_FORBIDDEN_KEYS = frozenset({
    "authorization",
    "cookie",
    "cookies",
    "headers",
    "body",
    "request",
    "response",
    "prompt",
    "content",
    "input",
    "output",
    "api_key",
    "apikey",
    "token",
    "access_token",
    "refresh_token",
    "id_token",
    "session_token",
    "quota_token",
    "bootstrap",
    "session",
    "password",
    "secret",
    "credential",
    "credentials",
    "auth",
    "set_cookie",
})

_REDACTED = "<redacted>"

_REDACTION_PATTERNS = [
    re.compile(r"(?i)\bbearer\s+[A-Za-z0-9._~+/=-]{6,}"),
    re.compile(r"(?i)\bsk-[A-Za-z0-9_-]{10,}\b"),
    re.compile(r"\bAIza[0-9A-Za-z_-]{20,}\b"),
    re.compile(r"(?i)\bgh[pousr]_[A-Za-z0-9]{20,}\b"),
    re.compile(r"\beyJ[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{8,}\.[A-Za-z0-9_-]{4,}\b"),
    re.compile(
        r"(?i)\b(api[-_ ]?key|access[-_ ]?token|refresh[-_ ]?token|token"
        r"|password|secret|set[-_ ]?cookie|cookie|authorization|bootstrap"
        r"|session)\s*[=:]\s*(?:bearer\s+)?[\"']?[^\s\"',;&{}]{4,}"
    ),
]

_PART_RE = re.compile(
    r"^emp-\d{8}T\d{6}Z-\d+-[0-9a-f]{16}-p(\d+)\.jsonl$"
)

_LEVELS = frozenset({"debug", "info", "warning", "error"})

_FORBIDDEN_KEY_SEGMENTS = frozenset({
    "auth",
    "authentication",
    "authorization",
    "cookie",
    "cookies",
    "credential",
    "credentials",
    "password",
    "secret",
    "secrets",
})

_FORBIDDEN_KEY_SUFFIXES = (
    "_api_key",
    "_apikey",
    "_token",
)


def _utc_now() -> str:
    return (
        datetime.datetime.now(datetime.timezone.utc)
        .isoformat()
        .replace("+00:00", "Z")
    )


def _stable_pseudonym(run_id: str, value: str) -> str:
    digest = hashlib.sha256(
        b"emp-pseudonym:" + run_id.encode("utf-8") + b":" + value.encode("utf-8")
    ).hexdigest()
    return digest[:16]


def _redact_string(text: str) -> str:
    result = text
    for pattern in _REDACTION_PATTERNS:
        result = pattern.sub(_REDACTED, result)
    return result


def _normalize_field_key(key: object) -> str:
    text = str(key).strip()
    text = re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", text)
    text = re.sub(r"[^A-Za-z0-9]+", "_", text)
    return text.strip("_").lower()


def _is_forbidden_field_key(key: object) -> bool:
    normalized = _normalize_field_key(key)
    if normalized in _FORBIDDEN_KEYS:
        return True
    segments = set(normalized.split("_"))
    if segments.intersection(_FORBIDDEN_KEY_SEGMENTS):
        return True
    return normalized.endswith(_FORBIDDEN_KEY_SUFFIXES)


def _absolute_path_prefixes(path: str, path_module=os.path):
    absolute = path_module.abspath(path)
    drive, tail = path_module.splitdrive(absolute)
    separator = path_module.sep
    alternate = getattr(path_module, "altsep", None)
    if alternate:
        tail = tail.replace(alternate, separator)
    current = drive + separator if tail.startswith(separator) else drive
    prefixes = []
    if current:
        prefixes.append(current)
    for component in tail.split(separator):
        if not component:
            continue
        current = path_module.join(current, component)
        prefixes.append(current)
    return tuple(prefixes)


def _assert_no_symlink_components(path: str) -> None:
    for current in _absolute_path_prefixes(path):
        try:
            info = os.lstat(current)
        except FileNotFoundError:
            continue
        if stat.S_ISLNK(info.st_mode):
            raise OSError("journal path contains a symlink")


def _sanitize(value, depth: int = 0):
    """Drop forbidden keys, redact strings, and bound size recursively."""
    if depth > _MAX_DEPTH:
        return "<max-depth>"
    if isinstance(value, str):
        text = _redact_string(value)
        if len(text) > _MAX_STRING_CHARS:
            text = text[:_MAX_STRING_CHARS] + "<truncated>"
        return text
    if isinstance(value, bool) or value is None or isinstance(value, int):
        return value
    if isinstance(value, float):
        if value != value or value in (float("inf"), float("-inf")):
            return repr(value)
        return value
    if isinstance(value, dict):
        out = {}
        truncated = len(value) - _MAX_COLLECTION_ITEMS
        for index, (raw_key, raw_item) in enumerate(value.items()):
            if index >= _MAX_COLLECTION_ITEMS:
                break
            key_text = str(raw_key)[:256]
            if _is_forbidden_field_key(key_text):
                continue
            out[_redact_string(key_text)] = _sanitize(raw_item, depth + 1)
        if truncated > 0:
            out["_truncated_items"] = truncated
        return out
    if isinstance(value, (list, tuple, set)):
        material = list(value)
        truncated = len(material) - _MAX_COLLECTION_ITEMS
        out = [_sanitize(item, depth + 1) for item in material[:_MAX_COLLECTION_ITEMS]]
        if truncated > 0:
            out.append("<truncated:%d>" % truncated)
        return tuple(out) if isinstance(value, tuple) else out
    return _redact_string(str(value))[:256]


class NullJournal:
    """No-op journal used when diagnostics are unavailable or disabled."""

    def __init__(self, run_id=None) -> None:
        self._run_id = run_id or secrets.token_hex(8)

    @property
    def enabled(self) -> bool:
        return False

    @property
    def run_id(self) -> str:
        return self._run_id

    @property
    def current_path(self):
        return None

    def pseudonym(self, value: str) -> str:
        return _stable_pseudonym(self._run_id, value)

    def event(self, level: str, event_name: str, **fields) -> None:
        return None

    def exception_event(
        self, level: str, event_name: str, stage: str, exception: BaseException
    ) -> None:
        return None

    def close(self) -> None:
        return None

    def __enter__(self):
        return self

    def __exit__(self, exc_type, exc, tb):
        self.close()
        return False


class DiagnosticJournal:
    """Thread-safe bounded JSONL writer with aggregate pruning."""

    def __init__(
        self,
        logs_dir: str,
        run_id: str,
        max_part_bytes: int = _DEFAULT_MAX_PART_BYTES,
        max_dir_bytes: int = _DEFAULT_MAX_DIR_BYTES,
    ) -> None:
        self._lock = threading.Lock()
        self._logs_dir = logs_dir
        self._run_id = run_id
        self._max_part_bytes = int(max_part_bytes)
        self._max_dir_bytes = int(max_dir_bytes)
        self._sequence = 0
        self._part_index = 1
        self._bytes_in_part = 0
        self._managed_bytes = 0
        self._handle = None
        self._warned = False
        stamp = datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y%m%dT%H%M%SZ"
        )
        self._base_name = "emp-%s-%d-%s" % (stamp, os.getpid(), run_id)

    @property
    def enabled(self) -> bool:
        return self._handle is not None

    @property
    def run_id(self) -> str:
        return self._run_id

    @property
    def current_path(self):
        return self._path_for(self._part_index)

    def pseudonym(self, value: str) -> str:
        return _stable_pseudonym(self._run_id, value)

    def _path_for(self, index: int) -> str:
        return os.path.join(
            self._logs_dir, "%s-p%03d.jsonl" % (self._base_name, index)
        )

    def open(self) -> bool:
        """Create the private log directory and first part. Fail closed."""
        try:
            _assert_no_symlink_components(self._logs_dir)
            os.makedirs(self._logs_dir, exist_ok=True)
            _assert_no_symlink_components(self._logs_dir)
            info = os.lstat(self._logs_dir)
            if stat.S_ISLNK(info.st_mode) or not stat.S_ISDIR(info.st_mode):
                raise OSError("log directory is a symlink or not a directory")
            os.chmod(self._logs_dir, 0o700)
            with self._lock:
                self._open_new_part_locked()
                self._prune_locked()
            return True
        except Exception as exc:
            self._disable("setup failed: %s" % exc.__class__.__name__)
            return False

    def _open_new_part_locked(self) -> None:
        flags = getattr(os, "O_NOFOLLOW", 0)
        flags |= os.O_WRONLY | os.O_CREAT | os.O_EXCL
        descriptor = os.open(self._path_for(self._part_index), flags, 0o600)
        self._handle = os.fdopen(descriptor, "w", encoding="utf-8", newline="\n")
        self._bytes_in_part = 0

    def _rotate_locked(self) -> None:
        try:
            self._handle.flush()
            self._handle.close()
        except Exception:
            pass
        self._handle = None
        self._part_index += 1
        self._open_new_part_locked()

    def _prune_locked(self) -> None:
        active_path = self.current_path
        parts = []
        try:
            names = sorted(os.listdir(self._logs_dir))
        except OSError:
            return
        for name in names:
            if _PART_RE.match(name) is None:
                continue
            path = os.path.join(self._logs_dir, name)
            try:
                info = os.lstat(path)
                if not stat.S_ISREG(info.st_mode):
                    continue
            except OSError:
                continue
            parts.append((info.st_mtime_ns, name, path, info.st_size))
        total_bytes = sum(part[3] for part in parts)
        self._managed_bytes = total_bytes
        if total_bytes <= self._max_dir_bytes:
            return
        newest_path = max(parts, key=lambda part: (part[0], part[1]))[2]
        protected_paths = {active_path, newest_path}
        for _, _, path, size in sorted(parts, key=lambda part: (part[0], part[1])):
            if total_bytes <= self._max_dir_bytes:
                break
            if path in protected_paths:
                continue
            try:
                os.unlink(path)
                total_bytes -= size
            except OSError:
                continue
        self._managed_bytes = total_bytes

    def _disable(self, reason: str) -> None:
        handle, self._handle = self._handle, None
        if handle is not None:
            try:
                handle.flush()
            except Exception:
                pass
            try:
                handle.close()
            except Exception:
                pass
        if not self._warned:
            self._warned = True
            print("Diagnostic journal disabled (%s)." % reason, file=sys.stderr)

    def close(self) -> None:
        with self._lock:
            if self._handle is None:
                return
            try:
                self._handle.flush()
                self._handle.close()
            except Exception as exc:
                self._disable("close failed: %s" % exc.__class__.__name__)
                return
            self._handle = None

    def __enter__(self):
        return self
    def __exit__(self, exc_type, exc, tb):
        self.close()
        return False

    def event(self, level: str, event_name: str, **fields) -> None:
        try:
            payload = self._fit_record(
                level, _redact_string(str(event_name))[:256], fields
            )
        except Exception:
            return
        with self._lock:
            if self._handle is None:
                return
            try:
                self._write_line_locked(payload)
            except Exception as exc:
                self._disable("write failed: %s" % exc.__class__.__name__)

    def _fit_record(self, level: str, event_name: str, fields: dict) -> bytes:
        if level not in _LEVELS:
            level = "info"
        record = {
            "timestamp": _utc_now(),
            "sequence": None,
            "run_id": self._run_id,
            "level": level,
            "event": event_name,
            "fields": _sanitize(fields),
        }
        encoded = json.dumps(record, ensure_ascii=False, separators=(",", ":"))
        if len(encoded.encode("utf-8")) > _MAX_RECORD_BYTES:
            record["fields"] = {"_dropped": True, "_event": event_name}
            encoded = json.dumps(record, ensure_ascii=False, separators=(",", ":"))
        return encoded.encode("utf-8")

    def _write_line_locked(self, payload: bytes) -> None:
        record = json.loads(payload.decode("utf-8"))
        next_sequence = self._sequence + 1
        record["sequence"] = next_sequence
        payload = json.dumps(
            record, ensure_ascii=False, separators=(",", ":")
        ).encode("utf-8")
        if len(payload) > _MAX_RECORD_BYTES:
            record["fields"] = {"_dropped": True}
            payload = json.dumps(
                record, ensure_ascii=False, separators=(",", ":")
            ).encode("utf-8")
        if len(payload) > _MAX_RECORD_BYTES:
            raise ValueError("bounded diagnostic record exceeds limit")
        line_bytes = payload + b"\n"
        if (
            self._bytes_in_part > 0
            and self._bytes_in_part + len(line_bytes) > self._max_part_bytes
        ):
            self._rotate_locked()
            self._prune_locked()
        self._handle.write(payload.decode("utf-8"))
        self._handle.write("\n")
        self._handle.flush()
        self._sequence = next_sequence
        self._bytes_in_part += len(line_bytes)
        self._managed_bytes += len(line_bytes)
        if self._managed_bytes > self._max_dir_bytes:
            self._prune_locked()

    def exception_event(
        self, level: str, event_name: str, stage: str, exception: BaseException
    ) -> None:
        frames = []
        try:
            extracted = traceback.extract_tb(exception.__traceback__)
            for frame_summary in extracted[-_MAX_FRAMES:]:
                frames.append({
                    "file": os.path.basename(frame_summary.filename),
                    "line": frame_summary.lineno,
                    "function": frame_summary.name,
                })
        except Exception:
            frames = []
        self.event(
            level,
            event_name,
            stage=str(stage)[:256],
            exception_class=exception.__class__.__name__,
            frames=frames,
        )


def create_journal(config_path, max_part_bytes=None, max_dir_bytes=None):
    """Open a bounded journal under <config_path>/state/logs or a no-op."""
    kwargs = {}
    if max_part_bytes is not None:
        kwargs["max_part_bytes"] = max_part_bytes
    if max_dir_bytes is not None:
        kwargs["max_dir_bytes"] = max_dir_bytes
    logs_dir = os.path.join(str(config_path), "state", "logs")
    run_id = secrets.token_hex(8)
    journal = DiagnosticJournal(logs_dir, run_id, **kwargs)
    if not journal.open():
        return NullJournal(run_id)
    return journal
