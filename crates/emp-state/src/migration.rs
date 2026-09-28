//! `.emp` migration envelope encoding and decoding.
//!
//! Envelope version 1 is the Python implementation's `EMP-MIGRATION\x01\n` +
//! scrypt(N=2^14)/Fernet format and is still accepted on import. New exports
//! write envelope version 2, which keeps the same framing and Fernet payload
//! but derives the key with a stronger, fixed scrypt cost (N=2^17).

use crate::accounts::{
    account_auth_path, normalize_account, same_account_auth, validate_auth_json,
};
use crate::config::{
    load_configuration, normalize_configuration, save_configuration_in_transaction,
};
use crate::fernet::{Fernet, FernetKey};
use crate::filesystem::{FilesystemError, VaultStore, read_file_limited, with_file_transaction};
use base64::Engine;
use base64::engine::general_purpose::URL_SAFE;
use scrypt::{Params, scrypt};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};
use zeroize::{Zeroize, Zeroizing};

/// Exact magic prefix for migration bundles.
pub const MIGRATION_MAGIC: &[u8] = b"EMP-MIGRATION\x01\n";
/// Schema string expected inside the JSON envelope.
pub const MIGRATION_SCHEMA: &str = "easy-multi-provider-migration";
/// Schema version of the decrypted payload and of the legacy (v1) envelope.
pub const MIGRATION_VERSION: u64 = 1;
/// Envelope version written by new exports (stronger fixed scrypt cost).
pub const MIGRATION_ENVELOPE_VERSION: u64 = 2;
/// Maximum encoded migration bundle size in bytes.
pub const MAX_BUNDLE_BYTES: usize = 32 * 1024 * 1024;
/// Minimum password length after trimming, in UTF-8 bytes, for new exports.
pub const MIN_PASSWORD_BYTES: usize = 12;
/// Minimum password length accepted when decoding, in UTF-8 bytes. Legacy v1
/// bundles were exported with an 8-byte minimum and must stay importable.
pub const MIN_IMPORT_PASSWORD_BYTES: usize = 8;
/// Maximum password length after trimming, in UTF-8 bytes.
pub const MAX_PASSWORD_BYTES: usize = 4096;
/// Required salt length in bytes.
pub const SALT_BYTES: usize = 16;
/// Fixed scrypt N parameter of legacy v1 envelopes.
pub const SCRYPT_N: u64 = 16384;
/// Fixed scrypt r parameter of legacy v1 envelopes.
pub const SCRYPT_R: u64 = 8;
/// Fixed scrypt p parameter of legacy v1 envelopes.
pub const SCRYPT_P: u64 = 1;
/// Fixed scrypt N parameter of v2 envelopes.
pub const SCRYPT_V2_N: u64 = 131072;
/// Fixed scrypt r parameter of v2 envelopes.
pub const SCRYPT_V2_R: u64 = 8;
/// Fixed scrypt p parameter of v2 envelopes.
pub const SCRYPT_V2_P: u64 = 1;
const MAX_NATIVE_CATALOG_BYTES: usize = 4 * 1024 * 1024;
const MAX_NATIVE_AUTH_BYTES: usize = 1024 * 1024;
const MODEL_CAPABILITY_MIGRATION_FIELDS: [&str; 6] = [
    "input_modalities",
    "output_modalities",
    "supported_protocols",
    "supports_image_detail_original",
    "capabilities",
    "capability_sources",
];

/// Fixed KDF parameters (attacker input cannot select cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationParams {
    pub n: u64,
    pub r: u64,
    pub p: u64,
}

impl MigrationParams {
    /// Fixed parameters of legacy v1 envelopes.
    pub const V1: MigrationParams = MigrationParams {
        n: SCRYPT_N,
        r: SCRYPT_R,
        p: SCRYPT_P,
    };
    /// Fixed parameters of v2 envelopes.
    pub const V2: MigrationParams = MigrationParams {
        n: SCRYPT_V2_N,
        r: SCRYPT_V2_R,
        p: SCRYPT_V2_P,
    };

    /// Fixed parameters for a supported envelope version. Parameters are
    /// never read from the bundle itself.
    pub fn for_envelope_version(version: u64) -> Option<MigrationParams> {
        match version {
            MIGRATION_VERSION => Some(Self::V1),
            MIGRATION_ENVELOPE_VERSION => Some(Self::V2),
            _ => None,
        }
    }
}

impl Default for MigrationParams {
    fn default() -> Self {
        Self::V2
    }
}

