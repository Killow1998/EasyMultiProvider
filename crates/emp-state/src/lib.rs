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

pub mod accounts;
pub mod config;
pub mod fernet;
pub mod filesystem;
pub mod lock;
pub mod migration;
pub mod model_values;
#[cfg(windows)]
mod private_windows;
pub mod vault;
pub mod web_session;

pub use accounts::{
    AccountError, AccountResult, account_auth_path, normalize_account, normalize_context_windows,
    normalize_hidden_models, same_account_auth, validate_auth_json,
};
pub use config::{
    CONFIG_PATH_ENV, ConfigError, ConfigResult, canonical_catalog_json, canonicalize_account_paths,
    canonicalize_private_paths, catalog_etag, config_path, load_configuration, merge_web_update,
    merge_web_update_with_time, normalize_catalog_presentations, normalize_codex_runtime_sources,
    normalize_configuration, normalize_context_calibrations, normalize_model,
    normalize_model_capability_sources, normalize_provider, normalize_provider_base_url,
    normalize_provider_id, normalize_subscription_search, public_configuration_with_file_status,
    save_configuration, save_configuration_in_transaction,
};
pub use fernet::{Fernet, FernetError, FernetKey, FernetKeyError};
pub use filesystem::{
    FileTransaction, FilesystemError, MASTER_KEY_ENV, MASTER_KEY_FILE_ENV,
    MAX_TRANSACTION_FILE_BYTES, VaultStore, with_file_transaction,
};
pub use lock::{IntegrationFileLock, LockError};
pub use migration::{
    ExportGroup, ExportGroups, MAX_BUNDLE_BYTES, MAX_PASSWORD_BYTES, MIGRATION_MAGIC,
    MIGRATION_SCHEMA, MIGRATION_VERSION, MIN_PASSWORD_BYTES, MigrationError,
    MigrationExportSummary, MigrationImportSummary, MigrationParams, MigrationResult, SALT_BYTES,
    SCRYPT_N, SCRYPT_P, SCRYPT_R, decode_migration, encode_migration, export_migration_bundle,
    export_migration_bundle_with_summary, import_migration_bundle, parse_envelope,
    select_export_config,
};
pub use model_values::{
    IMAGE_MODALITY, MAX_MODALITIES, MAX_MODALITY_ID_BYTES, TEXT_MODALITY, codex_input_modalities,
    input_modalities_known, input_modalities_metadata_source, normalize_input_modalities,
    normalize_output_modalities, normalize_reasoning_levels, normalize_supported_protocols,
    output_modalities_known, output_modalities_metadata_source, supported_protocols_known,
};
pub use vault::{VAULT_MAGIC, VaultError, VaultResult, decode_vault, encode_vault};
pub use web_session::{
    WEB_SESSION_LIFETIME_SECONDS, WEB_SESSION_TOKEN_BYTES, WEB_SESSION_TOKEN_LENGTH, WebSession,
    WebSessionError, load_or_create_web_session, web_session_path,
};
