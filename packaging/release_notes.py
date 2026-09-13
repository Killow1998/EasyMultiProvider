"""Print this version's user-facing changelog for the GitHub release."""

import re
import sys
from pathlib import Path


def release_notes(tag):
    version = tag.removeprefix("v")
    changelog = (Path(__file__).resolve().parents[1] / "CHANGELOG.md").read_text(encoding="utf-8")
    section = re.search(r"^## " + re.escape(version) + r" \([^\n]+\)\n(.*?)(?=^## |\Z)",
                        changelog, re.MULTILINE | re.DOTALL)
    if section is None or not section.group(1).strip():
        raise ValueError("Missing release notes for " + tag)
    return section.group(1).strip()


if __name__ == "__main__":
    print(release_notes(sys.argv[1]))
