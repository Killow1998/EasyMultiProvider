from __future__ import annotations

import base64
import datetime
import hashlib
import json
import os
from pathlib import Path
from dataclasses import dataclass
from typing import Any

from .common import confined_path, fail, sha256_file
from .constants import (
    CALL_ID, CALL_ITEM_ID, OPAQUE, OPAQUE_ID, OUTPUT_ITEM_ID, SUMMARY,
    TOOL_ARGUMENTS, TOOL_NAME, TOOL_OUTPUT,
)


def find_rollout(codex_home: Path, thread_id: str) -> Path:
    sessions = codex_home / "sessions"
    matches: list[Path] = []
    for path in sessions.rglob("*.jsonl"):
        try:
            with path.open("rb") as stream:
                for raw in stream:
                    try:
                        record = json.loads(raw)
                    except (UnicodeDecodeError, json.JSONDecodeError):
                        continue
                    if record.get("type") == "session_meta":
                        payload = record.get("payload")
                        if isinstance(payload, dict) and payload.get("id") == thread_id:
                            matches.append(path)
                        break
        except OSError:
            continue
    if len(matches) != 1:
        fail(f"expected one rollout for a generated thread id; found {len(matches)}")
    try:
        matches[0].resolve(strict=True).relative_to(codex_home.resolve(strict=True))
    except (OSError, ValueError):
        fail("generated rollout escaped the disposable Codex home")
    return matches[0]


@dataclass(frozen=True)
class FileSnapshot:
    path: Path
    size: int
    sha256: str

    @classmethod
    def capture(cls, path: Path) -> "FileSnapshot":
        return cls(path=path, size=path.stat().st_size, sha256=sha256_file(path))


def append_items(path: Path, items: list[dict[str, Any]]) -> None:
    before = FileSnapshot.capture(path)
    largest = -1
    with path.open("rb") as source:
        for line in source:
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            ordinal = record.get("ordinal")
            if isinstance(ordinal, int):
                largest = max(largest, ordinal)
    with path.open("ab", buffering=0) as stream:
        for item in items:
            largest += 1
            record = {
                "timestamp": datetime.datetime.now(datetime.timezone.utc).isoformat(timespec="milliseconds").replace("+00:00", "Z"),
                "ordinal": largest,
                "type": "response_item",
                "payload": item,
            }
            stream.write(json.dumps(record, separators=(",", ":")).encode("utf-8") + b"\n")
        os.fsync(stream.fileno())
    if not file_prefix_matches(path, before.size, before.sha256):
        fail("fixture append did not preserve its original byte prefix")
    directory_fd = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)


def append_large_historical_events(
    path: Path,
    minimum_total_bytes: int = 128 * 1024 * 1024 + 1,
) -> dict[str, Any]:
    """Append ignored historical event rows incrementally until the rollout exceeds 128 MiB."""
    start_size = path.stat().st_size
    largest = -1
    with path.open("rb") as source:
        for line in source:
            try:
                record = json.loads(line)
            except (UnicodeDecodeError, json.JSONDecodeError):
                continue
            ordinal = record.get("ordinal")
            if isinstance(ordinal, int):
                largest = max(largest, ordinal)

    padding = "H" * (512 * 1024)
    records = 0
    added = 0
    digest = hashlib.sha256()
    with path.open("ab", buffering=0) as stream:
        while start_size + added <= minimum_total_bytes:
            largest += 1
            record = {
                "timestamp": "2026-01-01T00:00:00Z",
                "ordinal": largest,
                "type": "event_msg",
                "payload": {"type": "token_count", "fixture_padding": padding},
            }
            encoded = json.dumps(record, separators=(",", ":")).encode("utf-8") + b"\n"
            stream.write(encoded)
            digest.update(encoded)
            added += len(encoded)
            records += 1
        os.fsync(stream.fileno())

    if path.stat().st_size <= 128 * 1024 * 1024:
        fail("large historical rollout fixture did not exceed 128 MiB")
    directory_fd = os.open(path.parent, os.O_RDONLY | getattr(os, "O_DIRECTORY", 0))
    try:
        os.fsync(directory_fd)
    finally:
        os.close(directory_fd)
    return {
        "minimum_total_bytes": minimum_total_bytes,
        "rollout_bytes": path.stat().st_size,
        "added_bytes": added,
        "record_count": records,
        "appended_sha256": digest.hexdigest(),
        "storage": "disk-backed incrementally appended JSONL",
        "context_padding_type": "event_msg/token_count",
    }


