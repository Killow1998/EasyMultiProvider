# Background updates

The management page has two separate controls: **Check updates** and a link to
the repository. Checking or installing requires the authenticated local management
session. The browser never fetches or runs release code itself.

EMP checks the official repository's latest **stable** release only. Clicking
**Update in background** downloads the platform asset, enforces a 512 MiB ceiling,
checks its exact size and GitHub-provided SHA-256 digest, and verifies the packaged
`--version` before any replacement. A missing digest is an error, not permission
to skip verification. Redirects must remain HTTPS on GitHub's release asset hosts.
No account credentials or conversation data are sent to GitHub.

The updater stages a sibling of the installed file/app. Windows uses `EMP.exe`,
Linux reads only the regular `EMP/EMP` member of the tar archive, and macOS mounts
the DMG read-only and copies `EMP.app` without following symlinks. User-writable
installations are supported; source checkouts, standalone macOS binaries, mounted
read-only disk images, and protected system package locations are not silently
converted or elevated. Use the package manager for protected `.deb` installations.

After staging, new requests are temporarily rejected with a retryable status while
accepted requests finish. Idle WebSocket connections do not count as running model
requests. If requests do not finish within five minutes, installation is cancelled
and the gate reopens. The helper confirms readiness before the old service stops,
waits for the old PyInstaller process and supervisor to exit, then replaces the
installation. EMP preserves the integration lease for the replacement process
to reconcile and opens a fresh authenticated management page after restart.

The new process must acknowledge startup with the planned version and nonce. If it
exits or does not acknowledge startup within one minute, only the updater-launched
process tree is stopped and the previous binary is restored and relaunched.
This is a binary rollback, not a rollback of a future release's data migrations.
Successful update staging is cleaned after the worker exits. Failed staging is
kept only when required to retain a recoverable old/failed installation.
The staging directory includes a fixed `worker-status.json` phase during
replacement, without exception messages, account information or request data.
Windows launch threads use `SetThreadErrorMode` to receive critical/bad-image
errors instead of waiting on an unattended OS dialog, and restore the calling
thread's previous mode after launch. This does not change system security policy.

The source tests cover release selection, URL boundaries, checksums, archive
selection, drain behavior, management authorization, and binary rollback. Native
packaged replacement must also be smoke-tested on the target OS before claiming
that OS's automatic update path is validated.

The package workflow runs `tests.test_packaged_self_update` with
`EMP_PACKAGE_UPDATE_SMOKE=1` on each native runner. It extracts that runner's real
release asset into an isolated installation, runs the frozen update worker,
checks the restarted service, and repeats with an invalid replacement to verify
binary rollback and service recovery. This does not require a public release or
an upstream account.

Intel macOS 15.7.3 validation used an isolated uv-managed Python 3.11 build.
Both native DMG scenarios passed: replacing the complete app bundle and restoring
it after an invalid replacement failed to start. Checks covered service health,
all bundle file hashes, launcher executable permissions, and successful staging
cleanup. The test resolves macOS's temporary-directory alias before configuring
the isolated vault; production key-path symlink protections remain unchanged.
This validates the local installer/rollback mechanism, not a live GitHub update
download, Gatekeeper approval, or Apple Silicon/Linux runtime behavior.

On a Windows test host without Python, `packaging/smoke_windows_update.ps1
-PackagePath <path-to-EMP.exe>` exercises the frozen worker and rollback in
temporary installations, then removes its own processes and data.
