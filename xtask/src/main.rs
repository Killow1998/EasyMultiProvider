//! Repository automation for EMP, run as `cargo xtask <command>`.
mod package;
mod release;
mod smoke;
mod windows;

use sha2::{Digest, Sha256};
use std::fs::{self, File};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};

pub type Result<T = ()> = std::result::Result<T, String>;

/// The source version every package and release tag must carry.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
pub const PRODUCT_NAME: &str = "EMP";

const USAGE: &str = "usage: cargo xtask <command>

commands:
  package [--skip-service-smoke]   build and smoke-test this platform's native package
  package-smoke                    run update/install smoke tests against artifacts/
  validate-release --tag TAG --artifacts DIR [--public-manifest FILE]
  release-notes TAG                print this version's CHANGELOG section";

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let rest = args.get(1..).unwrap_or_default();
    let result = match args.first().map(String::as_str) {
        Some("package") => package::run(rest),
        Some("package-smoke") => smoke::run(rest),
        Some("validate-release") => release::validate_command(rest),
        Some("release-notes") => release::notes_command(rest),
        _ => Err(USAGE.to_owned()),
    };
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            if std::env::var("GITHUB_ACTIONS").as_deref() == Ok("true") {
                let escaped = error
                    .replace('%', "%25")
                    .replace('\r', "%0D")
                    .replace('\n', "%0A");
                eprintln!("::error title=xtask failed::{escaped}");
            }
            eprintln!("xtask: {error}");
            ExitCode::FAILURE
        }
    }
}

pub fn project_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("xtask lives inside the workspace")
        .to_owned()
}

pub fn sha256_file(path: &Path) -> Result<String> {
    let mut hasher = Sha256::new();
    let mut file = File::open(path).map_err(|error| format!("{}: {error}", path.display()))?;
    io::copy(&mut file, &mut hasher).map_err(|error| format!("{}: {error}", path.display()))?;
    Ok(format!("{:x}", hasher.finalize()))
}

pub fn run_command(command: &mut Command) -> Result {
    let status = command
        .status()
        .map_err(|error| format!("could not run {:?}: {error}", command.get_program()))?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("{:?} failed with {status}", command.get_program()))
    }
}

pub fn copy(from: &Path, to: &Path) -> Result {
    fs::copy(from, to)
        .map(drop)
        .map_err(|error| format!("copy {} -> {}: {error}", from.display(), to.display()))
}

#[cfg(unix)]
pub fn make_executable(path: &Path) -> Result {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o755))
        .map_err(|error| format!("chmod {}: {error}", path.display()))
}

#[cfg(not(unix))]
pub fn make_executable(_path: &Path) -> Result {
    Ok(())
}

/// A path without symlink components. macOS exposes its temporary directory
/// through /var -> /private/var, and EMP rejects key paths through symlinks;
/// Windows keeps the plain absolute form rather than a `\\?\` path.
pub fn physical(path: &Path) -> Result<PathBuf> {
    let resolved = if cfg!(windows) {
        std::path::absolute(path)
    } else {
        path.canonicalize()
    };
    resolved.map_err(|error| format!("{}: {error}", path.display()))
}

pub fn temporary_directory(prefix: &str) -> Result<tempfile::TempDir> {
    tempfile::Builder::new()
        .prefix(prefix)
        .tempdir()
        .map_err(|error| format!("temporary directory: {error}"))
}
