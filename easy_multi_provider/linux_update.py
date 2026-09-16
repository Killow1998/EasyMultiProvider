"""System-owned Linux installations: OS authorization, never browser passwords.

Only dpkg/install run elevated. EMP, version probes and the restart worker keep
the desktop user's identity. No persistent Polkit policy or password cache is added.
"""
import hashlib
import json
import os
import shutil
import subprocess
import tarfile
from pathlib import Path

from .self_update import (
    MAX_PACKAGE_BYTES, RELEASE_API, UpdateError, download_package,
    release_asset, version_tuple,
)

DEB_ASSET = "EMP-linux-x86_64.deb"
PACKAGE = "easy-multi-provider"


def system_environment():
    environment = os.environ.copy()
    # PyInstaller adjusts the library path for its own bootloader. System tools
    # must use system libraries, including before pkexec sanitizes the root env.
    original = environment.pop("LD_LIBRARY_PATH_ORIG", None)
    if original:
        environment["LD_LIBRARY_PATH"] = original
    else:
        environment.pop("LD_LIBRARY_PATH", None)
    environment["LC_ALL"] = "C"
    return environment


def package_version(target):
    """Return the installed deb version, not a guess from its directory name."""
    if not Path("/usr/bin/dpkg-query").is_file():
        return None
    owner = subprocess.run(
        ["/usr/bin/dpkg-query", "--search", str(target)],
        capture_output=True, text=True, timeout=10, env=system_environment(),
    )
    matches = [line.rsplit(": ", 1)[0] for line in owner.stdout.splitlines()
               if line.endswith(": " + str(target))]
    if not matches:
        if owner.returncode not in (0, 1):
            raise UpdateError("package_query_failed")
        return None
    if matches != [PACKAGE] or target != Path("/usr/bin/EMP"):
        raise UpdateError("unsupported_installation")
    result = subprocess.run(
        ["/usr/bin/dpkg-query", "--show", "--showformat=${Status}\t${Version}", PACKAGE],
        capture_output=True, text=True, timeout=10, env=system_environment(),
    )
    status, _, version = result.stdout.strip().partition("\t")
    if result.returncode or status != "install ok installed":
        raise UpdateError("package_query_failed")
    version_tuple(version)
    return version


def prepare_deb(package, job, version):
    metadata = subprocess.run(
        ["/usr/bin/dpkg-deb", "--show", "--showformat=${Package}\t${Version}\t${Architecture}", str(package)],
        capture_output=True, text=True, timeout=30, env=system_environment(),
    )
    if metadata.returncode or metadata.stdout.strip() != PACKAGE + "\t" + version + "\tamd64":
        raise UpdateError("invalid_package")
    # Read only the executable, without unpacking package links or maintainer
    # scripts into the user's home. dpkg installs the verified package later.
    archive_path = job / "data.tar"
    candidate = job / "candidate"
    try:
        with archive_path.open("xb") as output:
            result = subprocess.run(["/usr/bin/dpkg-deb", "--fsys-tarfile", str(package)],
                                    stdout=output, stderr=subprocess.DEVNULL, timeout=60, env=system_environment())
        if result.returncode or archive_path.stat().st_size > MAX_PACKAGE_BYTES * 2:
            raise UpdateError("invalid_package")
        with tarfile.open(archive_path) as archive:
            members = [item for item in archive if item.name in {"./usr/bin/EMP", "usr/bin/EMP"}]
            if len(members) != 1 or not members[0].isfile() or not 0 < members[0].size <= MAX_PACKAGE_BYTES:
                raise UpdateError("invalid_package")
            with archive.extractfile(members[0]) as source, candidate.open("xb") as output:
                shutil.copyfileobj(source, output)
        candidate.chmod(0o755)
        return candidate, candidate
    finally:
        archive_path.unlink(missing_ok=True)


