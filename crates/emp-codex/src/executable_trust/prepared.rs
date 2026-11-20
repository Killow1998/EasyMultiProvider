use super::{TrustFailure, validate_executable};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct PreparedExecutable {
    path: PathBuf,
    selection: PathBuf,
}

impl PreparedExecutable {
    pub(crate) fn prepare(path: &Path) -> Result<Self, TrustFailure> {
        if !path.is_absolute() {
            return Err(TrustFailure::NotTrusted);
        }
        let canonical = path.canonicalize().map_err(|error| {
            if error.kind() == std::io::ErrorKind::NotFound {
                TrustFailure::Unavailable
            } else {
                TrustFailure::NotTrusted
            }
        })?;
        let prepared = Self {
            path: canonical,
            selection: path.to_owned(),
        };
        prepared.validate_path()?;
        #[cfg(target_os = "macos")]
        if let Some(bundle) = super::macos::known_bundle(prepared.path())
            && !super::macos::verify_bundle(&bundle, prepared.path())
        {
            return Err(TrustFailure::BundleVerificationFailed);
        }
        Ok(prepared)
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn validate_path(&self) -> Result<(), TrustFailure> {
        #[cfg(target_os = "macos")]
        if super::macos::known_bundle(&self.path).is_some() {
            return super::validate_executable_with(
                &self.path,
                super::macos::is_shared_applications,
            );
        }
        validate_executable(&self.path)
    }

    pub fn quota_selector(&self) -> &Path {
        &self.selection
    }
}
