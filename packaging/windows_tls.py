"""Pin Windows OpenSSL imports to the DLLs used by the build interpreter."""

import sys
from pathlib import Path


def collect_windows_tls_binaries():
    if sys.platform != "win32":
        return []

    import _ssl
    import ctypes
    from ctypes import wintypes
    from PyInstaller.depend.bindepend import get_imports

    # Dependency scanning can resolve names from PATH differently from Python's
    # DLL loader, notably in Conda/venv builds. Use import names only; obtain the
    # actual locations from modules already loaded by _ssl.
    names = sorted(
        name for name, _ in get_imports(_ssl.__file__)
        if name.lower().startswith(("libssl", "libcrypto"))
    )
    kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
    kernel32.GetModuleHandleW.argtypes = [wintypes.LPCWSTR]
    kernel32.GetModuleHandleW.restype = wintypes.HMODULE
    kernel32.GetModuleFileNameW.argtypes = [
        wintypes.HMODULE, wintypes.LPWSTR, wintypes.DWORD
    ]
    kernel32.GetModuleFileNameW.restype = wintypes.DWORD
    binaries = []
    for name in names:
        handle = kernel32.GetModuleHandleW(name)
        if not handle:
            raise RuntimeError("Python's TLS dependency is not loaded: %s" % name)
        path = ctypes.create_unicode_buffer(32768)
        length = kernel32.GetModuleFileNameW(handle, path, len(path))
        if not length or length >= len(path) or not Path(path.value).is_file():
            raise RuntimeError("Cannot resolve Python's TLS dependency: %s" % name)
        binaries.append((path.value, "."))
    return binaries
