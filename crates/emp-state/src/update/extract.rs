//! Candidate extraction. Only regular files inside the expected package root are accepted.
use super::release::MAX_PACKAGE_BYTES;
use super::{Result, UpdateError};
use flate2::read::GzDecoder;
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Write};
use std::path::{Component, Path, PathBuf};
#[cfg(target_os = "macos")]
use std::process::Command;

pub(super) fn extract_candidate(package: &Path, job: &Path, _relative: &str) -> Result<PathBuf> {
    if package
        .extension()
        .is_some_and(|extension| extension == "exe")
    {
        let candidate = job.join("candidate.exe");
        fs::rename(package, &candidate)?;
        return Ok(candidate);
    }
    if package
        .file_name()
        .is_some_and(|name| name.to_string_lossy().ends_with(".tar.gz"))
    {
        return extract_tarball(package, job);
    }
    #[cfg(target_os = "macos")]
    if package
        .extension()
        .is_some_and(|extension| extension == "dmg")
    {
        return extract_dmg(package, job);
    }
    Err(UpdateError("invalid_package"))
}

fn extract_tarball(package: &Path, job: &Path) -> Result<PathBuf> {
    let file = File::open(package)?;
    let mut archive = tar::Archive::new(GzDecoder::new(file));
    let candidate = job.join("candidate");
    let mut seen = HashSet::new();
    let mut total = 0_u64;
    let mut found = false;
    let entries = archive
        .entries()
        .map_err(|_| UpdateError("invalid_package"))?;
    for entry in entries {
        let mut entry = entry.map_err(|_| UpdateError("invalid_package"))?;
        let path = entry
            .path()
            .map_err(|_| UpdateError("invalid_package"))?
            .into_owned();
        validate_archive_path(&path)?;
        if !seen.insert(path.clone()) {
            return Err(UpdateError("invalid_package"));
        }
        let kind = entry.header().entry_type();
        if !kind.is_file() && !kind.is_dir() {
            return Err(UpdateError("invalid_package"));
        }
        let size = entry.size();
        total = total
            .checked_add(size)
            .ok_or(UpdateError("invalid_package"))?;
        if size > MAX_PACKAGE_BYTES || total > MAX_PACKAGE_BYTES {
            return Err(UpdateError("invalid_package"));
        }
        if kind.is_dir() {
            if size != 0 {
                return Err(UpdateError("invalid_package"));
            }
            continue;
        }
        if path == Path::new("EMP/EMP") {
            if found || size == 0 {
                return Err(UpdateError("invalid_package"));
            }
            found = true;
            let mut output = OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&candidate)?;
            let copied =
                io::copy(&mut entry, &mut output).map_err(|_| UpdateError("invalid_package"))?;
            if copied != size {
                return Err(UpdateError("invalid_package"));
            }
            output.flush()?;
            output.sync_all()?;
        } else {
            let copied = io::copy(&mut entry, &mut io::sink())
                .map_err(|_| UpdateError("invalid_package"))?;
            if copied != size {
                return Err(UpdateError("invalid_package"));
            }
        }
    }
    if !found {
        return Err(UpdateError("invalid_package"));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&candidate, fs::Permissions::from_mode(0o755))?;
    }
    Ok(candidate)
}

fn validate_archive_path(path: &Path) -> Result<()> {
    let mut components = path.components();
    if !matches!(components.next(), Some(Component::Normal(root)) if root == "EMP")
        || components.any(|component| !matches!(component, Component::Normal(_)))
    {
        return Err(UpdateError("invalid_package"));
    }
    Ok(())
}

#[cfg(target_os = "macos")]
fn extract_dmg(package: &Path, job: &Path) -> Result<PathBuf> {
    let mount = job.join("mount");
    let candidate = job.join("candidate.app");
    fs::create_dir(&mount)?;
    let attach = Command::new("/usr/bin/hdiutil")
        .args(["attach", "-readonly", "-nobrowse", "-mountpoint"])
        .arg(&mount)
        .arg(package)
        .status()
        .map_err(|_| UpdateError("invalid_package"))?;
    if !attach.success() {
        let _ = fs::remove_dir(&mount);
        return Err(UpdateError("invalid_package"));
    }
    let copy_result = copy_bundle(&mount.join("EMP.app"), &candidate);
    let detach = Command::new("/usr/bin/hdiutil")
        .args(["detach", "-force"])
        .arg(&mount)
        .status();
    if copy_result.is_err() || !detach.is_ok_and(|status| status.success()) {
        return Err(UpdateError("invalid_package"));
    }
    copy_result?;
    Ok(candidate)
}

#[cfg(target_os = "macos")]
fn copy_bundle(source: &Path, destination: &Path) -> Result<()> {
    if !source.is_dir() || source.is_symlink() {
        return Err(UpdateError("invalid_package"));
    }
    fs::create_dir(destination)?;
    for item in fs::read_dir(source)? {
        let item = item?;
        let source_path = item.path();
        let destination_path = destination.join(item.file_name());
        let metadata = fs::symlink_metadata(&source_path)?;
        if metadata.file_type().is_symlink() {
            return Err(UpdateError("invalid_package"));
        }
        if metadata.is_dir() {
            copy_bundle(&source_path, &destination_path)?;
        } else if metadata.is_file() {
            fs::copy(&source_path, &destination_path)?;
            fs::set_permissions(&destination_path, metadata.permissions())?;
        } else {
            return Err(UpdateError("invalid_package"));
        }
    }
    Ok(())
}
