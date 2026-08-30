"""Build and smoke-test one native EasyMultiProvider distribution."""

from __future__ import annotations

import argparse
import hashlib
import http.client
import json
import os
import platform
import plistlib
import re
import shutil
import socket
import subprocess
import sys
import tarfile
import tempfile
import time
from urllib.parse import urlsplit
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence

import psutil


PROJECT_ROOT = Path(__file__).resolve().parents[1]
PACKAGE_NAME = "easy-multi-provider"
PRODUCT_NAME = "EasyMultiProvider"
VERSION_PATTERN = re.compile(r'^__version__\s*=\s*"([^"]+)"\s*$', re.MULTILINE)


@dataclass(frozen=True)
class Target:
    system: str
    os_id: str
    arch: str
    executable_suffix: str
    deb_arch: Optional[str] = None

    @property
    def identity(self) -> str:
        return "%s-%s" % (self.os_id, self.arch)


@dataclass(frozen=True)
class PackageIcons:
    windows: Path
    macos: Path
    linux: Path


def current_target() -> Target:
    system = platform.system()
    raw_arch = platform.machine().lower()
    if raw_arch in ("amd64", "x86_64"):
        arch = "x86_64"
    elif raw_arch in ("arm64", "aarch64"):
        arch = "arm64"
    else:
        raise RuntimeError("unsupported packaging architecture: %s" % raw_arch)

    if system == "Windows" and arch == "x86_64":
        return Target(system, "windows", arch, ".exe")
    if system == "Linux" and arch == "x86_64":
        return Target(system, "linux", arch, "", deb_arch="amd64")
    if system == "Darwin" and arch in ("x86_64", "arm64"):
        return Target(system, "macos", arch, "")
    raise RuntimeError("unsupported packaging target: %s/%s" % (system, arch))


def project_version() -> str:
    source = (PROJECT_ROOT / "easy_multi_provider" / "__init__.py").read_text(
        encoding="utf-8"
    )
    match = VERSION_PATTERN.search(source)
    if match is None:
        raise RuntimeError("package version is unavailable")
    return match.group(1)


def _remove_managed_tree(path: Path, parent: Path) -> None:
    resolved_path = path.resolve()
    resolved_parent = parent.resolve()
    if resolved_path.parent != resolved_parent:
        raise RuntimeError("refusing to clean an unmanaged build path")
    if resolved_path.exists():
        shutil.rmtree(str(resolved_path))


def _run(command: Sequence[str], environment: Optional[dict] = None) -> None:
    subprocess.run(
        list(command),
        cwd=str(PROJECT_ROOT),
        env=environment,
        check=True,
    )


def _build_icons(build_root: Path) -> PackageIcons:
    output = build_root / "icons"
    _run(
        (
            sys.executable,
            str(PROJECT_ROOT / "packaging" / "icon_assets.py"),
            "--output",
            str(output),
        )
    )
    icons = PackageIcons(
        windows=output / "easy-multi-provider.ico",
        macos=output / "easy-multi-provider.icns",
        linux=output / "easy-multi-provider-256.png",
    )
    if not all(path.is_file() for path in (icons.windows, icons.macos, icons.linux)):
        raise RuntimeError("native icon generation did not produce every format")
    return icons


def _build_binary(target: Target, build_root: Path, icons: PackageIcons) -> Path:
    work_root = build_root / "pyinstaller-work"
    dist_root = build_root / "pyinstaller-dist"
    _remove_managed_tree(work_root, build_root)
    _remove_managed_tree(dist_root, build_root)
    work_root.mkdir(parents=True, exist_ok=True)
    dist_root.mkdir(parents=True, exist_ok=True)
    environment = os.environ.copy()
    environment["PYINSTALLER_CONFIG_DIR"] = str(build_root / "pyinstaller-cache")
    if target.system == "Windows":
        environment["EMP_PACKAGE_ICON"] = str(icons.windows)
    elif target.system == "Darwin":
        environment["EMP_PACKAGE_ICON"] = str(icons.macos)

    _run(
        (
            sys.executable,
            "-m",
            "PyInstaller",
            "--noconfirm",
            "--clean",
            "--workpath",
            str(work_root),
            "--distpath",
            str(dist_root),
            str(PROJECT_ROOT / "packaging" / "easy_multi_provider.spec"),
        ),
        environment=environment,
    )
    executable = dist_root / (PACKAGE_NAME + target.executable_suffix)
    if not executable.is_file():
        raise RuntimeError("PyInstaller did not produce the expected executable")
    if target.system != "Windows":
        executable.chmod(0o755)
    return executable


