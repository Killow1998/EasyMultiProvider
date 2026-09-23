//! Native update lifecycle with fixed public errors and recoverable installation state.
mod download;
mod extract;
pub mod manager;
mod process;
pub mod release;
pub mod worker;
pub use process::{OwnedChild, created, spawn};
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UpdateError(pub &'static str);
impl std::fmt::Display for UpdateError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.0)
    }
}
impl std::error::Error for UpdateError {}
impl From<std::io::Error> for UpdateError {
    fn from(error: std::io::Error) -> Self {
        Self(if error.kind() == std::io::ErrorKind::PermissionDenied {
            "directory_not_writable"
        } else {
            "update_failed"
        })
    }
}
pub type Result<T> = std::result::Result<T, UpdateError>;