/// Errors from migration operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MigrationError {
    PasswordTooShort,
    PasswordTooLong,
    TooLarge,
    NotMigrationFormat,
    InvalidEnvelope,
    UnsupportedVersion,
    UnsupportedKdf,
    InvalidSalt,
    InvalidPayload,
    DecryptFailed,
    InvalidConfiguration,
    InvalidAccounts,
    InvalidAccountRecord,
    DuplicateAccountIds,
    InvalidProviderKeys,
    UnknownProviderKey,
    InvalidExportGroup,
    AccountCredentialsUnavailable,
    ProviderCredentialsUnavailable,
    NativeCredentialsUnavailable,
    SaltGenerationFailed,
    StateUpdateFailed,
    ModelLost,
    ModelCapabilityChanged(&'static str),
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PasswordTooShort => {
                f.write_str("migration password must contain at least 12 bytes")
            }
            Self::PasswordTooLong => f.write_str("migration password is too long"),
            Self::TooLarge => f.write_str("migration bundle is too large"),
            Self::NotMigrationFormat => {
                f.write_str("file is not a supported .emp migration bundle")
            }
            Self::InvalidEnvelope => f.write_str("migration envelope is invalid"),
            Self::UnsupportedVersion => f.write_str("unsupported migration bundle"),
            Self::UnsupportedKdf => f.write_str("unsupported migration encryption"),
            Self::InvalidSalt => f.write_str("migration salt is invalid"),
            Self::InvalidPayload => f.write_str("migration bundle field is invalid: payload"),
            Self::DecryptFailed => {
                f.write_str("migration password is incorrect or file is invalid")
            }
            Self::InvalidConfiguration => f.write_str("migration configuration is invalid"),
            Self::InvalidAccounts => f.write_str("migration accounts are invalid"),
            Self::InvalidAccountRecord => f.write_str("migration account record is invalid"),
            Self::DuplicateAccountIds => f.write_str("migration account IDs must be unique"),
            Self::InvalidProviderKeys => f.write_str("migration Provider keys are invalid"),
            Self::UnknownProviderKey => {
                f.write_str("migration contains a key for an unknown Provider")
            }
            Self::InvalidExportGroup => f.write_str("select at least one valid export category"),
            Self::AccountCredentialsUnavailable => {
                f.write_str("account credentials are unavailable")
            }
            Self::ProviderCredentialsUnavailable => {
                f.write_str("Provider credentials are unavailable")
            }
            Self::NativeCredentialsUnavailable => {
                f.write_str("Native login credentials are unavailable or invalid")
            }
            Self::SaltGenerationFailed => f.write_str("migration salt could not be generated"),
            Self::StateUpdateFailed => f.write_str("migration state could not be updated"),
            Self::ModelLost => f.write_str("migration lost an imported model"),
            Self::ModelCapabilityChanged(field) => {
                write!(f, "migration changed model capability data: {field}")
            }
        }
    }
}

impl std::error::Error for MigrationError {}

pub type MigrationResult<T> = Result<T, MigrationError>;

impl From<FilesystemError> for MigrationError {
    fn from(_: FilesystemError) -> Self {
        Self::StateUpdateFailed
    }
}

/// Counts returned after a migration bundle is durably imported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationImportSummary {
    pub accounts: usize,
    pub providers: usize,
    pub models: usize,
    pub renamed_accounts: usize,
    /// IDs of existing local Providers replaced by same-id imported Providers.
    pub overwritten_providers: Vec<String>,
}

/// A byte vector with zeroization on drop.
pub(crate) struct PasswordBytes(Vec<u8>);

impl std::ops::Deref for PasswordBytes {
    type Target = [u8];
    fn deref(&self) -> &[u8] {
        &self.0
    }
}

impl Drop for PasswordBytes {
    fn drop(&mut self) {
        self.0.zeroize();
    }
}

/// Counts returned after an export operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationExportSummary {
    pub accounts: usize,
    pub providers: usize,
    pub models: usize,
    pub groups: Vec<&'static str>,
    pub native_login_included: bool,
    pub native_login_missing: bool,
}

/// Normalize and validate a password for a new export, matching Python's
/// `str.strip()` and UTF-8 byte-length bounds.
pub(crate) fn normalize_password(password: &str) -> MigrationResult<PasswordBytes> {
    normalize_password_with_minimum(password, MIN_PASSWORD_BYTES)
}

/// Normalize and validate a password for decoding an existing bundle, which
/// may predate the stronger export minimum.
pub(crate) fn normalize_import_password(password: &str) -> MigrationResult<PasswordBytes> {
    normalize_password_with_minimum(password, MIN_IMPORT_PASSWORD_BYTES)
}

fn normalize_password_with_minimum(
    password: &str,
    minimum: usize,
) -> MigrationResult<PasswordBytes> {
    let trimmed = password.trim();
    let encoded = trimmed.as_bytes();
    if encoded.len() < minimum {
        return Err(MigrationError::PasswordTooShort);
    }
    if encoded.len() > MAX_PASSWORD_BYTES {
        return Err(MigrationError::PasswordTooLong);
    }
    Ok(PasswordBytes(encoded.to_vec()))
}

mod codec;
mod export;
mod import;

pub use codec::{decode_migration, encode_migration, parse_envelope};
pub use export::{
    ExportGroup, ExportGroups, export_migration_bundle, export_migration_bundle_with_summary,
    select_export_config,
};
use import::id_of;
pub use import::{
    DecryptedMigration, apply_migration_import, decrypt_migration_bundle, import_migration_bundle,
};