def _reserve_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def _terminate_process_tree(process: subprocess.Popen) -> None:
    try:
        parent = psutil.Process(process.pid)
        processes = parent.children(recursive=True) + [parent]
    except (psutil.Error, OSError):
        processes = []
    for item in processes:
        try:
            item.terminate()
        except psutil.Error:
            pass
    _, alive = psutil.wait_procs(processes, timeout=5)
    for item in alive:
        try:
            item.kill()
        except psutil.Error:
            pass
    if process.poll() is None:
        try:
            process.wait(timeout=5)
        except subprocess.TimeoutExpired:
            process.kill()
            process.wait(timeout=5)


def _smoke_executable(executable: Path, version: str, target: Target) -> None:
    version_result = subprocess.run(
        (str(executable), "--version"),
        check=True,
        capture_output=True,
        text=True,
        timeout=15,
    )
    expected = "%s %s" % (PRODUCT_NAME, version)
    if version_result.stdout.strip() != expected:
        raise RuntimeError("packaged version probe returned an unexpected value")

    with tempfile.TemporaryDirectory(prefix="emp-package-smoke-") as temporary:
        temporary_root = Path(temporary)
        codex_home = temporary_root / "codex-home"
        codex_home.mkdir()
        port = _reserve_loopback_port()
        environment = os.environ.copy()
        environment["CODEX_HOME"] = str(codex_home)
        environment["EASY_MULTI_PROVIDER_CONFIG"] = str(
            temporary_root / "config.json"
        )
        environment["EASY_MULTI_PROVIDER_MASTER_KEY"] = ""
        environment["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(
            temporary_root / "state" / "master.key"
        )
        environment["PYTHONUNBUFFERED"] = "1"
        if target.system == "Windows":
            desktop_root = temporary_root / "local-app-data"
            environment["LOCALAPPDATA"] = str(desktop_root)
            desktop_config = desktop_root / "EasyMultiProvider" / "config.json"
            browser = temporary_root / "browser-ok.bat"
            browser.write_text("@exit /b 0\n", encoding="ascii")
        elif target.system == "Darwin":
            desktop_root = temporary_root / "home"
            environment["HOME"] = str(desktop_root)
            desktop_config = (
                desktop_root
                / "Library"
                / "Application Support"
                / "EasyMultiProvider"
                / "config.json"
            )
            browser = temporary_root / "browser-ok.sh"
            browser.write_text("#!/bin/sh\nexit 0\n", encoding="ascii")
            browser.chmod(0o755)
        else:
            desktop_root = temporary_root / "xdg-config"
            environment["XDG_CONFIG_HOME"] = str(desktop_root)
            desktop_config = desktop_root / "easy-multi-provider" / "config.json"
            browser = temporary_root / "browser-ok.sh"
            browser.write_text("#!/bin/sh\nexit 0\n", encoding="ascii")
            browser.chmod(0o755)
        desktop_config.parent.mkdir(parents=True)
        desktop_config.write_text(
            json.dumps({"host": "127.0.0.1", "port": port}),
            encoding="utf-8",
        )
        environment["BROWSER"] = str(browser)
        output_path = temporary_root / "service-output.txt"
        with output_path.open("w", encoding="utf-8") as output_handle:
            process = subprocess.Popen(
                (str(executable),),
                cwd=str(temporary_root),
                env=environment,
                stdout=output_handle,
                stderr=subprocess.STDOUT,
                text=True,
            )
            try:
                deadline = time.monotonic() + 20.0
                last_error: Optional[BaseException] = None
                request_target: Optional[str] = None
                session_cookie: Optional[str] = None
                while time.monotonic() < deadline:
                    if process.poll() is not None:
                        output_handle.flush()
                        output = output_path.read_text(
                            encoding="utf-8", errors="replace"
                        )
                        raise RuntimeError(
                            "packaged service exited during smoke test: %s"
                            % output.strip()
                        )
                    output_handle.flush()
                    output = output_path.read_text(
                        encoding="utf-8", errors="replace"
                    )
                    match = re.search(r"^Open in browser: (\S+)$", output, re.MULTILINE)
                    if match is not None:
                        parsed = urlsplit(match.group(1))
                        request_target = parsed.path or "/"
                        if parsed.query:
                            request_target += "?" + parsed.query
                    if request_target is None:
                        time.sleep(0.1)
                        continue
                    connection = http.client.HTTPConnection(
                        "127.0.0.1", port, timeout=0.5
                    )
                    try:
                        headers = (
                            {"Cookie": session_cookie}
                            if session_cookie is not None
                            else {}
                        )
                        connection.request("GET", request_target, headers=headers)
                        response = connection.getresponse()
                        response.read()
                        if response.status == 200:
                            break
                        if response.status == 303:
                            cookie_header = response.getheader("Set-Cookie", "")
                            session_cookie = cookie_header.split(";", 1)[0].strip()
                            if session_cookie.startswith("emp_session="):
                                request_target = response.getheader("Location", "/")
                                continue
                            session_cookie = None
                        last_error = RuntimeError(
                            "packaged service returned HTTP %s" % response.status
                        )
                    except (OSError, http.client.HTTPException) as exc:
                        last_error = exc
                    finally:
                        connection.close()
                    time.sleep(0.1)
                else:
                    raise RuntimeError(
                        "packaged service did not become ready"
                    ) from last_error
            finally:
                if process.poll() is None:
                    _terminate_process_tree(process)


