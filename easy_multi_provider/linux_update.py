"""Read-only detection for Linux package-managed EMP installations."""

import os
import subprocess
from pathlib import Path

from .self_update import UpdateError, version_tuple


PACKAGE = "easy-multi-provider"


def system_environment():
    """Return an environment suitable for read-only system package queries."""

    environment = os.environ.copy()
    original = environment.pop("LD_LIBRARY_PATH_ORIG", None)
    if original:
        environment["LD_LIBRARY_PATH"] = original
    else:
        environment.pop("LD_LIBRARY_PATH", None)
    environment["LC_ALL"] = "C"
    return environment


def package_version(target):
    """Return the installed deb version, without modifying the installation."""

    target = Path(target)
    if not Path("/usr/bin/dpkg-query").is_file():
        return None
    owner = subprocess.run(
        ["/usr/bin/dpkg-query", "--search", str(target)],
        capture_output=True,
        text=True,
        timeout=10,
        env=system_environment(),
    )
    matches = [
        line.rsplit(": ", 1)[0]
        for line in owner.stdout.splitlines()
        if line.endswith(": " + str(target))
    ]
    if not matches:
        if owner.returncode not in (0, 1):
            raise UpdateError("package_query_failed")
        return None
    if matches != [PACKAGE] or target != Path("/usr/bin/EMP"):
        raise UpdateError("unsupported_installation")
    result = subprocess.run(
        ["/usr/bin/dpkg-query", "--show", "--showformat=${Status}\t${Version}", PACKAGE],
        capture_output=True,
        text=True,
        timeout=10,
        env=system_environment(),
    )
    status, _, version = result.stdout.strip().partition("\t")
    if result.returncode or status != "install ok installed":
        raise UpdateError("package_query_failed")
    version_tuple(version)
    return version
