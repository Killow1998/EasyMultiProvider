# -*- mode: python ; coding: utf-8 -*-

import os
import sys
from pathlib import Path

sys.path.insert(0, SPECPATH)
from windows_tls import collect_windows_tls_binaries


PROJECT_ROOT = Path(SPECPATH).parent
PACKAGE_ROOT = PROJECT_ROOT / "easy_multi_provider"
PACKAGE_ICON = os.environ.get("EMP_PACKAGE_ICON") or None

analysis = Analysis(
    [str(PROJECT_ROOT / "packaging" / "entrypoint.py")],
    pathex=[str(PROJECT_ROOT)],
    binaries=collect_windows_tls_binaries(),
    datas=[
        (str(PACKAGE_ROOT / "web" / "index.html"), "easy_multi_provider/web"),
        (
            str(PACKAGE_ROOT / "data" / "official_models.json"),
            "easy_multi_provider/data",
        ),
    ],
    hiddenimports=[],
    hookspath=[],
    hooksconfig={},
    runtime_hooks=[],
    excludes=[],
    noarchive=False,
    optimize=0,
)

pyz = PYZ(analysis.pure)

executable = EXE(
    pyz,
    analysis.scripts,
    analysis.binaries,
    analysis.datas,
    [],
    name="EMP",
    debug=False,
    bootloader_ignore_signals=False,
    strip=False,
    upx=False,
    console=True,
    disable_windowed_traceback=False,
    argv_emulation=False,
    target_arch=None,
    codesign_identity=None,
    entitlements_file=None,
    icon=PACKAGE_ICON,
)
