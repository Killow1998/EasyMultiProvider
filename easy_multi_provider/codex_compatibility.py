"""User-facing Codex CLI compatibility policy.

Version status is informational. Runtime adapters remain responsible for
validating the concrete protocol shapes they consume and failing closed when a
new Codex build changes one of those contracts.
"""

from __future__ import annotations

import re
from dataclasses import dataclass
from typing import Any, Dict, Optional, Tuple


SUPPORTED_MIN = (0, 149)
SUPPORTED_MAX = (0, 153)
RECOMMENDED = (0, 153)
RECOMMENDED_PATCH = 4
SUPPORTED_RANGE_LABEL = "0.149.x–0.153.x"
RECOMMENDED_LABEL = "0.153.4"

_VERSION_PATTERN = re.compile(
    r"(?<![A-Za-z0-9_.])v?(\d{1,4})\.(\d{1,4})\.(\d{1,6})"
    r"(?P<prerelease>-[0-9A-Za-z.-]+)?(?P<build>\+[0-9A-Za-z.-]+)?"
    r"(?![A-Za-z0-9_.+-])"
)


@dataclass(frozen=True)
class CodexCompatibility:
    installed: Optional[str]
    status: str

    def public(self) -> Dict[str, Any]:
        return {
            "installed": self.installed,
            "status": self.status,
            "supported_range": SUPPORTED_RANGE_LABEL,
            "recommended": RECOMMENDED_LABEL,
        }


def classify_codex_version(output: str) -> CodexCompatibility:
    """Parse bounded `codex --version` output without retaining raw text."""
    match = _VERSION_PATTERN.search(output if isinstance(output, str) else "")
    if match is None:
        return CodexCompatibility(None, "unknown")
    major, minor, patch = (int(match.group(index)) for index in (1, 2, 3))
    suffix = (match.group("prerelease") or "") + (match.group("build") or "")
    installed = "%d.%d.%d%s" % (major, minor, patch, suffix)
    release_line: Tuple[int, int] = (major, minor)
    if release_line < SUPPORTED_MIN:
        status = "unsupported"
    elif match.group("prerelease"):
        status = "unverified"
    elif release_line > SUPPORTED_MAX:
        status = "unverified"
    elif release_line == RECOMMENDED and patch >= RECOMMENDED_PATCH:
        status = "recommended"
    else:
        status = "supported"
    return CodexCompatibility(installed, status)


def unavailable_compatibility() -> CodexCompatibility:
    return CodexCompatibility(None, "unavailable")
