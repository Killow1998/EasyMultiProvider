pub mod gemini_signatures;
pub mod vault;

pub use emp_files::FilesystemError;
pub use vault::fernet;
pub use vault::fernet::{Fernet, FernetError, FernetKey, FernetKeyError};
pub use vault::{
    MASTER_KEY_ENV, MASTER_KEY_FILE_ENV, VAULT_MAGIC, VaultError, VaultResult, VaultStore,
    decode_vault, encode_vault,
};