def file_digest(path):
    with Path(path).open("rb") as source:
        digest = hashlib.sha256()
        for block in iter(lambda: source.read(256 * 1024), b""):
            digest.update(block)
    return digest.hexdigest()


def prepare_rollback(job, target, installed_version, opener):
    if os.geteuid() == 0:
        raise UpdateError("update_requires_desktop_user")
    if not Path("/usr/bin/pkexec").is_file():
        raise UpdateError("authorization_unavailable")
    previous = job / "previous"
    shutil.copy2(target, previous)
    owner = target.stat()
    info = {"kind": "deb" if installed_version else "binary",
            "previous_digest": file_digest(previous), "uid": owner.st_uid, "gid": owner.st_gid}
    if installed_version:
        # Keep a verified old package as well: restoring just /usr/bin/EMP would
        # leave dpkg's version and the desktop resources inconsistent.
        rollback = job / "rollback"
        rollback.mkdir()
        try:
            url = RELEASE_API.rsplit("/", 1)[0] + "/tags/v" + installed_version
            with opener(url) as response:
                raw = response.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise UpdateError("invalid_release")
            asset = release_asset(json.loads(raw), DEB_ASSET, current="0.0.0")
            if not asset or asset["version"] != installed_version:
                raise UpdateError("invalid_release")
            package = rollback / DEB_ASSET
            download_package(asset, package, opener=opener)
            _, binary = prepare_deb(package, rollback, installed_version)
            if file_digest(binary) != info["previous_digest"]:
                raise UpdateError("installation_changed")
            info.update(previous_version=installed_version, previous_package_digest=asset["sha256"])
        except Exception as exc:
            raise UpdateError("rollback_package_unavailable") from exc
    return info


def apply_install(plan, job, *, rollback=False):
    """Request one-time OS authorization for a fixed system tool invocation."""
    info = plan["linux_install"]
    target = Path(plan["target"])
    if info["kind"] == "deb":
        source = job / "rollback" / DEB_ASSET if rollback else job / DEB_ASSET
        digest = info["previous_package_digest"] if rollback else info["package_digest"]
        command = ["/usr/bin/dpkg", "--install", str(source)]
    else:
        source = job / "previous" if rollback else Path(plan["candidate"])
        digest = info["previous_digest"] if rollback else info["candidate_digest"]
        # GNU install unlinks the old executable, preserving the inode of a
        # running EMP. No shell, recursive operation or permission relaxation.
        command = ["/usr/bin/install", "--no-target-directory", "--mode=755",
                   "--owner=" + str(info["uid"]), "--group=" + str(info["gid"]),
                   "--", str(source), str(target)]
    if source.is_symlink() or target.is_symlink() or file_digest(source) != digest:
        raise UpdateError("checksum_mismatch")
    # No internal text agent: this background job has no interactive terminal.
    # Desktop Polkit owns the dialog and password. Do not kill an authorized
    # package manager on a timer; interrupting dpkg can corrupt its database.
    result = subprocess.run(
        ["/usr/bin/pkexec", "--disable-internal-agent", *command],
        stdin=subprocess.DEVNULL, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL,
        env=system_environment(),
    )
    if result.returncode == 126:
        raise UpdateError("authorization_cancelled")
    if result.returncode == 127:
        raise UpdateError("authorization_unavailable")
    if result.returncode:
        raise UpdateError("system_install_failed")
    expected = info["previous_digest"] if rollback else info["candidate_digest"]
    if file_digest(target) != expected:
        raise UpdateError("system_install_failed")
    if info["kind"] == "deb":
        expected_version = info["previous_version"] if rollback else plan["version"]
        if package_version(target) != expected_version:
            raise UpdateError("system_install_failed")


def valid_user_job(job):
    """System installs stage in a private user directory, not beside /usr/bin."""
    return (os.geteuid() != 0 and job.stat().st_uid == os.geteuid()
            and job.stat().st_mode & 0o077 == 0)
