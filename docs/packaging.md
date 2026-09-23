# Native Rust packaging

Each release runner builds `EMP` from the Rust workspace. PyInstaller is not
used, and the shipped service does not need a Python installation. Cross
compilation is intentionally unsupported: every binary and desktop bundle is
built on its native operating system.

| Runner | Target | Artifacts |
| --- | --- | --- |
| `windows-2025` | Windows x64 | `EMP.exe`, `EMP.zip` |
| `ubuntu-22.04` | Linux x86_64 | executable, `.tar.gz`, interactive installer `.sh` |
| `macos-15-intel` | macOS Intel | executable, `.tar.gz`, `.app` in `.dmg` |
| `macos-15` | macOS Apple Silicon | executable, `.tar.gz`, `.app` in `.dmg` |

Linux binaries target Ubuntu 22.04 or a compatible distribution with glibc 2.35
or newer. macOS artifacts are architecture-specific, unsigned development
builds; they are not universal binaries.

## Local build

Install Rust 1.93.1 and Python 3.11 with `uv`, then run on the target OS:

```bash
uv sync --frozen --group package
CARGO_TARGET_DIR=/tmp/emp-luna-target uv run --frozen --group package python packaging/build.py
```

The builder runs `cargo build --locked --release -p emp-app --bin EMP`, checks
that `EMP --version` matches the Cargo workspace version, and starts the built
service from an isolated configuration. It checks `/healthz`, completes the
bootstrap login, and compares the served Web UI bytes with
`easy_multi_provider/web/index.html`. It then assembles the established archive
layout, desktop metadata, platform icons, and SHA-256 sidecars under `artifacts/`.
On Windows, `rc.exe` compiles the generated icon and Cargo-derived file/product
versions into the executable; the package build and Windows-only test inspect
the final PE resources with `pefile`. That Windows path is defined here but has
not been validated on a local Windows runner.

The Python environment is used only by the build and smoke-test scripts for
artifact assembly, icon conversion, and process cleanup. It is not copied into
the executable. The Linux bootstrap installer retains the Python 0.11.10
interactive contract and therefore requires `curl`, Python 3, `tar`, and
`sha256sum`; the installed EMP service itself has no Python runtime dependency.

The Rust Web UI embeds the existing HTML at compile time. Linux tarballs retain
the updater-compatible `EMP/EMP` path and also include `EMP/install-user.sh`,
the SVG icon, and documentation. The Windows archive keeps the `EMP/` root;
macOS disk images contain `EMP.app` and an `Applications` link. macOS packaging
uses the native `hdiutil` tool. No archive layout or release asset names are
changed by the language rewrite.

The isolated package smoke does not enable Codex integration or use provider or
subscription credentials. The Linux update/rollback tests also exercise the
Rust executable through the Python 0.11.10 updater contract. The package workflow
also runs the Rust transport test that rejects untrusted and wrong-host TLS
certificates while accepting a configured root.

## Release workflow

The **Package** workflow builds on all four native runners, merges the outputs,
checks the exact 22-file manifest and each SHA-256 sidecar, and publishes five
user-facing assets:

- `EMP.exe`
- `EMP-linux-x86_64.tar.gz`
- `EMP-linux-x86_64-install.sh`
- `EMP-macos-x86_64.dmg`
- `EMP-macos-arm64.dmg`

The release tag must be `v` plus the Cargo workspace version. The workflow
rejects missing or unexpected artifacts, bad checksums, and an existing tag
that targets another commit. macOS downloads remain unsigned until Developer
ID signing and notarization are configured.
