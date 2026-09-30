//! Prepare signed desktop engines without executing from shared Applications.
use super::{TrustFailure, validate_executable};
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const ENGINE_REQUIREMENT: &str = "=anchor apple generic and certificate 1[field.1.2.840.113635.100.6.2.6] and certificate leaf[field.1.2.840.113635.100.6.1.13] and certificate leaf[subject.OU] = \"2DC432GLL2\" and identifier \"codex\"";
const ENGINE_RELATIVE: &str = "Contents/MacOS/codex";
const MAX_ENTRIES: usize = 128;
const MAX_BYTES: u64 = 512 * 1024 * 1024;
// macOS SDK sys/clonefile.h; libc does not currently expose this flag.
// Root-installed sources must still produce caller-owned temporary files.
#[cfg(target_os = "macos")]
const CLONE_NOOWNERCOPY: u32 = 0x0002;

#[cfg(target_os = "macos")]
pub(super) fn prepare_snapshot(path: &Path) -> Result<(tempfile::TempDir, PathBuf), TrustFailure> {
    let bundle = eligible_bundle(path).ok_or(TrustFailure::Writable)?;
    snapshot_with_verifier(&bundle, None, verify_snapshot)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn eligible_bundle(path: &Path) -> Option<PathBuf> {
    let bundle = known_bundle(path)?;
    let target = fs::metadata(path).ok()?;
    // Keep the target and every other ancestor's original trust rules.
    // SAFETY: getuid has no preconditions and cannot fail.
    let uid = unsafe { libc::getuid() };
    let groups = super::PrivateGroups::current(uid);
    if !target.is_file()
        || target.mode() & 0o111 == 0
        || target.mode() & 0o6000 != 0
        || !super::trusted_owner(target.uid(), uid)
        || super::foreign_writable(&target, None, uid, &groups)
    {
        return None;
    }
    let mut child_uid = target.uid();
    let mut shared_applications = false;
    for ancestor in path.ancestors().skip(1) {
        let info = fs::metadata(ancestor).ok()?;
        if super::foreign_writable(&info, Some(child_uid), uid, &groups) {
            if !is_shared_applications(ancestor, &info) {
                return None;
            }
            shared_applications = true;
        }
        child_uid = info.uid();
    }
    shared_applications.then_some(bundle)
}

fn known_bundle(path: &Path) -> Option<PathBuf> {
    ["ChatGPT.app", "Codex.app"].iter().find_map(|app| {
        let bundle = Path::new("/Applications")
            .join(app)
            .join("Contents/Resources/codex-cli/CodexCLI.app");
        (path == bundle.join(ENGINE_RELATIVE)).then_some(bundle)
    })
}

fn is_shared_applications(path: &Path, info: &fs::Metadata) -> bool {
    path == Path::new("/Applications")
        && info.is_dir()
        && info.uid() == 0
        && info.mode() & 0o022 == 0o020
        && info.mode() & 0o6000 == 0
}

fn snapshot_with_verifier(
    source: &Path,
    temporary_parent: Option<&Path>,
    verify: impl FnOnce(&Path, &Path) -> bool,
) -> Result<(tempfile::TempDir, PathBuf), TrustFailure> {
    let mut builder = tempfile::Builder::new();
    builder
        .prefix("emp-codex-engine-")
        .permissions(fs::Permissions::from_mode(0o700));
    let directory = match temporary_parent {
        Some(parent) => builder.tempdir_in(parent),
        None => builder.tempdir(),
    }
    .map_err(|_| TrustFailure::Unavailable)?;
    let destination = directory.path().join("CodexCLI.app");
    let mut budget = CopyBudget {
        entries: 0,
        bytes: 0,
    };
    copy_bundle(source, &destination, 0, &mut budget).map_err(|_| TrustFailure::NotTrusted)?;
    let executable = destination
        .join(ENGINE_RELATIVE)
        .canonicalize()
        .map_err(|_| TrustFailure::Unavailable)?;
    // The new owner and ALL destination ancestors must satisfy the unchanged
    // strict validator before we run even the signature verifier.
    validate_executable(&executable)?;
    if !verify(&destination, &executable) {
        return Err(TrustFailure::NotTrusted);
    }
    Ok((directory, executable))
}

struct CopyBudget {
    entries: usize,
    bytes: u64,
}

fn copy_bundle(
    source: &Path,
    destination: &Path,
    depth: usize,
    budget: &mut CopyBudget,
) -> std::io::Result<()> {
    budget.entries += 1;
    if depth > 8 || budget.entries > MAX_ENTRIES {
        return Err(std::io::Error::other("bundle snapshot limit exceeded"));
    }
    let info = fs::symlink_metadata(source)?;
    if info.is_dir() {
        fs::DirBuilder::new().mode(0o700).create(destination)?;
        for entry in fs::read_dir(source)? {
            let entry = entry?;
            copy_bundle(
                &entry.path(),
                &destination.join(entry.file_name()),
                depth + 1,
                budget,
            )?;
        }
        return Ok(());
    }
    if !info.is_file() {
        return Err(std::io::Error::other(
            "bundle links and special files are not supported",
        ));
    }
    // A source swap cannot make copying follow a symlink or block on a FIFO.
    // All copied bytes are subsequently authenticated at the private destination.
    let mut source_file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(source)?;
    let info = source_file.metadata()?;
    if !info.is_file() || info.len() > MAX_BYTES - budget.bytes {
        return Err(std::io::Error::other("bundle snapshot limit exceeded"));
    }
    budget.bytes += info.len();
    if !clone_file(&source_file, destination)? {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(destination)?;
        let copied = std::io::copy(&mut (&mut source_file).take(info.len() + 1), &mut output)?;
        output.flush()?;
        if copied != info.len() {
            return Err(std::io::Error::other("bundle changed while copying"));
        }
    }
    if fs::metadata(destination)?.len() != info.len() {
        return Err(std::io::Error::other("bundle changed while cloning"));
    }
    fs::set_permissions(
        destination,
        fs::Permissions::from_mode(if info.mode() & 0o111 != 0 {
            0o500
        } else {
            0o400
        }),
    )
}

#[cfg(target_os = "macos")]
fn clone_file(source: &File, destination: &Path) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;
    use std::os::unix::ffi::OsStrExt;
    let destination = std::ffi::CString::new(destination.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::other("invalid snapshot path"))?;
    // SAFETY: the source descriptor and destination C string are valid for
    // the call. APFS cloning creates a different inode, never a hard link.
    if unsafe {
        libc::fclonefileat(
            source.as_raw_fd(),
            libc::AT_FDCWD,
            destination.as_ptr(),
            CLONE_NOOWNERCOPY,
        )
    } == 0
    {
        return Ok(true);
    }
    let error = std::io::Error::last_os_error();
    match error.raw_os_error() {
        Some(libc::ENOTSUP | libc::EXDEV | libc::ENOSYS) => Ok(false),
        _ => Err(error),
    }
}

