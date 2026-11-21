import argparse
import hashlib
import json
import platform
import shutil
import subprocess
import sys
import sysconfig
import tempfile
import time
from importlib.metadata import distribution
from pathlib import Path

from worker import VERSION


def build(output):
    root = Path(__file__).resolve().parent
    operating_system = {"linux": "linux", "darwin": "macos", "win32": "windows"}[
        sys.platform
    ]
    architecture = {"amd64": "x86_64", "arm64": "aarch64"}.get(
        platform.machine().lower(), platform.machine().lower()
    )
    name = f"EMP-mod-aero-{operating_system}-{architecture}"
    output = output.resolve()
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="aero-build-") as temporary:
        scratch = Path(temporary)
        licenses = scratch / "licenses"
        licenses.mkdir()
        python_license = next(
            path
            for path in [
                Path(sysconfig.get_path("stdlib")) / "LICENSE.txt",
                Path(sys.base_prefix) / "LICENSE.txt",
            ]
            if path.is_file()
        )
        shutil.copyfile(python_license, licenses / "PYTHON-LICENSE.txt")
        (licenses / "PYINSTALLER-LICENSE.txt").write_text(
            distribution("pyinstaller").read_text("licenses/COPYING.txt"),
            encoding="utf-8",
        )
        subprocess.run(
            [
                sys.executable,
                "-m",
                "PyInstaller",
                "--noconfirm",
                "--onefile",
                "--exclude-module",
                "readline",
                "--exclude-module",
                "curses",
                "--exclude-module",
                "tkinter",
                "--name",
                name,
                "--distpath",
                str(output),
                "--workpath",
                str(scratch / "work"),
                "--specpath",
                str(scratch),
                "--add-data",
                str(root / "web") + ":web",
                "--add-data",
                str(licenses) + ":licenses",
                str(root / "worker.py"),
            ],
            check=True,
        )
    asset = output / (name + (".exe" if operating_system == "windows" else ""))
    started = time.monotonic()
    result = subprocess.run(
        [str(asset)],
        input=b"",
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=5,
        check=True,
    )
    hello = json.loads(result.stdout)
    assert hello == {
        "module": "aero",
        "version": VERSION,
        "module_version": VERSION,
        "protocol": 1,
        "host_api": 2,
    }, hello
    checksum = asset.with_name(asset.name + ".sha256")
    checksum.write_text(
        hashlib.sha256(asset.read_bytes()).hexdigest() + "  " + asset.name + "\n",
        encoding="utf-8",
    )
    print(
        json.dumps(
            {
                "asset": str(asset),
                "checksum": str(checksum),
                "version": VERSION,
                "cold_start_and_exit_seconds": round(time.monotonic() - started, 3),
            }
        )
    )


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--output", type=Path, default=Path(__file__).resolve().parent / "artifacts"
    )
    build(parser.parse_args().output)
