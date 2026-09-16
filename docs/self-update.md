# Background updates

The management page has two separate controls: **Check updates** and a link to
the repository. Checking or installing requires the authenticated local management
session. The browser never fetches or runs release code itself.

Opening **Check updates** starts a fresh check. If a check, download, or installation
is already running, the dialog shows its progress without restarting it. The dialog
offers **Update in background** when an installable release is available.

EMP checks the official repository's latest **stable** release only. Clicking
**Update in background** downloads the platform asset, enforces a 512 MiB ceiling,
checks its exact size and GitHub-provided SHA-256 digest, and verifies the packaged
`--version` before any replacement. A missing digest is an error, not permission
to skip verification. Redirects must remain HTTPS on GitHub's release asset hosts.
No account credentials or conversation data are sent to GitHub.

The updater stages a sibling of the installed file/app. Windows uses `EMP.exe`,
Linux reads only the regular `EMP/EMP` member of the tar archive, and macOS mounts
the DMG read-only and copies `EMP.app` without following symlinks. User-writable
installations are supported. Source checkouts, standalone macOS binaries and
mounted read-only disk images are not silently converted into installations.

The recommended Linux installation uses `install-user.sh` from the tar archive.
It installs under `$XDG_DATA_HOME/easy-multi-provider` (normally
`~/.local/share/easy-multi-provider`), with a launcher at `~/.local/bin/EMP` and
a user desktop entry. Configuration stays under `$XDG_CONFIG_HOME` (normally
`~/.config/easy-multi-provider`). The launcher executes the real binary rather
than a symlink; updates stage beside that user-owned binary and never request
root authorization. Existing files and configurations are not overwritten by
the installer. This avoids the privileged staging boundary described below.

For existing system installations, EMP queries dpkg ownership instead of guessing from the directory.
An installed `easy-multi-provider` package uses the release's `.deb`, preserving
the package database and desktop resources. A protected portable installation
uses the tar archive. Downloads and version checks run as the desktop user in a
private temporary directory. EMP also prepares a recovery copy before proceeding;
deb updates require the matching, verified old release package. If that package
is unavailable or differs from the installed binary, nothing is changed.

After active requests finish, `pkexec` requests administrator authorization from
the desktop's Polkit agent. Only `/usr/bin/dpkg --install` or `/usr/bin/install`
runs elevated. EMP never receives a password, installs a permissive Polkit rule,
changes directory permissions, or runs the restarted service as root. System
tools do not inherit PyInstaller's private library path. Cancelling authorization
keeps the old service running and reopens the request gate. The page distinguishes
cancellation, unavailable authorization, installation and recovery failures.

Known security boundary: the elevated system tool reopens a user-owned staging
path after Polkit authorization. The checksum and private directory do not prevent
another process with the same user's write access from replacing that file while
authorization is pending. Post-install executable/version checks cannot verify
all effects of a substituted deb package. This path is not hardened against that
local attacker; adding another pre-install checksum would not close the window.
A trusted privileged installer that snapshots and verifies the whole release
inside its own boundary, or a system-package-manager-only update flow, is needed
before claiming that protection. The existing desktop authorization test checks
functionality, not resistance to this substitution attack.

A Linux desktop with `pkexec` and a Polkit authentication agent is required for
this path. SSH/headless sessions without an agent cannot display the desktop
dialog: use the system package manager in an interactive terminal instead.
The background updater does not collect a sudo password or fake a browser prompt.
See the [Polkit authorization behavior](https://polkit.pages.freedesktop.org/polkit/pkexec.1.html).

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
For system Linux installations, replacement happens through the authorized system
tool before the old service stops. The ordinary-user worker then restarts EMP.
Recovery uses the same system tool, including restoring the old `.deb` and its
package metadata, and may require a second authorization. If recovery is denied
or fails, files remain recoverable and no success is reported. A package manager
is never force-killed on a timeout while modifying its database. WebUI exit is
blocked during authorization, replacement and recovery.
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

Linux x86_64 validation additionally covers real deb inspection, isolated dpkg
upgrade/downgrade (both binary contents and package version), replacement of a
running portable executable, and the frozen worker restarting as the original
user from non-sibling private staging. These tests use temporary roots and user
namespaces, not the machine's package database. Cancellation and missing-agent
return codes are covered by backend tests.

Desktop authorization was accepted interactively on Linux x86_64 with GNOME/X11.
The test called the production `apply_install` function from a desktop terminal
against an isolated directory that the ordinary user could not write. After the
tester confirmed the system password dialog, the real `pkexec`/`install` operation
replaced the file and then restored its original contents. Both content checks
passed; the calling process and replacement file retained the ordinary user's
UID. No installed application or Codex configuration was changed, and temporary
test files were removed. This verifies desktop authorization and protected-file
replacement, not a live GitHub-to-system-deb upgrade.

The same host's SSH session returned Polkit's `127` / no-authentication-agent
failure for a no-op probe. That headless-session result is separate from the
successful desktop authorization test.

On a Windows test host without Python, `packaging/smoke_windows_update.ps1
-PackagePath <path-to-EMP.exe>` exercises the frozen worker and rollback in
temporary installations, then removes its own processes and data.
