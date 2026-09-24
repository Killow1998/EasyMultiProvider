//! Read-only detection of Linux installations managed by the system package manager.
use super::release::parse_version;
use super::{Result, UpdateError};
use std::ffi::CString;
use std::os::unix::ffi::OsStrExt;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

const DPKG_QUERY: &str = "/usr/bin/dpkg-query";
const PACKAGE: &str = "easy-multi-provider";

pub(super) fn system_install_manual(target: &Path, query: &Path) -> Result<bool> {
    let parent = target
        .parent()
        .ok_or(UpdateError("unsupported_installation"))?;
    let parent = CString::new(parent.as_os_str().as_bytes())
        .map_err(|_| UpdateError("unsupported_installation"))?;
    // SAFETY: parent is a valid NUL-terminated path; access does not modify it.
    let writable = unsafe { libc::access(parent.as_ptr(), libc::W_OK) == 0 };
    Ok(package_version(target, query)?.is_some() || !writable)
}

pub(super) fn default_query() -> &'static Path {
    Path::new(DPKG_QUERY)
}

fn package_version(target: &Path, query: &Path) -> Result<Option<String>> {
    if !query.is_file() {
        return Ok(None);
    }
    let owner = query_output(query, &["--search"], Some(target))?;
    let suffix = [b": ".as_slice(), target.as_os_str().as_bytes()].concat();
    let matches = owner
        .stdout
        .split(|byte| *byte == b'\n')
        .filter_map(|line| line.strip_suffix(suffix.as_slice()))
        .collect::<Vec<_>>();
    if matches.is_empty() {
        return if owner.status.success() || owner.status.code() == Some(1) {
            Ok(None)
        } else {
            Err(UpdateError("package_query_failed"))
        };
    }
    if matches != [PACKAGE.as_bytes()] || target != Path::new("/usr/bin/EMP") {
        return Err(UpdateError("unsupported_installation"));
    }
    let version = query_output(
        query,
        &["--show", "--showformat=${Status}\t${Version}", PACKAGE],
        None,
    )?;
    let output =
        std::str::from_utf8(&version.stdout).map_err(|_| UpdateError("package_query_failed"))?;
    let (status, value) = output
        .trim()
        .split_once('\t')
        .ok_or(UpdateError("package_query_failed"))?;
    if !version.status.success() || status != "install ok installed" {
        return Err(UpdateError("package_query_failed"));
    }
    parse_version(value)?;
    Ok(Some(value.to_owned()))
}

fn query_output(
    query: &Path,
    args: &[&str],
    target: Option<&Path>,
) -> Result<std::process::Output> {
    let mut command = Command::new(query);
    command.args(args);
    if let Some(target) = target {
        command.arg(target);
    }
    command
        .env_remove("LD_LIBRARY_PATH_ORIG")
        .env("LC_ALL", "C");
    if let Some(original) =
        std::env::var_os("LD_LIBRARY_PATH_ORIG").filter(|value| !value.is_empty())
    {
        command.env("LD_LIBRARY_PATH", original);
    } else {
        command.env_remove("LD_LIBRARY_PATH");
    }
    let mut child = command
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| UpdateError("package_query_failed"))?;
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if child
            .try_wait()
            .map_err(|_| UpdateError("package_query_failed"))?
            .is_some()
        {
            return child
                .wait_with_output()
                .map_err(|_| UpdateError("package_query_failed"));
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(UpdateError("package_query_failed"));
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[cfg(test)]
mod tests {
    use super::{package_version, system_install_manual};
    use crate::update::UpdateError;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;
    use std::path::Path;

    fn fake_query(script: &str) -> (tempfile::TempDir, std::path::PathBuf) {
        let directory = tempfile::tempdir().unwrap();
        let query = directory.path().join("dpkg-query");
        fs::write(&query, script).unwrap();
        fs::set_permissions(&query, fs::Permissions::from_mode(0o700)).unwrap();
        (directory, query)
    }

    #[test]
    fn package_ownership_is_exact_and_read_only() {
        let (_directory, query) = fake_query(
            "#!/bin/sh\nif [ \"$1\" = --search ]; then printf 'easy-multi-provider: /usr/bin/EMP\\n'; else printf 'install ok installed\\t0.11.2'; fi\n",
        );
        assert_eq!(
            package_version(Path::new("/usr/bin/EMP"), &query)
                .unwrap()
                .as_deref(),
            Some("0.11.2")
        );
        assert!(system_install_manual(Path::new("/usr/bin/EMP"), &query).unwrap());
        let (_directory, other) =
            fake_query("#!/bin/sh\nprintf 'different-package: /usr/bin/EMP\\n'\n");
        assert_eq!(
            package_version(Path::new("/usr/bin/EMP"), &other),
            Err(UpdateError("unsupported_installation"))
        );
        let (_directory, missing) = fake_query("#!/bin/sh\nexit 1\n");
        assert_eq!(
            package_version(Path::new("/usr/bin/EMP"), &missing).unwrap(),
            None
        );
        let (_directory, broken) = fake_query("#!/bin/sh\nexit 2\n");
        assert_eq!(
            package_version(Path::new("/usr/bin/EMP"), &broken),
            Err(UpdateError("package_query_failed"))
        );
    }
}
