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

#[cfg(test)]
mod tests {
    use super::super::release::MAX_PACKAGE_BYTES;
    use super::extract_tarball;
    use flate2::Compression;
    use flate2::read::GzDecoder;
    use flate2::write::GzEncoder;
    use std::fs::File;
    use std::io::Write;
    use std::path::Path;
    use tar::{Builder, EntryType, Header};
    use tempfile::TempDir;

    fn archive_path(root: &Path, name: &str) -> std::path::PathBuf {
        root.join(name)
    }

    fn append_file(builder: &mut Builder<GzEncoder<File>>, path: &str, bytes: &[u8]) {
        let mut header = Header::new_gnu();
        header.set_size(bytes.len() as u64);
        header.set_mode(0o755);
        header.set_cksum();
        builder.append_data(&mut header, path, bytes).unwrap();
    }

    #[test]
    fn rejects_traversal_archive_entries() {
        let temp = TempDir::new().unwrap();
        let archive = archive_path(temp.path(), "traversal.tar.gz");
        let mut encoder = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        let mut header = Header::new_gnu();
        header.set_path("EMP/EMP").unwrap();
        header.set_entry_type(EntryType::Regular);
        header.set_size(7);
        header.set_mode(0o755);
        let traversal = b"EMP/../escape";
        let raw_path = &mut header.as_mut_bytes()[..100];
        raw_path.fill(0);
        raw_path[..traversal.len()].copy_from_slice(traversal);
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(b"outside").unwrap();
        encoder.write_all(&[0; 505]).unwrap();
        encoder.write_all(&[0; 1024]).unwrap();
        encoder.finish().unwrap();
        let mut parsed = tar::Archive::new(GzDecoder::new(File::open(&archive).unwrap()));
        let mut entries = parsed.entries().unwrap();
        let entry = entries.next().unwrap().unwrap();
        assert_eq!(entry.path().unwrap(), Path::new("EMP/../escape"));
        assert_eq!(
            extract_tarball(&archive, temp.path()).unwrap_err(),
            super::super::UpdateError("invalid_package")
        );
    }

    #[test]
    fn rejects_symlink_archive_entries() {
        let temp = TempDir::new().unwrap();
        let archive = archive_path(temp.path(), "symlink.tar.gz");
        let encoder = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        let mut builder = Builder::new(encoder);
        let mut header = Header::new_gnu();
        header.set_entry_type(EntryType::Symlink);
        header.set_size(0);
        header.set_cksum();
        builder
            .append_link(&mut header, "EMP/EMP", "/bin/sh")
            .unwrap();
        builder.finish().unwrap();
        assert_eq!(
            extract_tarball(&archive, temp.path()).unwrap_err(),
            super::super::UpdateError("invalid_package")
        );
    }

    #[test]
    fn rejects_duplicate_archive_paths() {
        let temp = TempDir::new().unwrap();
        let archive = archive_path(temp.path(), "duplicate.tar.gz");
        let encoder = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        let mut builder = Builder::new(encoder);
        append_file(&mut builder, "EMP/EMP", b"first");
        append_file(&mut builder, "EMP/EMP", b"second");
        builder.finish().unwrap();
        assert_eq!(
            extract_tarball(&archive, temp.path()).unwrap_err(),
            super::super::UpdateError("invalid_package")
        );
    }

    #[test]
    fn rejects_declared_archive_size_over_limit_before_reading_content() {
        let temp = TempDir::new().unwrap();
        let archive = archive_path(temp.path(), "oversized.tar.gz");
        let mut encoder = GzEncoder::new(File::create(&archive).unwrap(), Compression::default());
        let mut header = Header::new_gnu();
        header.set_path("EMP/EMP").unwrap();
        header.set_entry_type(EntryType::Regular);
        header.set_size(MAX_PACKAGE_BYTES + 1);
        header.set_mode(0o755);
        header.set_cksum();
        encoder.write_all(header.as_bytes()).unwrap();
        encoder.write_all(&[0; 1024]).unwrap();
        encoder.finish().unwrap();
        assert_eq!(
            extract_tarball(&archive, temp.path()).unwrap_err(),
            super::super::UpdateError("invalid_package")
        );
    }
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
