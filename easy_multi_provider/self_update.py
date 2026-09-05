"""User-triggered stable updates. No credentials, shell commands or forced drain."""
from __future__ import annotations

import hashlib
import json
import os
import platform
import re
import shutil
import subprocess
import sys
import tarfile
import tempfile
import threading
import time
import uuid
from contextlib import contextmanager
from pathlib import Path
from urllib.error import HTTPError
from urllib.parse import urlsplit
from urllib.request import HTTPRedirectHandler, Request, build_opener

import psutil

from . import __version__
from .diagnostic_journal import NullJournal

REPO_URL = "https://github.com/Killow1998/EasyMultiProvider"
RELEASE_API = "https://api.github.com/repos/Killow1998/EasyMultiProvider/releases/latest"
MAX_PACKAGE_BYTES = 512 * 1024 * 1024
_VERSION = re.compile(r"v?(\d+)\.(\d+)\.(\d+)$")
_DIGEST = re.compile(r"sha256:([a-f0-9]{64})$")


class UpdateError(Exception):
    """Fixed codes only; never expose URLs, filesystem paths or network errors."""


def version_tuple(value):
    match = _VERSION.fullmatch(str(value))
    if not match:
        raise UpdateError("invalid_release")
    return tuple(int(part) for part in match.groups())


def asset_name(system=None, machine=None):
    system, machine = system or platform.system(), (machine or platform.machine()).lower()
    arch = "x86_64" if machine in {"amd64", "x86_64"} else "arm64" if machine in {"arm64", "aarch64"} else ""
    if system == "Windows" and arch == "x86_64":
        return "EMP.exe"
    if system == "Linux" and arch == "x86_64":
        return "EMP-linux-x86_64.tar.gz"
    if system == "Darwin" and arch:
        return "EMP-macos-%s.dmg" % arch
    raise UpdateError("unsupported_platform")


def release_asset(release, name, current=__version__):
    if not isinstance(release, dict) or release.get("draft") or release.get("prerelease"):
        raise UpdateError("invalid_release")
    tag = release.get("tag_name", "")
    latest = version_tuple(tag)
    if latest <= version_tuple(current):
        return None
    matches = [item for item in release.get("assets", []) if isinstance(item, dict) and item.get("name") == name]
    if len(matches) != 1:
        raise UpdateError("package_missing")
    item = matches[0]
    digest = _DIGEST.fullmatch(str(item.get("digest", "")))
    if not digest:
        raise UpdateError("checksum_missing")
    url = item.get("browser_download_url")
    if url != REPO_URL + "/releases/download/" + tag + "/" + name:
        raise UpdateError("invalid_download_url")
    size = item.get("size")
    if isinstance(size, bool) or not isinstance(size, int) or not 0 < size <= MAX_PACKAGE_BYTES:
        raise UpdateError("invalid_package_size")
    return {"version": tag.lstrip("v"), "name": name, "url": url, "sha256": digest[1], "size": size}


def _allowed_url(url):
    parsed = urlsplit(url)
    return (parsed.scheme == "https" and not parsed.username and not parsed.password
            and parsed.port in (None, 443) and parsed.hostname in {
                "api.github.com", "github.com", "release-assets.githubusercontent.com",
                "objects.githubusercontent.com",
            })


class _ReleaseRedirect(HTTPRedirectHandler):
    def redirect_request(self, req, fp, code, msg, headers, newurl):
        if not _allowed_url(newurl):
            raise UpdateError("invalid_download_url")
        return super().redirect_request(req, fp, code, msg, headers, newurl)


def _open(url):
    if not _allowed_url(url):
        raise UpdateError("invalid_download_url")
    return build_opener(_ReleaseRedirect()).open(Request(url, headers={
        "User-Agent": "EMP/" + __version__, "Accept": "application/vnd.github+json" if url == RELEASE_API else "application/octet-stream",
    }), timeout=20)


def download_package(asset, destination, progress=lambda value: None, opener=_open):
    digest, size, deadline = hashlib.sha256(), 0, time.monotonic() + 600
    try:
        with opener(asset["url"]) as response, destination.open("xb") as target:
            while True:
                block = response.read(256 * 1024)
                if not block:
                    break
                size += len(block)
                if size > asset["size"] or time.monotonic() > deadline:
                    raise UpdateError("invalid_package_size")
                digest.update(block)
                target.write(block)
                progress(min(99, int(size * 100 / asset["size"])))
        if size != asset["size"] or digest.hexdigest() != asset["sha256"]:
            raise UpdateError("checksum_mismatch")
    except Exception:
        destination.unlink(missing_ok=True)
        raise