def _copy_release_files(destination: Path, executable: Path, target: Target) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    binary = destination / (PACKAGE_NAME + target.executable_suffix)
    shutil.copy2(str(executable), str(binary))
    if target.system != "Windows":
        binary.chmod(0o755)
    for name in ("README.md", "README.zh-CN.md", "LICENSE"):
        shutil.copy2(str(PROJECT_ROOT / name), str(destination / name))


def _write_zip(
    output: Path, executable: Path, target: Target, archive_root: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="emp-zip-") as temporary:
        content = Path(temporary) / archive_root
        _copy_release_files(content, executable, target)
        with zipfile.ZipFile(str(output), "w", zipfile.ZIP_DEFLATED) as archive:
            for path in sorted(content.rglob("*")):
                if path.is_file():
                    archive.write(str(path), str(Path(archive_root) / path.relative_to(content)))


def _write_tar(
    output: Path, executable: Path, target: Target, archive_root: str
) -> None:
    with tempfile.TemporaryDirectory(prefix="emp-tar-") as temporary:
        content = Path(temporary) / archive_root
        _copy_release_files(content, executable, target)
        with tarfile.open(str(output), "w:gz") as archive:
            archive.add(str(content), arcname=archive_root)


def _write_deb(
    output: Path,
    executable: Path,
    target: Target,
    version: str,
    build_root: Path,
    icons: PackageIcons,
) -> None:
    dpkg_deb = shutil.which("dpkg-deb")
    if dpkg_deb is None or target.deb_arch is None:
        raise RuntimeError("dpkg-deb is required for the Linux package")
    stage = build_root / "deb-root"
    _remove_managed_tree(stage, build_root)
    binary_dir = stage / "usr" / "bin"
    docs_dir = stage / "usr" / "share" / "doc" / PACKAGE_NAME
    control_dir = stage / "DEBIAN"
    applications_dir = stage / "usr" / "share" / "applications"
    scalable_icon_dir = (
        stage / "usr" / "share" / "icons" / "hicolor" / "scalable" / "apps"
    )
    raster_icon_dir = (
        stage / "usr" / "share" / "icons" / "hicolor" / "256x256" / "apps"
    )
    metadata_dir = stage / "usr" / "share" / "metainfo"
    binary_dir.mkdir(parents=True)
    docs_dir.mkdir(parents=True)
    control_dir.mkdir(parents=True)
    applications_dir.mkdir(parents=True)
    scalable_icon_dir.mkdir(parents=True)
    raster_icon_dir.mkdir(parents=True)
    metadata_dir.mkdir(parents=True)
    binary = binary_dir / PACKAGE_NAME
    shutil.copy2(str(executable), str(binary))
    binary.chmod(0o755)
    shutil.copy2(str(PROJECT_ROOT / "README.md"), str(docs_dir / "README.md"))
    shutil.copy2(
        str(PROJECT_ROOT / "README.zh-CN.md"), str(docs_dir / "README.zh-CN.md")
    )
    shutil.copy2(str(PROJECT_ROOT / "LICENSE"), str(docs_dir / "copyright"))
    shutil.copy2(
        str(PROJECT_ROOT / "assets" / "branding" / "easy-multi-provider-icon.svg"),
        str(scalable_icon_dir / "easy-multi-provider.svg"),
    )
    shutil.copy2(
        str(icons.linux),
        str(raster_icon_dir / "easy-multi-provider.png"),
    )
    (applications_dir / "easy-multi-provider.desktop").write_text(
        "[Desktop Entry]\n"
        "Type=Application\n"
        "Name=EasyMultiProvider\n"
        "Comment=Local multi-provider control plane for Codex\n"
        "Exec=easy-multi-provider\n"
        "TryExec=easy-multi-provider\n"
        "Icon=easy-multi-provider\n"
        "Terminal=true\n"
        "Categories=Development;\n"
        "Keywords=Codex;AI;Model;Router;\n"
        "StartupNotify=true\n",
        encoding="utf-8",
    )
    (metadata_dir / "io.github.Killow1998.EasyMultiProvider.metainfo.xml").write_text(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n"
        "<component type=\"desktop-application\">\n"
        "  <id>io.github.Killow1998.EasyMultiProvider</id>\n"
        "  <name>EasyMultiProvider</name>\n"
        "  <summary>Local multi-provider control plane for Codex</summary>\n"
        "  <description>\n"
        "    <p>Configure Codex subscriptions, API providers, and model routing "
        "from a local browser interface.</p>\n"
        "  </description>\n"
        "  <metadata_license>CC0-1.0</metadata_license>\n"
        "  <project_license>MIT</project_license>\n"
        "  <url type=\"homepage\">"
        "https://github.com/Killow1998/EasyMultiProvider</url>\n"
        "  <launchable type=\"desktop-id\">easy-multi-provider.desktop</launchable>\n"
        "</component>\n",
        encoding="utf-8",
    )
    installed_kib = max(
        1,
        sum(path.stat().st_size for path in stage.rglob("*") if path.is_file())
        // 1024,
    )
    control = (
        "Package: easy-multi-provider\n"
        "Version: %s\n"
        "Section: utils\n"
        "Priority: optional\n"
        "Architecture: %s\n"
        "Maintainer: Killow1998 <Killow1998@users.noreply.github.com>\n"
        "Depends: libc6 (>= 2.35)\n"
        "Installed-Size: %s\n"
        "Description: Local multi-provider control plane for Codex\n"
        " EasyMultiProvider adds subscriptions and external model providers to\n"
        " the native Codex model picker while Codex keeps task ownership.\n"
    ) % (version, target.deb_arch, installed_kib)
    (control_dir / "control").write_text(control, encoding="utf-8")
    _run((dpkg_deb, "--build", "--root-owner-group", str(stage), str(output)))


