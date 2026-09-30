# Background updates

The management page checks the official repository's latest stable release.
Checking or installing requires the authenticated local management session. The
browser never downloads or executes release code itself.

For supported user-writable installations, **Update in background** downloads the
platform asset, enforces a 512 MiB ceiling, verifies the exact size and
GitHub-provided SHA-256 digest, and checks the packaged `--version` before any
replacement. Redirects must remain HTTPS on GitHub's release asset hosts. No
account credentials or conversation data are sent to GitHub.

The updater stages beside the installed file or app. Windows replaces `EMP.exe`,
Linux reads only the regular `EMP/EMP` member of the tar archive, and macOS mounts
the DMG read-only and copies `EMP.app` without following symlinks. Source
checkouts, standalone macOS binaries, mounted read-only disk images, and protected
portable directories are not silently converted into installations.

## Windows upgrades from Python releases

The existing master key and encrypted account files are reused. A legacy key
whose owner and sole FullControl grant are the current user can inherit that
grant from its private state directory. On first load, EMP protects that key's
DACL without changing the key bytes or its owner. A key granting another
principal access is still rejected rather than silently treated as private.

Private-state and updater writes preserve an already-correct owner. They request
DACL changes without also requesting ownership changes, so an ordinary user's
own installation does not need `WRITE_OWNER` merely to write the startup marker.
The Windows regression tests cover inherited keys, existing encrypted credentials,
an owner without `WRITE_OWNER`, and rejection of shared keys.

## Linux installation and migration

The recommended Linux installation uses `install-user.sh` from the extracted tar
archive. It installs under `$XDG_DATA_HOME/easy-multi-provider` (normally
`~/.local/share/easy-multi-provider`), with a launcher at `~/.local/bin/EMP` and a
user desktop entry. Configuration stays under `$XDG_CONFIG_HOME` (normally
`~/.config/easy-multi-provider/config.json`). The launcher executes the real
user-owned binary, so later background updates need no root authorization.

EMP does not overwrite a system-owned executable through `pkexec`. Reopening a
user-owned download after an authorization delay creates a substitution window
that checksum checks outside the privileged boundary cannot close. A detected
`/usr/bin/EMP` or dpkg installation therefore shows two explicit choices:

1. Recommended: restore native Codex and exit EMP, open the latest Release,
   download `EMP-linux-x86_64.tar.gz`, then run:

   ```sh
   sudo apt remove easy-multi-provider
   tar -xzf EMP-linux-x86_64.tar.gz
   cd EMP
   ./install-user.sh
   ~/.local/bin/EMP
   ```

   `apt remove` does not delete the per-user EMP configuration, so the new user
   installation reuses it without a copy step.

2. Remain system-managed: continue using the package manager that installed the
   existing system package. The current release does not publish a `.deb` asset.

The WebUI provides **Open latest Release** and **Copy migration commands** for a
system installation. It never asks for a password and never starts a privileged
replacement in the background.

## Replacement and rollback

After staging, new requests are temporarily rejected with a retryable status while
accepted requests finish. Idle WebSocket connections do not count as running model
requests. If requests do not finish within five minutes, installation is cancelled
and the gate reopens. The helper waits for the old packaged process to exit, then
replaces only the validated sibling installation and starts it in a visible window.

An exited process no longer counts as running just because its launcher retains
a Windows process handle. The native package check starts an old EMP, exits it
through the management API, and retains that handle through replacement and
rollback to exercise this handoff.

The new process must acknowledge startup with the planned version and nonce. If it
exits or does not acknowledge startup within one minute, only the updater-launched
process tree is stopped and the previous binary or app is restored and relaunched.
This is binary rollback, not rollback for future data migrations. The staging
directory contains only fixed phase diagnostics, without exception text, account
information, or request data.

The most recent failed attempt is also saved as `state/update-last-error.json`
beside the user configuration. This one small file is overwritten on the next
failure and survives cleanup of a failed staging directory. It records the
failed stage and available HTTP status, system error number or child exit code;
it does not store exception text, URLs, arguments, account data or credentials.
The update dialog shows the failure stage. If the receipt cannot be saved, the
same fixed fields remain in the process console and current management state.

The source tests cover release selection, URL boundaries, checksums, archive
selection, drain behavior, management authorization, and binary rollback. Native
packaged replacement must also be smoke-tested on each target OS before claiming
that OS's automatic update path is validated. System-level Linux replacement is
deliberately outside the in-app updater.
