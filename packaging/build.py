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
import tomllib
from urllib.parse import urlsplit
import zipfile
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional, Sequence

PROJECT_ROOT = Path(__file__).resolve().parents[1]
PRODUCT_NAME = "EMP"
EXECUTABLE_NAME = "EMP"
ARTIFACT_NAME = "EMP"


@dataclass(frozen=True)
class Target:
    system: str
    os_id: str
    arch: str
    executable_suffix: str

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
        return Target(system, "linux", arch, "")
    if system == "Darwin" and arch in ("x86_64", "arm64"):
        return Target(system, "macos", arch, "")
    raise RuntimeError("unsupported packaging target: %s/%s" % (system, arch))


def project_version() -> str:
    workspace = tomllib.loads((PROJECT_ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    version = workspace.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(version, str) or not version:
        raise RuntimeError("Cargo workspace package version is unavailable")
    return version


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


def _build_binary(
    target: Target, windows_resource_file: Optional[Path] = None
) -> Path:
    cargo = os.environ.get("CARGO") or shutil.which("cargo")
    if not cargo:
        raise RuntimeError("Rust Cargo is required to build EMP")
    cargo_command = [cargo, "+1.93.1"]
    if os.environ.get("CARGO_NET_OFFLINE", "").lower() in {"1", "true", "yes"}:
        cargo_command.append("--offline")
    cargo_command.extend(
        [
            "build",
            "--locked",
            "--release",
            "--package",
            "emp-app",
            "--bin",
            EXECUTABLE_NAME,
        ]
    )
    environment = os.environ.copy()
    if target.system == "Windows":
        if windows_resource_file is None or not windows_resource_file.is_file():
            raise RuntimeError("Windows packaging requires compiled icon/version resources")
        environment["EMP_PACKAGE_RESOURCE"] = str(windows_resource_file.resolve())
        environment["EMP_REQUIRE_WINDOWS_RESOURCES"] = "1"
    _run(cargo_command, environment)
    target_root = Path(os.environ.get("CARGO_TARGET_DIR", PROJECT_ROOT / "target"))
    if not target_root.is_absolute():
        target_root = PROJECT_ROOT / target_root
    executable = target_root / "release" / (EXECUTABLE_NAME + target.executable_suffix)
    if not executable.is_file():
        raise RuntimeError("Cargo did not produce the expected EMP executable")
    if target.system != "Windows":
        executable.chmod(0o755)
    return executable


def _write_windows_resource_script(
    destination: Path, icon: Path, version: str
) -> Path:
    if not icon.is_file():
        raise RuntimeError("Windows application icon is missing")
    match = re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version)
    if match is None:
        raise RuntimeError("Windows file resources require a stable three-part version")
    components = tuple(int(component) for component in match.groups())
    if any(component > 65535 for component in components):
        raise RuntimeError("Windows file version components must fit 16 bits")

    destination.mkdir(parents=True, exist_ok=True)
    shutil.copyfile(icon, destination / "easy-multi-provider.ico")
    file_version = ".".join(str(component) for component in components)
    numeric_version = ",".join(str(component) for component in components) + ",0"
    source = "\n".join(
        (
            "#define VS_VERSION_INFO 1",
            '1 ICON "easy-multi-provider.ico"',
            "",
            "VS_VERSION_INFO VERSIONINFO",
            " FILEVERSION %s" % numeric_version,
            " PRODUCTVERSION %s" % numeric_version,
            " FILEFLAGSMASK 0x3fL",
            " FILEFLAGS 0x0L",
            " FILEOS 0x00040004L",
            " FILETYPE 0x00000001L",
            " FILESUBTYPE 0x00000000L",
            "BEGIN",
            ' BLOCK "StringFileInfo"',
            " BEGIN",
            '  BLOCK "040904b0"',
            "  BEGIN",
            '   VALUE "CompanyName", "Killow1998"',
            '   VALUE "FileDescription", "EMP local multi-provider control plane"',
            '   VALUE "FileVersion", "%s"' % file_version,
            '   VALUE "InternalName", "EMP"',
            '   VALUE "OriginalFilename", "EMP.exe"',
            '   VALUE "ProductName", "EasyMultiProvider"',
            '   VALUE "ProductVersion", "%s"' % file_version,
            "  END",
            " END",
            ' BLOCK "VarFileInfo"',
            " BEGIN",
            '  VALUE "Translation", 0x0409, 1200',
            " END",
            "END",
        )
    ) + "\n"
    resource_script = destination / "EMP.rc"
    resource_script.write_text(source, encoding="ascii", newline="\n")
    return resource_script


def _find_windows_resource_compiler() -> Path:
    configured = os.environ.get("EMP_RC")
    if configured:
        compiler = Path(configured)
        if compiler.is_file():
            return compiler.resolve()
        raise RuntimeError("EMP_RC does not point to an existing rc.exe")

    candidates = []
    sdk_bin = os.environ.get("WindowsSdkBinPath")
    if sdk_bin:
        base = Path(sdk_bin)
        candidates.extend(base / architecture / "rc.exe" for architecture in ("x64", "x86"))

    sdk_dir = os.environ.get("WindowsSdkDir")
    sdk_version = os.environ.get("WindowsSDKVersion", "").rstrip("\\/")
    if sdk_dir:
        bin_dir = Path(sdk_dir) / "bin"
        if sdk_version:
            candidates.extend(
                bin_dir / sdk_version / architecture / "rc.exe"
                for architecture in ("x64", "x86")
            )
        candidates.extend(_versioned_resource_compilers(bin_dir))

    program_files = os.environ.get("ProgramFiles(x86)")
    if program_files:
        candidates.extend(
            _versioned_resource_compilers(Path(program_files) / "Windows Kits" / "10" / "bin")
        )

    path_compiler = shutil.which("rc.exe")
    if path_compiler:
        candidates.append(Path(path_compiler))
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    raise RuntimeError("Windows SDK rc.exe was not found")


def _versioned_resource_compilers(bin_directory: Path) -> List[Path]:
    try:
        versions = [path for path in bin_directory.iterdir() if path.is_dir()]
    except OSError:
        return []

    def version_key(path: Path) -> tuple:
        values = re.findall(r"\d+", path.name)
        return tuple(int(value) for value in values)

    candidates = []
    for version in sorted(versions, key=version_key, reverse=True):
        candidates.extend(
            version / architecture / "rc.exe" for architecture in ("x64", "x86")
        )
    return candidates


def _compile_windows_resource(resource_script: Path) -> Path:
    if not resource_script.is_file():
        raise RuntimeError("Windows resource script is missing")
    if not (resource_script.parent / "easy-multi-provider.ico").is_file():
        raise RuntimeError("Windows application icon is missing beside its resource script")
    compiler = _find_windows_resource_compiler()
    resource_file = resource_script.with_suffix(".res")
    try:
        result = subprocess.run(
            [str(compiler), "/nologo", "/fo", str(resource_file), resource_script.name],
            cwd=str(resource_script.parent),
            capture_output=True,
            text=True,
            timeout=60,
            check=False,
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        raise RuntimeError("Windows resource compiler could not run") from exc
    if result.returncode != 0:
        details = (result.stderr or result.stdout).strip()
        raise RuntimeError("Windows resource compilation failed: %s" % details)
    if not resource_file.is_file() or resource_file.stat().st_size == 0:
        raise RuntimeError("Windows resource compiler did not create a nonempty .res file")
    return resource_file


def _validate_windows_resources(executable: Path, version: str) -> None:
    try:
        import pefile
    except ImportError as exc:
        raise RuntimeError("pefile is required to verify Windows package resources") from exc

    match = re.fullmatch(r"(0|[1-9]\d*)\.(0|[1-9]\d*)\.(0|[1-9]\d*)", version)
    if match is None:
        raise RuntimeError("Windows package version is not a stable three-part version")
    major, minor, patch = (int(component) for component in match.groups())
    try:
        pe = pefile.PE(str(executable))
    except (OSError, pefile.PEFormatError) as exc:
        raise RuntimeError("Windows package is not a readable PE executable") from exc

    try:
        if pe.FILE_HEADER.Machine != 0x8664:
            raise RuntimeError("Windows package is not an x86_64 executable")
        resource_directory = getattr(pe, "DIRECTORY_ENTRY_RESOURCE", None)
        resource_types = {
            entry.id for entry in getattr(resource_directory, "entries", [])
        }
        required_types = {3, 14, 16}  # RT_ICON, RT_GROUP_ICON, RT_VERSION
        if not required_types.issubset(resource_types):
            raise RuntimeError("Windows package is missing icon or version resources")

        expected_ms = (major << 16) | minor
        expected_ls = patch << 16
        fixed_versions = getattr(pe, "VS_FIXEDFILEINFO", [])
        if not fixed_versions:
            raise RuntimeError("Windows package has no fixed version resource")
        fixed = fixed_versions[0]
        actual_versions = (
            fixed.FileVersionMS,
            fixed.FileVersionLS,
            fixed.ProductVersionMS,
            fixed.ProductVersionLS,
        )
        if actual_versions != (expected_ms, expected_ls, expected_ms, expected_ls):
            raise RuntimeError("Windows package fixed version does not match source")

        strings = {}
        for file_info in getattr(pe, "FileInfo", []) or []:
            for block in file_info if isinstance(file_info, (list, tuple)) else [file_info]:
                for table in getattr(block, "StringTable", []) or []:
                    for key, value in table.entries.items():
                        if isinstance(key, bytes):
                            key = key.decode("ascii", errors="replace")
                        if isinstance(value, bytes):
                            encoding = (
                                "utf-16le"
                                if len(value) % 2 == 0 and not any(value[1::2])
                                else "utf-8"
                            )
                            value = value.decode(encoding, errors="replace")
                        strings[key] = value.rstrip("\x00")
        expected_string = "%s.%s.%s" % (major, minor, patch)
        if strings.get("FileVersion") != expected_string:
            raise RuntimeError("Windows package FileVersion string does not match source")
        if strings.get("ProductVersion") != expected_string:
            raise RuntimeError("Windows package ProductVersion string does not match source")
    finally:
        pe.close()


def _reserve_loopback_port() -> int:
    with socket.socket(socket.AF_INET, socket.SOCK_STREAM) as probe:
        probe.bind(("127.0.0.1", 0))
        return int(probe.getsockname()[1])


def _terminate_process(process: subprocess.Popen) -> None:
    if process.poll() is not None:
        return
    process.terminate()
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

    temporary_parent = Path(tempfile.gettempdir()).resolve()
    with tempfile.TemporaryDirectory(
        prefix="emp-package-smoke-", dir=str(temporary_parent)
    ) as temporary:
        temporary_root = Path(temporary)
        codex_home = temporary_root / "codex-home"
        codex_home.mkdir()
        port = _reserve_loopback_port()
        environment = os.environ.copy()
        environment["CODEX_HOME"] = str(codex_home)
        config_path = temporary_root / "config.json"
        environment.pop("EMP_UPDATE_READY", None)
        environment.pop("EMP_UPDATE_RESULT", None)
        environment["EASY_MULTI_PROVIDER_MASTER_KEY"] = ""
        environment["EASY_MULTI_PROVIDER_MASTER_KEY_FILE"] = str(
            temporary_root / "state" / "master.key"
        )
        if target.system == "Windows":
            desktop_root = temporary_root / "local-app-data"
            environment["LOCALAPPDATA"] = str(desktop_root)
        elif target.system == "Darwin":
            desktop_root = temporary_root / "home"
            environment["HOME"] = str(desktop_root)
        else:
            desktop_root = temporary_root / "xdg-config"
            environment["XDG_CONFIG_HOME"] = str(desktop_root)
        config_path.write_text(
            json.dumps({"host": "127.0.0.1", "port": port}),
            encoding="utf-8",
        )
        output_path = temporary_root / "service-output.txt"
        with output_path.open("w", encoding="utf-8") as output_handle:
            process = subprocess.Popen(
                (
                    str(executable),
                    "serve",
                    "--config",
                    str(config_path),
                    "--port",
                    str(port),
                ),
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
                    connection = http.client.HTTPConnection("127.0.0.1", port, timeout=0.5)
                    try:
                        connection.request("GET", "/healthz")
                        response = connection.getresponse()
                        response.read()
                        if response.status == 200:
                            connection.close()
                            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                            connection.request("GET", request_target)
                            response = connection.getresponse()
                            response.read()
                            session_cookie = response.getheader("Set-Cookie", "").split(";", 1)[0]
                            if response.status != 303 or not session_cookie.startswith("emp_session="):
                                raise RuntimeError("packaged service bootstrap did not establish a session")
                            connection.close()
                            connection = http.client.HTTPConnection("127.0.0.1", port, timeout=1)
                            connection.request("GET", "/", headers={"Cookie": session_cookie})
                            response = connection.getresponse()
                            body = response.read()
                            if response.status != 200:
                                raise RuntimeError("packaged UI returned HTTP %s" % response.status)
                            expected_ui = (PROJECT_ROOT / "easy_multi_provider" / "web" / "index.html").read_bytes()
                            if body != expected_ui:
                                raise RuntimeError("packaged service did not serve the source Web UI bytes")
                            break
                        last_error = RuntimeError(
                            "packaged health check returned HTTP %s" % response.status
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
                    _terminate_process(process)


def _copy_release_files(destination: Path, executable: Path, target: Target) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    binary = destination / (EXECUTABLE_NAME + target.executable_suffix)
    shutil.copy2(str(executable), str(binary))
    if target.system != "Windows":
        binary.chmod(0o755)
    for name in (
        "README.md",
        "README.zh-CN.md",
        "LICENSE",
        "THIRD_PARTY_NOTICES.md",
    ):
        shutil.copy2(str(PROJECT_ROOT / name), str(destination / name))
    if target.system == "Linux":
        installer = destination / "install-user.sh"
        installer.write_bytes((PROJECT_ROOT / "packaging" / "install-user.sh").read_bytes().replace(b"\r\n", b"\n"))
        installer.chmod(0o755)
        shutil.copy2(
            PROJECT_ROOT / "assets" / "branding" / "easy-multi-provider-icon.svg",
            destination / "easy-multi-provider.svg",
        )


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


def _validate_linux_archive(archive_path: Path, executable: Path) -> None:
    expected_files = {
        "EMP/EMP",
        "EMP/install-user.sh",
        "EMP/easy-multi-provider.svg",
        "EMP/README.md",
        "EMP/README.zh-CN.md",
        "EMP/LICENSE",
        "EMP/THIRD_PARTY_NOTICES.md",
    }
    expected_members = expected_files | {"EMP"}
    with tarfile.open(str(archive_path), "r:gz") as archive:
        members = archive.getmembers()
    names = [member.name.rstrip("/") for member in members]
    if len(names) != len(set(names)) or set(names) != expected_members:
        raise RuntimeError("Linux archive layout differs from the updater contract")
    for member in members:
        path = Path(member.name)
        if path.is_absolute() or ".." in path.parts:
            raise RuntimeError("Linux archive contains an unsafe path")
        if member.name.rstrip("/") == "EMP":
            if not member.isdir():
                raise RuntimeError("Linux archive root must be a directory")
        elif not member.isfile():
            raise RuntimeError("Linux archive may contain only regular files and its root")
        if member.name.endswith((".py", ".pyc")) or "python" in path.name.lower():
            raise RuntimeError("Linux archive must not contain a Python runtime")
    executable_member = next(
        member for member in members if member.name.rstrip("/") == "EMP/EMP"
    )
    if not executable_member.mode & 0o111:
        raise RuntimeError("Linux EMP executable is not marked executable")
    with tarfile.open(str(archive_path), "r:gz") as archive:
        bundled_binary = archive.extractfile("EMP/EMP")
        if bundled_binary is None or bundled_binary.read() != executable.read_bytes():
            raise RuntimeError("Linux archive does not contain the Cargo-built EMP binary")


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
    shutil.copy2(str(executable), str(resources_dir / EXECUTABLE_NAME))
    (resources_dir / EXECUTABLE_NAME).chmod(0o755)
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
        "exec \"$resources_dir/EMP\" serve "
        "--config \"$config_path\" --open-browser\n",
        encoding="utf-8",
    )
    terminal_command.chmod(0o755)

    bundle_version = re.match(r"\d+\.\d+\.\d+", version).group()
    info = {
        "CFBundleDevelopmentRegion": "en",
        "CFBundleDisplayName": PRODUCT_NAME,
        "CFBundleExecutable": PRODUCT_NAME,
        "CFBundleIconFile": "easy-multi-provider.icns",
        "CFBundleIdentifier": "io.github.Killow1998.EasyMultiProvider",
        "CFBundleInfoDictionaryVersion": "6.0",
        "CFBundleName": PRODUCT_NAME,
        "CFBundlePackageType": "APPL",
        "CFBundleShortVersionString": bundle_version,
        "CFBundleVersion": bundle_version,
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
    for name in (
        "README.md",
        "README.zh-CN.md",
        "LICENSE",
        "THIRD_PARTY_NOTICES.md",
    ):
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

    artifact_base = (
        ARTIFACT_NAME
        if target.system == "Windows"
        else "%s-%s" % (ARTIFACT_NAME, target.identity)
    )
    expected_suffixes = [target.executable_suffix]
    if target.system == "Windows":
        expected_suffixes.append(".zip")
    else:
        expected_suffixes.append(".tar.gz")
    if target.system == "Linux":
        expected_suffixes.extend(("-install.sh", ".deb"))
    if target.system == "Darwin":
        expected_suffixes.append(".dmg")
    for suffix in expected_suffixes:
        for stale in (
            artifacts_root / (artifact_base + suffix),
            artifacts_root / (artifact_base + suffix + ".sha256"),
        ):
            if stale.is_file():
                stale.unlink()

    icons = _build_icons(build_root)
    windows_resource_file = None
    if target.system == "Windows":
        windows_resource_script = _write_windows_resource_script(
            build_root / "windows-resources", icons.windows, version
        )
        windows_resource_file = _compile_windows_resource(windows_resource_script)
    executable = _build_binary(target, windows_resource_file)
    if target.system == "Windows":
        _validate_windows_resources(executable, version)
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
        _write_zip(archive, executable, target, PRODUCT_NAME)
        artifacts.append(archive)
    else:
        archive = artifacts_root / (artifact_base + ".tar.gz")
        _write_tar(archive, executable, target, PRODUCT_NAME)
        if target.system == "Linux":
            _validate_linux_archive(archive, executable)
        artifacts.append(archive)
    if target.system == "Linux":
        installer = artifacts_root / (artifact_base + "-install.sh")
        shutil.copy2(PROJECT_ROOT / "packaging" / "install-linux.sh", installer)
        installer.chmod(0o755)
        artifacts.append(installer)
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
