# Native Rust packaging

Each release runner builds `EMP` from the Rust workspace. The shipped service
does not need any other runtime. Cross
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

Install Rust 1.93.1, then run on the target OS:

```bash
cargo xtask package         # build, service smoke, archives and checksums
cargo xtask package-smoke   # update/rollback (and Linux install) smoke tests
```

`cargo xtask package` runs `cargo build --locked --release -p emp-app --bin EMP`,
checks that `EMP --version` matches the Cargo workspace version, and starts the
built service from an isolated configuration. It checks `/healthz`, completes
the bootstrap login, and compares the served Web UI bytes with
`crates/emp-app/web/index.html`. It then assembles the established archive
layout, desktop metadata, platform icons, and SHA-256 sidecars under
`artifacts/`. The Windows and macOS icons are generated once from
`assets/branding/easy-multi-provider-icon-1024.png` and committed under
`assets/branding/native/`. On Windows, `rc.exe` compiles the icon and
Cargo-derived file/product versions into the executable, and the build reads
the final PE resources back to verify them.

`cargo xtask package-smoke` hands a synthetic installation to the packaged
`--emp-apply-update` worker twice: once to replace it and once with a broken
candidate that must roll back. On macOS it installs from the real `.dmg`. On
Linux it also installs the archive with `install-user.sh` into an isolated
desktop-user home. Neither the repository nor any shipped script uses Python.
The Linux bootstrap installer (`EMP-linux-x86_64-install.sh`) needs only
`curl`, `tar`, `gzip` and `sha256sum`: it resolves the latest stable tag from
the `releases/latest` redirect, verifies the archive against the published
`EMP-linux-x86_64.tar.gz.sha256`, checks every archive entry, and lets the
verified binary migrate an older configuration with
`EMP --emp-migrate-config SOURCE TARGET`.

The Rust Web UI embeds the existing HTML at compile time. Linux tarballs retain
the updater-compatible `EMP/EMP` path and also include `EMP/install-user.sh`,
the SVG icon, and documentation. The Windows archive keeps the `EMP/` root;
macOS disk images contain `EMP.app` and an `Applications` link. macOS packaging
uses the native `hdiutil` tool. No archive layout or release asset names are
changed by the language rewrite.

The isolated package smoke does not enable Codex integration or use provider or
subscription credentials. The package workflow
also runs the Rust transport test that rejects untrusted and wrong-host TLS
certificates while accepting a configured root.

## Release workflow

The **Package** workflow builds on all four native runners, merges the outputs,
checks the exact 22-file manifest and each SHA-256 sidecar with
`cargo xtask validate-release`, and publishes six
user-facing assets:

- `EMP.exe`
- `EMP-linux-x86_64.tar.gz`
- `EMP-linux-x86_64.tar.gz.sha256` (read by the Linux installer)
- `EMP-linux-x86_64-install.sh`
- `EMP-macos-x86_64.dmg`
- `EMP-macos-arm64.dmg`

The release tag must be `v` plus the Cargo workspace version. The workflow
rejects missing or unexpected artifacts, bad checksums, and an existing tag
that targets another commit. macOS downloads remain unsigned until Developer
ID signing and notarization are configured.
