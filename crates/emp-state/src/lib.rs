//! State cryptography and migration envelope compatibility for EMP.
//!
//! This crate preserves the Python implementation's encrypted-at-rest formats
//! without introducing a filesystem layer. It provides:
//!
//! - [`vault`]: `easy-multi-provider-v1\n` + Fernet token encoding.
//! - [`migration`]: `EMP-MIGRATION\x01\n` + JSON envelope with
//!   scrypt-derived Fernet encryption.
//! - [`fernet`]: Fernet key material and token operations.
//!
//! Crypto primitives come from the RustCrypto ecosystem crates; sensitive
//! buffers are zeroized.

pub mod config;
pub mod fernet;
pub mod filesystem;
pub mod migration;
pub mod vault;

pub use config::{
    ConfigError, ConfigResult, canonical_catalog_json, catalog_etag,
    normalize_catalog_presentations, normalize_codex_runtime_sources,
    normalize_subscription_search,
};
pub use fernet::{Fernet, FernetError, FernetKey, FernetKeyError};
pub use filesystem::{
    FileTransaction, FilesystemError, MASTER_KEY_ENV, MASTER_KEY_FILE_ENV,
    MAX_TRANSACTION_FILE_BYTES, VaultStore, with_file_transaction,
};
pub use migration::{
    MAX_BUNDLE_BYTES, MAX_PASSWORD_BYTES, MIGRATION_MAGIC, MIGRATION_SCHEMA, MIGRATION_VERSION,
    MIN_PASSWORD_BYTES, MigrationError, MigrationParams, MigrationResult, SALT_BYTES, SCRYPT_N,
    SCRYPT_P, SCRYPT_R, decode_migration, encode_migration, parse_envelope,
};
pub use vault::{VAULT_MAGIC, VaultError, VaultResult, decode_vault, encode_vault};