class UpdateGate:
    """Count actual requests, not idle persistent WebSocket connections."""
    def __init__(self):
        self.condition = threading.Condition()
        self.active = 0
        self.draining = False

    def enter(self):
        with self.condition:
            if self.draining:
                return False
            self.active += 1
            return True

    def leave(self):
        with self.condition:
            self.active -= 1
            self.condition.notify_all()

    def drain(self, timeout=300):
        with self.condition:
            self.draining = True
            if not self.condition.wait_for(lambda: self.active == 0, timeout):
                self.draining = False
                raise UpdateError("requests_busy")

    def reopen(self):
        with self.condition:
            self.draining = False


def installation_target(executable):
    executable = Path(executable).absolute()
    if executable.is_symlink():
        raise UpdateError("unsupported_installation")
    if sys.platform == "darwin":
        app = executable.parent.parent.parent
        if app.suffix != ".app" or executable.relative_to(app).as_posix() != "Contents/Resources/EMP":
            raise UpdateError("unsupported_installation")
        return app, "Contents/Resources/EMP"
    return executable, ""


def prepare_candidate(package, job, relative_binary):
    if package.suffix == ".exe":
        candidate = job / "candidate.exe"
        package.rename(candidate)
    elif package.name.endswith(".tar.gz"):
        candidate = job / "candidate"
        with tarfile.open(package, "r:gz") as archive:
            members = [entry for entry in archive if entry.name == "EMP/EMP"]
            if len(members) != 1 or not members[0].isfile() or not 0 < members[0].size <= MAX_PACKAGE_BYTES:
                raise UpdateError("invalid_package")
            with archive.extractfile(members[0]) as source, candidate.open("xb") as target:
                shutil.copyfileobj(source, target, 256 * 1024)
        candidate.chmod(0o755)
    else:
        candidate, mount = job / "candidate.app", job / "mount"
        mount.mkdir()
        try:
            subprocess.run(["/usr/bin/hdiutil", "attach", str(package), "-readonly", "-nobrowse", "-mountpoint", str(mount)],
                           check=True, timeout=90, stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            app = mount / "EMP.app"
            # Current EMP bundles have no symlinks. Never follow an archive link
            # while copying a downloaded application into the install directory.
            if not app.is_dir() or any(path.is_symlink() for path in app.rglob("*")):
                raise UpdateError("invalid_package")
            shutil.copytree(app, candidate)
        finally:
            subprocess.run(["/usr/bin/hdiutil", "detach", str(mount)], timeout=30,
                           stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
    binary = candidate / relative_binary if relative_binary else candidate
    if not binary.is_file():
        raise UpdateError("invalid_package")
    return candidate, binary


def _child_environment():
    environment = os.environ.copy()
    environment["PYINSTALLER_RESET_ENVIRONMENT"] = "1"
    environment.pop("EMP_UPDATE_READY", None)
    return environment


@contextmanager
def _quiet_launch():
    if os.name != "nt":
        yield
        return
    import ctypes
    from ctypes import wintypes
    kernel = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel.GetThreadErrorMode.restype = wintypes.DWORD
    kernel.SetThreadErrorMode.argtypes = [wintypes.DWORD, ctypes.POINTER(wintypes.DWORD)]
    kernel.SetThreadErrorMode.restype = wintypes.BOOL
    previous = wintypes.DWORD()
    # CreateProcess itself can block on a bad-image/critical-error dialog, before
    # Popen returns and its timeout can start. Affect only this launch thread.
    if not kernel.SetThreadErrorMode(kernel.GetThreadErrorMode() | 0x8003, ctypes.byref(previous)):
        raise ctypes.WinError(ctypes.get_last_error())
    try:
        yield
    finally:
        kernel.SetThreadErrorMode(previous.value, None)


def _spawn(command, **kwargs):
    kwargs.setdefault("stdin", subprocess.DEVNULL)
    kwargs.setdefault("stdout", subprocess.DEVNULL)
    kwargs.setdefault("stderr", subprocess.DEVNULL)
    kwargs.setdefault("env", _child_environment())
    if os.name == "nt":
        kwargs["creationflags"] = subprocess.CREATE_NO_WINDOW | subprocess.CREATE_NEW_PROCESS_GROUP
    else:
        kwargs["start_new_session"] = True
    with _quiet_launch():
        return subprocess.Popen(command, **kwargs)


class UpdateManager:
    def __init__(self, config_path, *, executable=None, opener=_open, journal=None):
        self.lock = threading.RLock()
        self.gate = UpdateGate()
        self.config_path = Path(config_path).resolve()
        self.executable = Path(executable or sys.executable).absolute()
        self.supported = bool(executable or getattr(sys, "frozen", False))
        self.opener = opener
        self.journal = journal if journal is not None else NullJournal()
        self.asset = None
        self.shutdown = None
        self.restart_args = []
        self.installing = False
        self.data = {"state": "idle", "current_version": __version__, "latest_version": None,
                     "progress": 0, "error": "", "supported": self.supported}
        if os.environ.pop("EMP_UPDATE_RESULT", "") == "rolled_back":
            self._set(state="error", error="install_rolled_back")

    def snapshot(self):
        with self.lock:
            return dict(self.data)

    def _set(self, **values):
        with self.lock:
            self.data.update(values)
            if "state" in values:
                try:
                    self.journal.event("info", "update_state", state=self.data["state"],
                                       result_class=self.data["error"], version=self.data["latest_version"])
                except Exception:
                    pass

    def start(self, operation):
        with self.lock:
            if self.data["state"] in {"checking", "downloading", "verifying", "waiting", "installing"}:
                return self.snapshot()
            if operation == "check":
                self.asset = None
                self._set(state="checking", error="", latest_version=None, progress=0)
                target = self._check
            elif operation == "install" and self.asset and self.supported and self.shutdown:
                self._set(state="downloading", error="", progress=0)
                target = self._install
            else:
                raise UpdateError("update_unavailable")
            threading.Thread(target=self._run, args=(target,), daemon=True, name="emp-update").start()
            return self.snapshot()

    def _run(self, operation):
        try:
            operation()
        except UpdateError as exc:
            self.gate.reopen()
            self._set(state="error", error=str(exc))
        except PermissionError:
            self.gate.reopen()
            self._set(state="error", error="directory_not_writable")
        except Exception:
            self.gate.reopen()
            self._set(state="error", error="update_failed")

    def _check(self):
        try:
            with self.opener(RELEASE_API) as response:
                raw = response.read(1024 * 1024 + 1)
            if len(raw) > 1024 * 1024:
                raise UpdateError("invalid_release")
            release = json.loads(raw)
        except HTTPError as exc:
            if exc.code == 404:
                self.asset = None
                self._set(state="no_release", latest_version=None)
                return
            raise UpdateError("check_failed") from None
        self.asset = release_asset(release, asset_name())
        self._set(state="available" if self.asset else "current", latest_version=release["tag_name"].lstrip("v"))

    def _install(self):
        target, relative_binary = installation_target(self.executable)
        if not target.exists() or not os.access(target.parent, os.W_OK):
            raise UpdateError("directory_not_writable")
        job = Path(tempfile.mkdtemp(prefix=".emp-update-", dir=target.parent))
        handed_off = False
        try:
            package = job / self.asset["name"]
            download_package(self.asset, package, lambda value: self._set(progress=value), self.opener)
            self._set(state="verifying", progress=100)
            candidate, binary = prepare_candidate(package, job, relative_binary)
            with _quiet_launch():
                probe = subprocess.run([str(binary), "--version"], capture_output=True, timeout=30, env=_child_environment(),
                                       creationflags=subprocess.CREATE_NO_WINDOW if os.name == "nt" else 0)
            if probe.returncode or probe.stdout.decode("utf-8", "replace").strip() != "EMP " + self.asset["version"]:
                raise UpdateError("version_mismatch")
            helper = job / ("worker.exe" if os.name == "nt" else "worker")
            shutil.copy2(self.executable, helper)
            process = psutil.Process()
            parents = [{"pid": process.pid, "created": process.create_time()}]
            parent = process.parent()
            if parent and Path(parent.exe()).resolve() == self.executable.resolve():
                parents.append({"pid": parent.pid, "created": parent.create_time()})
            plan = {"target": str(target), "candidate": str(candidate), "relative_binary": relative_binary,
                    "parents": parents, "args": self.restart_args, "version": self.asset["version"], "nonce": uuid.uuid4().hex}
            (job / "plan.json").write_text(json.dumps(plan), encoding="utf-8")
            self._set(state="waiting")
            self.gate.drain()
            worker = _spawn([str(helper), "--emp-apply-update", str(job / "plan.json")])
            deadline = time.monotonic() + 30
            while not (job / "worker-ready").is_file():
                if worker.poll() is not None or time.monotonic() > deadline:
                    _stop_child(worker)
                    raise UpdateError("worker_failed")
                time.sleep(.1)
            handed_off = True
            self.installing = True
            self._set(state="installing")
            self.shutdown()
        finally:
            if not handed_off:
                shutil.rmtree(job, ignore_errors=True)


def _process_alive(identity):
    try:
        return psutil.Process(identity["pid"]).create_time() == identity["created"]
    except psutil.NoSuchProcess:
        return False


def _stop_child(child):
    if child.poll() is not None:
        return
    try:
        owned = psutil.Process(child.pid)
        processes = owned.children(recursive=True) + [owned]
        for process in processes:
            process.terminate()
        _, alive = psutil.wait_procs(processes, timeout=5)
        for process in alive:
            process.kill()
        psutil.wait_procs(alive, timeout=5)
    except psutil.NoSuchProcess:
        pass


def run_update_worker(plan_path):
    """Detached old executable replaces only its validated sibling installation."""
    plan_path = Path(plan_path).resolve()
    job = plan_path.parent
    plan = json.loads(plan_path.read_text(encoding="utf-8"))
    target, candidate = Path(plan["target"]), Path(plan["candidate"])
    if (not job.name.startswith(".emp-update-") or target.parent.resolve() != job.parent
            or candidate.parent.resolve() != job or target.is_symlink() or candidate.is_symlink()
            or plan["relative_binary"] not in {"", "Contents/Resources/EMP"}):
        raise UpdateError("invalid_update_plan")
    def phase(name):
        # The old journal is closed during replacement. Retain only this fixed
        # phase so a stopped worker can be diagnosed without paths or payloads.
        try:
            pending = job / "worker-status.tmp"
            pending.write_text(json.dumps({"phase": name}), encoding="utf-8")
            pending.replace(job / "worker-status.json")
        except OSError:
            pass
    phase("waiting_for_exit")
    (job / "worker-ready").touch()
    deadline = time.monotonic() + 90
    while any(_process_alive(identity) for identity in plan["parents"]):
        if time.monotonic() > deadline:
            raise UpdateError("shutdown_timeout")
        time.sleep(.2)
    backup = job / "previous"
    binary = target / plan["relative_binary"] if plan["relative_binary"] else target
    child = None
    try:
        phase("replacing")
        target.rename(backup)
        candidate.rename(target)
        environment = _child_environment()
        environment["EMP_UPDATE_READY"] = str(job / "ready.json")
        phase("starting")
        child = _spawn([str(binary), *plan["args"]], env=environment)
        phase("waiting_for_startup")
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            ready = job / "ready.json"
            if ready.is_file():
                record = json.loads(ready.read_text(encoding="utf-8"))
                if record == {"version": plan["version"], "nonce": plan["nonce"]}:
                    phase("complete")
                    (job / "success").touch()
                    return 0
            if child.poll() is not None:
                break
            time.sleep(.2)
        # Only terminate the process tree launched by this update worker.
        raise UpdateError("startup_failed")
    except Exception:
        phase("restoring")
        if child is not None:
            _stop_child(child)
        try:
            # If the first rename failed, the old installation is still in place.
            # Never rename or delete that working copy during rollback.
            if backup.exists():
                if target.exists():
                    target.rename(job / "failed")
                backup.rename(target)
            environment = _child_environment()
            environment["EMP_UPDATE_RESULT"] = "rolled_back"
            phase("restarting_previous")
            _spawn([str(binary), *plan["args"]], env=environment)
            phase("rolled_back")
            (job / "rolled-back").touch()
        except Exception:
            phase("recovery_required")
            # Leave both files recoverable if permissions or antivirus prevent
            # rollback. No raw OS exception (possibly containing paths) is stored.
            (job / "recovery-required").touch()
        return 1


def mark_update_ready():
    marker = os.environ.pop("EMP_UPDATE_READY", "")
    if not marker:
        return
    ready = Path(marker).resolve()
    job = ready.parent
    plan = json.loads((job / "plan.json").read_text(encoding="utf-8"))
    target, _ = installation_target(sys.executable)
    if ready.name != "ready.json" or Path(plan["target"]).resolve() != target.resolve() or job.parent != target.parent or not job.name.startswith(".emp-update-"):
        raise UpdateError("invalid_update_plan")
    pending = ready.with_suffix(".tmp")
    pending.write_text(json.dumps({"version": __version__, "nonce": plan["nonce"]}), encoding="utf-8")
    pending.replace(ready)
    def cleanup():
        # The onefile worker needs time to exit before Windows can delete it.
        for _ in range(30):
            time.sleep(2)
            if (job / "success").is_file():
                try:
                    shutil.rmtree(job)
                    return
                except OSError:
                    pass
    threading.Thread(target=cleanup, daemon=True, name="emp-update-cleanup").start()
