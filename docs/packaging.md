# Native packaging

EMP uses one PyInstaller definition and builds separately on each target
operating system. Cross-compilation is intentionally unsupported.

## Targets

| Runner | Target | Outputs |
| --- | --- | --- |
| `windows-2025` | Windows x64 | branded `.exe`, `.zip` |
| `ubuntu-22.04` | Linux x64 | binary, `.tar.gz`, desktop-enabled `.deb` |
| `macos-15-intel` | macOS Intel | binary, `.tar.gz`, `.app` in `.dmg` |
| `macos-15` | macOS Apple Silicon | binary, `.tar.gz`, `.app` in `.dmg` |

Linux artifacts target Ubuntu 22.04 or a compatible distribution with glibc
2.35 or newer. macOS artifacts are architecture-specific; they are not
universal binaries.

## Local build

Install the locked build group and run the builder on the target platform:

```bash
uv sync --frozen --group package
uv run --frozen --group package python packaging/build.py
```

Artifacts and their `.sha256` sidecars are written under `artifacts/`. The
builder performs two checks before packaging:

1. the frozen executable reports the source version through `--version`;
2. the frozen service starts from a temporary config and Codex home and serves
   its Web UI over loopback.

The smoke test does not enable Codex integration and does not use Provider or
subscription credentials.

## Desktop package boundary

`assets/branding/easy-multi-provider-icon.svg` is the editable artwork and its
committed 1024-pixel RGBA rendering is the raster master. The packaging-only
icon generator creates a multi-size Windows ICO, a macOS ICNS, and a 256-pixel
Linux PNG inside the managed build directory. These generated derivatives are
never runtime dependencies.

Desktop launch is intentionally foreground-only:

- the Windows console executable starts desktop mode when opened without
  arguments;
- the Debian package installs a `Terminal=true` desktop entry plus scalable and
  raster icons;
- the DMG contains `EasyMultiProvider.app`, whose launcher opens the bundled
  command in Terminal.

All three forms open the authenticated browser URL only after the listener is
ready. The owning terminal is the process indicator and stop control: use
`Ctrl+C` for a clean shutdown. No daemon, tray process, PID file, or second
service lifecycle is introduced.

## Runtime boundary

The package contains EMP, its Python dependencies, and platform launcher/icon
metadata. It does not contain Codex CLI, credentials, configuration, model
catalogs, or generated state. The host must provide a working `codex` command.

## GitHub Actions and Releases

Run the **Package** workflow manually from GitHub Actions:

- leave `release_tag` blank to build and retain the four platform bundles as
  workflow artifacts for 14 days;
- on the `main` branch, enter the exact source tag, such as `v0.9.0`, to wait
  for all four builds, merge their outputs, verify the complete 22-file
  manifest and every SHA-256 sidecar, and create a GitHub Draft Pre-release.

The requested tag must equal `v` plus the version in both `pyproject.toml` and
`easy_multi_provider/__init__.py`. The release job fails closed on a version
mismatch, a missing or unexpected asset, an invalid checksum, or an existing
tag that targets a different commit. The build jobs retain read-only repository
access; only the gated release job receives `contents: write`.

The release remains private as a draft until a maintainer reviews the generated
notes and assets and clicks **Publish release** on GitHub. The workflow creates
the tag at the exact commit it built; no local `git tag` command is needed.

Current macOS outputs are unsigned development artifacts. Keep their release
marked as a pre-release until Developer ID signing and notarization are
configured.
