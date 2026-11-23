//! Vault envelope encoding and decoding: `easy-multi-provider-v1\n` + Fernet.

pub mod fernet;
mod store;
pub use store::{MASTER_KEY_ENV, MASTER_KEY_FILE_ENV, VaultStore};

use crate::fernet::{Fernet, FernetError, FernetKey};
use std::fmt;
use zeroize::Zeroizing;

pub const VAULT_MAGIC: &[u8] = b"easy-multi-provider-v1\n";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VaultError {
    NotVaultFormat,
    DecryptFailed,
}

impl fmt::Display for VaultError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::NotVaultFormat => {
                f.write_str("credential file is not encrypted with the supported format")
            }
            Self::DecryptFailed => {
                f.write_str("credential file cannot be decrypted with the current master key")
            }
        }
    }
}

impl std::error::Error for VaultError {}

impl From<FernetError> for VaultError {
    fn from(_: FernetError) -> Self {
        Self::DecryptFailed
    }
}

pub type VaultResult<T> = Result<T, VaultError>;

pub fn encode_vault(key: &FernetKey, plaintext: &[u8]) -> Vec<u8> {
    let fernet = Fernet::new(key);
    let mut output = Vec::with_capacity(VAULT_MAGIC.len() + 128 + plaintext.len());
    output.extend_from_slice(VAULT_MAGIC);
    output.extend_from_slice(&fernet.encrypt(plaintext));
    output
}

pub fn decode_vault(key: &FernetKey, encoded: &[u8]) -> VaultResult<Zeroizing<Vec<u8>>> {
    if !encoded.starts_with(VAULT_MAGIC) {
        return Err(VaultError::NotVaultFormat);
    }
    let token = &encoded[VAULT_MAGIC.len()..];
    if token.is_empty() {
        return Err(VaultError::DecryptFailed);
    }
    let fernet = Fernet::new(key);
    fernet.decrypt(token).map_err(Into::into)
}
