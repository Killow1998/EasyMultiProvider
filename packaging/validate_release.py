"""Fail closed unless a complete native release set matches the source version."""

from __future__ import annotations

import argparse
import hashlib
import re
import sys
import tomllib
from pathlib import Path
from typing import Iterable, Optional, Sequence, Set


PROJECT_ROOT = Path(__file__).resolve().parents[1]
ARTIFACT_NAME = "EMP"
VERSION_PATTERN = re.compile(r'^__version__\s*=\s*"([^"]+)"\s*$', re.MULTILINE)
SHA256_PATTERN = re.compile(r"^[0-9a-f]{64}$")


class ReleaseValidationError(RuntimeError):
    pass


def source_version(project_root: Path = PROJECT_ROOT) -> str:
    pyproject = tomllib.loads(
        (project_root / "pyproject.toml").read_text(encoding="utf-8")
    )
    project_value = pyproject.get("project", {}).get("version")
    if not isinstance(project_value, str) or not project_value:
        raise ReleaseValidationError("pyproject.toml project version is unavailable")

    package_source = (
        project_root / "easy_multi_provider" / "__init__.py"
    ).read_text(encoding="utf-8")
    package_match = VERSION_PATTERN.search(package_source)
    if package_match is None:
        raise ReleaseValidationError("package __version__ is unavailable")
    package_value = package_match.group(1)
    if project_value != package_value:
        raise ReleaseValidationError(
            "source versions disagree: pyproject=%s package=%s"
            % (project_value, package_value)
        )
    return project_value


def primary_artifact_names(version: str) -> Set[str]:
    return {
        "EMP.exe",
        "EMP.zip",
        "EMP-linux-x86_64",
        "EMP-linux-x86_64.tar.gz",
        "EMP-linux-x86_64.deb",
        "EMP-macos-x86_64",
        "EMP-macos-x86_64.tar.gz",
        "EMP-macos-x86_64.dmg",
        "EMP-macos-arm64",
        "EMP-macos-arm64.tar.gz",
        "EMP-macos-arm64.dmg",
    }


def release_artifact_names(version: str) -> Set[str]:
    primary = primary_artifact_names(version)
    return primary | {name + ".sha256" for name in primary}


def public_artifact_names(version: str) -> Set[str]:
    return {
        "EMP.exe",
        "EMP-linux-x86_64.tar.gz",
        "EMP-linux-x86_64.deb",
        "EMP-macos-x86_64.dmg",
        "EMP-macos-arm64.dmg",
    }


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def _format_names(names: Iterable[str]) -> str:
    return ", ".join(sorted(names)) or "none"


def validate_release(tag: str, artifacts_root: Path) -> str:
    version = source_version()
    expected_tag = "v" + version
    if tag != expected_tag:
        raise ReleaseValidationError(
            "release tag %r does not match source version %r" % (tag, expected_tag)
        )
    if not artifacts_root.is_dir():
        raise ReleaseValidationError(
            "artifact directory does not exist: %s" % artifacts_root
        )

    actual_paths = {
        path.name: path for path in artifacts_root.iterdir() if path.is_file()
    }
    expected_names = release_artifact_names(version)
    actual_names = set(actual_paths)
    missing = expected_names - actual_names
    unexpected = actual_names - expected_names
    if missing or unexpected:
        raise ReleaseValidationError(
            "release manifest mismatch; missing: %s; unexpected: %s"
            % (_format_names(missing), _format_names(unexpected))
        )

    for artifact_name in sorted(primary_artifact_names(version)):
        sidecar = actual_paths[artifact_name + ".sha256"]
        fields = sidecar.read_text(encoding="ascii").strip().split(maxsplit=1)
        if len(fields) != 2:
            raise ReleaseValidationError("invalid checksum sidecar: %s" % sidecar.name)
        expected_digest, recorded_name = fields
        if not SHA256_PATTERN.fullmatch(expected_digest):
            raise ReleaseValidationError("invalid checksum digest: %s" % sidecar.name)
        if recorded_name != artifact_name:
            raise ReleaseValidationError(
                "checksum filename mismatch in %s" % sidecar.name
            )
        if _sha256(actual_paths[artifact_name]) != expected_digest:
            raise ReleaseValidationError("checksum mismatch: %s" % artifact_name)
    return version


def write_public_manifest(
    version: str, artifacts_root: Path, destination: Path
) -> None:
    names = sorted(public_artifact_names(version))
    missing = [name for name in names if not (artifacts_root / name).is_file()]
    if missing:
        raise ReleaseValidationError(
            "public release assets are missing: %s" % _format_names(missing)
        )
    destination.write_text(
        "".join("%s\n" % (artifacts_root / name) for name in names),
        encoding="utf-8",
    )


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Validate a complete EasyMultiProvider release asset set"
    )
    parser.add_argument("--tag", required=True, help="release tag, for example v0.9.6")
    parser.add_argument("--artifacts", required=True, type=Path)
    parser.add_argument(
        "--public-manifest",
        type=Path,
        help="write the five user-facing release asset paths after validation",
    )
    args = parser.parse_args(argv)
    try:
        version = validate_release(args.tag, args.artifacts)
        if args.public_manifest is not None:
            write_public_manifest(version, args.artifacts, args.public_manifest)
    except (OSError, UnicodeError, ReleaseValidationError) as exc:
        print("release validation failed: %s" % exc, file=sys.stderr)
        return 1
    print("release assets verified: 22 files; 5 public downloads for v%s" % version)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