def _write_macos_app(
    destination: Path,
    executable: Path,
    version: str,
    icons: PackageIcons,
) -> None:
    contents = destination / "Contents"
    executable_dir = contents / "MacOS"
    resources_dir = contents / "Resources"
    executable_dir.mkdir(parents=True)
    resources_dir.mkdir(parents=True)
    shutil.copy2(str(executable), str(resources_dir / PACKAGE_NAME))
    (resources_dir / PACKAGE_NAME).chmod(0o755)
    shutil.copy2(str(icons.macos), str(resources_dir / "easy-multi-provider.icns"))

    launcher = executable_dir / PRODUCT_NAME
    launcher.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        "contents_dir=$(CDPATH= cd \"$(dirname \"$0\")/..\" && pwd)\n"
        "exec /usr/bin/open -a Terminal \"$contents_dir/Resources/launch.command\"\n",
        encoding="utf-8",
    )
    launcher.chmod(0o755)
    terminal_command = resources_dir / "launch.command"
    terminal_command.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        "resources_dir=$(CDPATH= cd \"$(dirname \"$0\")\" && pwd)\n"
        "config_path=\"$HOME/Library/Application Support/EasyMultiProvider/config.json\"\n"
        "exec \"$resources_dir/easy-multi-provider\" serve "
        "--config \"$config_path\" --open-browser\n",
        encoding="utf-8",
    )
    terminal_command.chmod(0o755)

    info = {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleDisplayName": PRODUCT_NAME,
        "CFBundleExecutable": PRODUCT_NAME,
        "CFBundleIconFile": "easy-multi-provider.icns",
        "CFBundleIdentifier": "io.github.Killow1998.EasyMultiProvider",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": PRODUCT_NAME,
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": version,
        "CFBundleVersion": version,
        "NSHighResolutionCapable": True,
    }
    with (contents / "Info.plist").open("wb") as handle:
        plistlib.dump(info, handle, sort_keys=True)