#[cfg(not(target_os = "macos"))]
fn clone_file(_source: &File, _destination: &Path) -> std::io::Result<bool> {
    Ok(false)
}

fn verify_snapshot(bundle: &Path, executable: &Path) -> bool {
    // Bundle verification authenticates its main Mach-O and Info.plist.
    // Bind that authenticated main executable to our exact destination,
    // avoiding a second full hash of the same engine. See Apple's Code
    // Signing Guide, "Examining a Code Signature", and Bundle Structures.
    let mut command = Command::new("/usr/bin/codesign");
    command
        .env_clear()
        .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
        .args([
            "--verify",
            "--strict",
            "--all-architectures",
            "--deep",
            "-R",
            ENGINE_REQUIREMENT,
        ])
        .arg(bundle);
    run_verifier(&mut command, Duration::from_secs(10)) && bundle_binds_engine(bundle, executable)
}

fn bundle_binds_engine(bundle: &Path, executable: &Path) -> bool {
    if bundle.join(ENGINE_RELATIVE).canonicalize().ok().as_deref() != Some(executable) {
        return false;
    }
    let Ok(file) = File::open(bundle.join("Contents/Info.plist")) else {
        return false;
    };
    const MAX_INFO_BYTES: u64 = 256 * 1024;
    let mut bytes = Vec::new();
    if file
        .take(MAX_INFO_BYTES + 1)
        .read_to_end(&mut bytes)
        .is_err()
        || bytes.len() as u64 > MAX_INFO_BYTES
    {
        return false;
    }
    plist::Value::from_reader(std::io::Cursor::new(bytes))
        .ok()
        .and_then(|value| {
            value
                .as_dictionary()
                .and_then(|dictionary| dictionary.get("CFBundleExecutable"))
                .and_then(plist::Value::as_string)
                .map(|name| name == "codex")
        })
        == Some(true)
}

fn run_verifier(command: &mut Command, timeout: Duration) -> bool {
    let started = Instant::now();
    let Ok(mut child) = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
    else {
        return false;
    };
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return status.success() && started.elapsed() <= timeout,
            Ok(None) if started.elapsed() < timeout => {
                std::thread::sleep(Duration::from_millis(10))
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

#[cfg(test)]
mod tests;
