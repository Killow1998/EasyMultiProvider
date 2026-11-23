pub mod filesystem;
pub mod lock;
#[cfg(windows)]
mod private_windows;

#[cfg(windows)]
#[doc(hidden)]
pub use private_windows::{CurrentUser, LocalAllocation, private_acl};

pub use filesystem::{
    FileTransaction, FilesystemError, MAX_ENCRYPTED_FILE_BYTES, MAX_TRANSACTION_FILE_BYTES,
    atomic_write_config, atomic_write_private_state, create_private_directory, read_file_limited,
    read_to_limit, with_file_transaction, write_catalog_json, write_new_private_file,
};
pub use lock::{IntegrationFileLock, LockError};