def file_prefix_matches(path: Path, prefix_size: int, expected_sha256: str) -> bool:
    digest = hashlib.sha256()
    remaining = prefix_size
    with path.open("rb") as source:
        while remaining:
            chunk = source.read(min(1024 * 1024, remaining))
            if not chunk:
                return False
            digest.update(chunk)
            remaining -= len(chunk)
    return digest.hexdigest() == expected_sha256


def files_equal_prefix(prefix: Path, target: Path, size: int) -> bool:
    remaining = size
    with prefix.open("rb") as left, target.open("rb") as right:
        while remaining:
            amount = min(1024 * 1024, remaining)
            left_chunk = left.read(amount)
            right_chunk = right.read(amount)
            if len(left_chunk) != amount or left_chunk != right_chunk:
                return False
            remaining -= amount
    return True


def parent_fixture_items() -> list[dict[str, Any]]:
    encoded = base64.urlsafe_b64encode(SUMMARY.encode("utf-8")).decode("ascii").rstrip("=")
    return [
        {
            "type": "compaction",
            "id": "compaction_emp_acceptance_001",
            "encrypted_content": "emp1:" + encoded,
        },
        {
            "type": "reasoning",
            "id": OPAQUE_ID,
            "summary": [],
            "encrypted_content": OPAQUE,
        },
        {
            "type": "function_call",
            "id": CALL_ITEM_ID,
            "call_id": CALL_ID,
            "name": TOOL_NAME,
            "arguments": TOOL_ARGUMENTS,
        },
        {
            "type": "function_call_output",
            "id": OUTPUT_ITEM_ID,
            "call_id": CALL_ID,
            "name": TOOL_NAME,
            "output": TOOL_OUTPUT,
        },
    ]


def inspect_reports(
    codex_home: Path,
    state_dir: Path,
    original_files: dict[str, FileSnapshot],
) -> dict[str, Any]:
    codex_home = codex_home.resolve(strict=True)
    repair_roots = [
        codex_home / ".emp-history-repair",
        state_dir / "history-repair",
    ]
    manifests: set[Path] = set()
    for repair_root in repair_roots:
        if not repair_root.exists():
            continue
        try:
            repair_root.resolve(strict=True).relative_to(codex_home)
        except (OSError, ValueError):
            fail("history repair manifest directory escapes the disposable Codex home")
        if not repair_root.is_dir():
            fail("history repair manifest directory is not a directory")
        for path in repair_root.glob("repair-*/manifest.json"):
            manifests.add(confined_path(codex_home, str(path.relative_to(codex_home)), "manifest"))

    checked: set[str] = set()
    prefix_checked: set[str] = set()
    for path in sorted(manifests):
        try:
            manifest = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError):
            fail("EMP created an unreadable history repair manifest")
        if not isinstance(manifest, dict) or manifest.get("status") != "committed":
            fail("EMP history repair transaction did not reach committed state")
        entries = manifest.get("entries")
        if not isinstance(entries, list):
            fail("EMP history repair manifest has no entry list")
        for entry in entries:
            if not isinstance(entry, dict):
                fail("EMP history repair manifest has an invalid entry")
            backup = confined_path(codex_home, entry.get("backup"), "backup")
            target = confined_path(codex_home, entry.get("target"), "target")
            thread_id = entry.get("thread_id")
            if thread_id not in original_files:
                continue
            original = original_files[thread_id]
            backup_size = backup.stat().st_size
            backup_hash = sha256_file(backup)
            if backup_size != entry.get("before_bytes") or backup_hash != entry.get("before_sha256"):
                fail("history repair backup does not match its manifest")
            if backup_size != original.size or backup_hash != original.sha256:
                fail("history repair backup differs from the exact pre-restore rollout bytes")
            if target.stat().st_size < original.size or not files_equal_prefix(backup, target, original.size):
                fail("history repair target does not preserve the exact original JSONL byte prefix")
            target_hash = sha256_file(target)
            if target_hash != entry.get("after_sha256"):
                fail("history repair target does not match its manifest")
            checked.add(thread_id)
            prefix_checked.add(thread_id)
    return {
        "manifest_count": len(manifests),
        "backup_threads_verified": sorted(checked),
        "target_prefix_threads_verified": sorted(prefix_checked),
    }