def _write_dmg(
    output: Path,
    executable: Path,
    target: Target,
    version: str,
    build_root: Path,
    icons: PackageIcons,
) -> None:
    hdiutil = shutil.which("hdiutil")
    if hdiutil is None:
        raise RuntimeError("hdiutil is required for the macOS disk image")
    stage = build_root / "dmg-root"
    _remove_managed_tree(stage, build_root)
    stage.mkdir(parents=True)
    _write_macos_app(stage / (PRODUCT_NAME + ".app"), executable, version, icons)
    for name in ("README.md", "README.zh-CN.md", "LICENSE"):
        shutil.copy2(str(PROJECT_ROOT / name), str(stage / name))
    (stage / "Applications").symlink_to("/Applications")
    _run(
        (
            hdiutil,
            "create",
            "-volname",
            PRODUCT_NAME,
            "-srcfolder",
            str(stage),
            "-ov",
            "-format",
            "UDZO",
            str(output),
        )
    )


def _write_checksum(path: Path) -> Path:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            digest.update(chunk)
    checksum = path.with_name(path.name + ".sha256")
    checksum.write_text("%s  %s\n" % (digest.hexdigest(), path.name), encoding="ascii")
    return checksum


def build(skip_service_smoke: bool = False) -> List[Path]:
    target = current_target()
    version = project_version()
    build_parent = PROJECT_ROOT / "build" / "standalone"
    build_root = build_parent / target.identity
    artifacts_root = PROJECT_ROOT / "artifacts"
    _remove_managed_tree(build_root, build_parent)
    build_root.mkdir(parents=True)
    artifacts_root.mkdir(parents=True, exist_ok=True)

    artifact_base = "%s-%s-%s" % (PACKAGE_NAME, version, target.identity)
    for stale in artifacts_root.glob(artifact_base + "*"):
        if stale.is_file():
            stale.unlink()

    icons = _build_icons(build_root)
    executable = _build_binary(target, build_root, icons)
    if skip_service_smoke:
        version_result = subprocess.run(
            (str(executable), "--version"),
            check=True,
            capture_output=True,
            text=True,
            timeout=15,
        )
        if version_result.stdout.strip() != "%s %s" % (PRODUCT_NAME, version):
            raise RuntimeError("packaged version probe returned an unexpected value")
    else:
        _smoke_executable(executable, version, target)

    raw_executable = artifacts_root / (artifact_base + target.executable_suffix)
    shutil.copy2(str(executable), str(raw_executable))
    if target.system != "Windows":
        raw_executable.chmod(0o755)
    artifacts: List[Path] = [raw_executable]

    if target.system == "Windows":
        archive = artifacts_root / (artifact_base + ".zip")
        _write_zip(archive, executable, target, artifact_base)
        artifacts.append(archive)
    else:
        archive = artifacts_root / (artifact_base + ".tar.gz")
        _write_tar(archive, executable, target, artifact_base)
        artifacts.append(archive)
    if target.system == "Linux":
        deb = artifacts_root / (artifact_base + ".deb")
        _write_deb(deb, executable, target, version, build_root, icons)
        artifacts.append(deb)
    if target.system == "Darwin":
        dmg = artifacts_root / (artifact_base + ".dmg")
        _write_dmg(dmg, executable, target, version, build_root, icons)
        artifacts.append(dmg)

    checksums = [_write_checksum(path) for path in artifacts]
    return artifacts + checksums


def main(argv: Optional[Sequence[str]] = None) -> int:
    parser = argparse.ArgumentParser(
        description="Build one native EasyMultiProvider distribution"
    )
    parser.add_argument(
        "--skip-service-smoke",
        action="store_true",
        help="only verify the packaged --version command",
    )
    args = parser.parse_args(argv)
    try:
        artifacts = build(skip_service_smoke=args.skip_service_smoke)
    except Exception as exc:
        if os.environ.get("GITHUB_ACTIONS") == "true":
            message = "%s: %s" % (type(exc).__name__, exc)
            message = (
                message.replace("%", "%25")
                .replace("\r", "%0D")
                .replace("\n", "%0A")
            )
            print(
                "::error title=Native package build failed::%s" % message,
                file=sys.stderr,
            )
        raise
    for artifact in artifacts:
        print(artifact.relative_to(PROJECT_ROOT))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
