//! `.emp` migration envelope encoding and decoding, compatible with the
//! Python implementation's `EMP-MIGRATION\x01\n` + scrypt/Fernet format.

use crate::accounts::{
    account_auth_path, normalize_account, same_account_auth, validate_auth_json,
};
use crate::config::{
    load_configuration, normalize_configuration, save_configuration_in_transaction,
};
use crate::fernet::{Fernet, FernetKey};
use crate::filesystem::{FilesystemError, VaultStore, with_file_transaction};
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
/// Schema version expected inside the JSON envelope.
pub const MIGRATION_VERSION: u64 = 1;
/// Maximum encoded migration bundle size in bytes.
pub const MAX_BUNDLE_BYTES: usize = 32 * 1024 * 1024;
/// Minimum password length after trimming, in UTF-8 bytes.
pub const MIN_PASSWORD_BYTES: usize = 8;
/// Maximum password length after trimming, in UTF-8 bytes.
pub const MAX_PASSWORD_BYTES: usize = 4096;
/// Required salt length in bytes.
pub const SALT_BYTES: usize = 16;
/// Fixed scrypt N parameter.
pub const SCRYPT_N: u64 = 16384;
/// Fixed scrypt r parameter.
pub const SCRYPT_R: u64 = 8;
/// Fixed scrypt p parameter.
pub const SCRYPT_P: u64 = 1;

/// Fixed KDF parameters (attacker input cannot select cost).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationParams {
    pub n: u64,
    pub r: u64,
    pub p: u64,
}

impl Default for MigrationParams {
    fn default() -> Self {
        MigrationParams {
            n: SCRYPT_N,
            r: SCRYPT_R,
            p: SCRYPT_P,
        }
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
    StateUpdateFailed,
}

impl fmt::Display for MigrationError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::PasswordTooShort => {
                f.write_str("migration password must contain at least 8 bytes")
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
            Self::StateUpdateFailed => f.write_str("migration state could not be updated"),
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
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct MigrationImportSummary {
    pub accounts: usize,
    pub providers: usize,
    pub models: usize,
    pub renamed_accounts: usize,
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

/// Normalize and validate a password, matching Python's `str.strip()` and
/// UTF-8 byte-length bounds.
pub(crate) fn normalize_password(password: &str) -> MigrationResult<PasswordBytes> {
    let trimmed = password.trim();
    let encoded = trimmed.as_bytes();
    if encoded.len() < MIN_PASSWORD_BYTES {
        return Err(MigrationError::PasswordTooShort);
    }
    if encoded.len() > MAX_PASSWORD_BYTES {
        return Err(MigrationError::PasswordTooLong);
    }
    Ok(PasswordBytes(encoded.to_vec()))
}

/// Derive a Fernet key from a password and salt using fixed scrypt parameters.
/// Returns a padded URL-safe base64 string, matching Python's Fernet key format.
fn derive_fernet_encoded_key(password: &[u8], salt: &[u8]) -> FernetKey {
    let params = Params::new(14, 8, 1).expect("valid scrypt params");
    let mut derived = [0u8; 32];
    scrypt(password, salt, &params, &mut derived).expect("32-byte output is valid");
    let encoded = Zeroizing::new(URL_SAFE.encode(derived));
    derived.zeroize();
    FernetKey::from_encoded(&encoded).expect("derived key is valid Fernet")
}

/// Build the JSON envelope with the given salt and Fernet-encrypted payload.
fn build_envelope(salt: &[u8; 16], encrypted_payload: &[u8]) -> Map<String, Value> {
    let mut scrypt_obj = Map::new();
    scrypt_obj.insert("n".to_string(), Value::Number(SCRYPT_N.into()));
    scrypt_obj.insert("r".to_string(), Value::Number(SCRYPT_R.into()));
    scrypt_obj.insert("p".to_string(), Value::Number(SCRYPT_P.into()));

    let mut envelope = Map::new();
    envelope.insert(
        "schema".to_string(),
        Value::String(MIGRATION_SCHEMA.to_string()),
    );
    envelope.insert(
        "version".to_string(),
        Value::Number(MIGRATION_VERSION.into()),
    );
    envelope.insert("kdf".to_string(), Value::String("scrypt".to_string()));
    envelope.insert("scrypt".to_string(), Value::Object(scrypt_obj));
    envelope.insert("salt".to_string(), Value::String(URL_SAFE.encode(salt)));
    envelope.insert(
        "payload".to_string(),
        Value::String(URL_SAFE.encode(encrypted_payload)),
    );
    envelope
}

/// Encode a plaintext payload into a `.emp` migration bundle.
pub fn encode_migration(
    password: &str,
    salt: &[u8; 16],
    plaintext: &[u8],
) -> MigrationResult<Vec<u8>> {
    if plaintext.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    let password = normalize_password(password)?;
    let fernet_key = derive_fernet_encoded_key(&password, salt);
    let fernet = Fernet::new(&fernet_key);
    let encrypted = fernet.encrypt(plaintext);
    let envelope = build_envelope(salt, &encrypted);
    let envelope_json = serde_json::to_vec(&Value::Object(envelope)).expect("serializable JSON");

    let mut output = Vec::with_capacity(MIGRATION_MAGIC.len() + envelope_json.len() + 1);
    output.extend_from_slice(MIGRATION_MAGIC);
    output.extend_from_slice(&envelope_json);
    output.push(b'\n');

    if output.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    Ok(output)
}

/// Decode a `.emp` migration bundle and return the decrypted plaintext.
pub fn decode_migration(password: &str, encoded: &[u8]) -> MigrationResult<Zeroizing<Vec<u8>>> {
    if encoded.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    if !encoded.starts_with(MIGRATION_MAGIC) {
        return Err(MigrationError::NotMigrationFormat);
    }

    let body = &encoded[MIGRATION_MAGIC.len()..];
    let body = if body.last() == Some(&b'\n') {
        &body[..body.len() - 1]
    } else {
        body
    };

    let envelope: Value =
        serde_json::from_slice(body).map_err(|_| MigrationError::InvalidEnvelope)?;
    let envelope = envelope
        .as_object()
        .ok_or(MigrationError::InvalidEnvelope)?;

    let schema = envelope.get("schema").and_then(Value::as_str).unwrap_or("");
    if schema != MIGRATION_SCHEMA {
        return Err(MigrationError::UnsupportedVersion);
    }
    let version = envelope.get("version").and_then(Value::as_u64).unwrap_or(0);
    if version != MIGRATION_VERSION {
        return Err(MigrationError::UnsupportedVersion);
    }

    let kdf = envelope.get("kdf").and_then(Value::as_str).unwrap_or("");
    if kdf != "scrypt" {
        return Err(MigrationError::UnsupportedKdf);
    }
    let params = envelope
        .get("scrypt")
        .and_then(Value::as_object)
        .ok_or(MigrationError::UnsupportedKdf)?;
    let n = params.get("n").and_then(Value::as_u64).unwrap_or(0);
    let r = params.get("r").and_then(Value::as_u64).unwrap_or(0);
    let p = params.get("p").and_then(Value::as_u64).unwrap_or(0);
    if n != SCRYPT_N || r != SCRYPT_R || p != SCRYPT_P {
        return Err(MigrationError::UnsupportedKdf);
    }

    let salt_str = envelope
        .get("salt")
        .and_then(Value::as_str)
        .ok_or(MigrationError::InvalidSalt)?;
    let salt = URL_SAFE
        .decode(salt_str.as_bytes())
        .map_err(|_| MigrationError::InvalidSalt)?;
    if salt.len() != SALT_BYTES {
        return Err(MigrationError::InvalidSalt);
    }

    let payload_str = envelope
        .get("payload")
        .and_then(Value::as_str)
        .ok_or(MigrationError::InvalidPayload)?;
    let encrypted = URL_SAFE
        .decode(payload_str.as_bytes())
        .map_err(|_| MigrationError::InvalidPayload)?;

    let password = normalize_password(password)?;
    let fernet_key = derive_fernet_encoded_key(&password, &salt);
    let fernet = Fernet::new(&fernet_key);
    let plaintext = fernet
        .decrypt(&encrypted)
        .map_err(|_| MigrationError::DecryptFailed)?;
    Ok(plaintext)
}

struct ValidatedImport {
    source: Value,
    accounts: Vec<(Value, Value)>,
    provider_keys: Map<String, Value>,
}

fn validate_import_payload(payload: &Value) -> MigrationResult<ValidatedImport> {
    let payload = payload
        .as_object()
        .ok_or(MigrationError::UnsupportedVersion)?;
    if payload.get("schema").and_then(Value::as_str) != Some(MIGRATION_SCHEMA)
        || payload.get("version").and_then(Value::as_u64) != Some(MIGRATION_VERSION)
    {
        return Err(MigrationError::UnsupportedVersion);
    }

    let raw_config = payload
        .get("config")
        .filter(|value| value.is_object())
        .ok_or(MigrationError::InvalidConfiguration)?;
    let source = normalize_configuration(Some(raw_config))
        .map_err(|_| MigrationError::InvalidConfiguration)?;

    let records = payload
        .get("accounts")
        .and_then(Value::as_array)
        .ok_or(MigrationError::InvalidAccounts)?;
    let mut accounts = Vec::with_capacity(records.len());
    let mut account_ids = BTreeSet::new();
    for record in records {
        let record = record
            .as_object()
            .ok_or(MigrationError::InvalidAccountRecord)?;
        let metadata = normalize_account(record.get("metadata").unwrap_or(&Value::Null))
            .map_err(|_| MigrationError::InvalidAccountRecord)?;
        let auth = validate_auth_json(record.get("auth").unwrap_or(&Value::Null))
            .map_err(|_| MigrationError::InvalidAccountRecord)?;
        let id = metadata
            .get("id")
            .and_then(Value::as_str)
            .ok_or(MigrationError::InvalidAccountRecord)?;
        if !account_ids.insert(id.to_owned()) {
            return Err(MigrationError::DuplicateAccountIds);
        }
        accounts.push((metadata, auth));
    }

    let provider_keys = match payload.get("provider_keys") {
        None => Map::new(),
        Some(Value::Object(values)) => values.clone(),
        Some(_) => return Err(MigrationError::InvalidProviderKeys),
    };
    if provider_keys.values().any(|value| !value.is_string()) {
        return Err(MigrationError::InvalidProviderKeys);
    }
    let provider_ids = source
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| provider.get("id").and_then(Value::as_str))
        .collect::<BTreeSet<_>>();
    if provider_keys
        .keys()
        .any(|provider_id| !provider_ids.contains(provider_id.as_str()))
    {
        return Err(MigrationError::UnknownProviderKey);
    }

    Ok(ValidatedImport {
        source,
        accounts,
        provider_keys,
    })
}

fn id_of(value: &Value) -> &str {
    value.get("id").and_then(Value::as_str).unwrap_or("")
}

fn prefix_of(value: &Value) -> &str {
    value.get("prefix").and_then(Value::as_str).unwrap_or("")
}

fn set_string(value: &mut Value, field: &str, replacement: impl Into<String>) {
    if let Some(object) = value.as_object_mut() {
        object.insert(field.to_owned(), Value::String(replacement.into()));
    }
}

fn replace_or_push(values: &mut Vec<Value>, value: Value) {
    let id = id_of(&value);
    if let Some(index) = values.iter().position(|item| id_of(item) == id) {
        values[index] = value;
    } else {
        values.push(value);
    }
}

fn unique_segment(value: &str, reserved: &mut BTreeSet<String>) -> String {
    for suffix in 2_u64.. {
        let ending = format!("-{suffix}");
        let prefix_bytes = 64_usize.saturating_sub(ending.len());
        let prefix = value.get(..prefix_bytes).unwrap_or(value);
        let candidate = format!("{prefix}{ending}");
        if reserved.insert(candidate.clone()) {
            return candidate;
        }
    }
    unreachable!("an unbounded numeric suffix must yield a unique segment")
}

fn stored_account_auth(account: &Value, vault: &VaultStore) -> Option<Value> {
    let path = PathBuf::from(account.get("auth_file")?.as_str()?);
    let metadata = fs::symlink_metadata(&path).ok()?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return None;
    }
    let auth = vault.read_encrypted_json(&path).ok()?;
    validate_auth_json(&auth).ok()
}

fn merge_import(
    current: &Value,
    payload: ValidatedImport,
    config_path: &Path,
    vault: &VaultStore,
) -> MigrationResult<(Value, BTreeMap<String, Value>, MigrationImportSummary)> {
    let ValidatedImport {
        source,
        accounts: source_accounts,
        provider_keys,
    } = payload;
    let source_provider_count = source
        .get("providers")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let source_model_count = source
        .get("models")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    let mut target = current.clone();

    let mut providers = target
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for raw in source
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let mut provider = raw.clone();
        let id = id_of(&provider).to_owned();
        let key = provider_keys.get(&id).and_then(Value::as_str).unwrap_or("");
        set_string(&mut provider, "api_key", key);
        set_string(&mut provider, "api_key_file", "");
        replace_or_push(&mut providers, provider);
    }
    target
        .as_object_mut()
        .ok_or(MigrationError::StateUpdateFailed)?
        .insert("providers".to_owned(), Value::Array(providers.clone()));

    let mut models = target
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    for model in source
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        replace_or_push(&mut models, model.clone());
    }
    target
        .as_object_mut()
        .expect("target object checked")
        .insert("models".to_owned(), Value::Array(models));

    let mut family_presentations = target
        .get("catalog_family_presentations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(source_presentations) = source
        .get("catalog_family_presentations")
        .and_then(Value::as_object)
    {
        family_presentations.extend(source_presentations.clone());
    }
    target
        .as_object_mut()
        .expect("target object checked")
        .insert(
            "catalog_family_presentations".to_owned(),
            Value::Object(family_presentations),
        );

    let native_hidden_models = target
        .get("native_hidden_models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .chain(
            source
                .get("native_hidden_models")
                .and_then(Value::as_array)
                .into_iter()
                .flatten(),
        )
        .filter_map(Value::as_str)
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    target
        .as_object_mut()
        .expect("target object checked")
        .insert(
            "native_hidden_models".to_owned(),
            Value::Array(
                native_hidden_models
                    .iter()
                    .cloned()
                    .map(Value::String)
                    .collect(),
            ),
        );
    let mut accounts = target
        .get("accounts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let mut reserved = providers
        .iter()
        .map(id_of)
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for account in accounts
        .iter()
        .chain(source_accounts.iter().map(|(metadata, _)| metadata))
    {
        reserved.insert(id_of(account).to_owned());
        reserved.insert(prefix_of(account).to_owned());
    }

    let mut imported_auth = BTreeMap::new();
    let mut prefix_map = BTreeMap::new();
    let mut renamed_accounts = 0_usize;
    for (mut metadata, auth) in source_accounts {
        let source_prefix = prefix_of(&metadata).to_owned();
        let original_id = id_of(&metadata).to_owned();
        let original_existing = accounts
            .iter()
            .position(|account| id_of(account) == original_id);
        let mut same_index = None;
        if let Some(existing_index) = original_existing {
            let candidates = std::iter::once(existing_index)
                .chain((0..accounts.len()).filter(|index| *index != existing_index));
            for candidate in candidates {
                let Some(existing_auth) = stored_account_auth(&accounts[candidate], vault) else {
                    continue;
                };
                if same_account_auth(&existing_auth, &auth) {
                    same_index = Some(candidate);
                    break;
                }
            }
            if let Some(index) = same_index {
                set_string(&mut metadata, "id", id_of(&accounts[index]));
                set_string(&mut metadata, "prefix", prefix_of(&accounts[index]));
            } else {
                let renamed = unique_segment(&original_id, &mut reserved);
                set_string(&mut metadata, "id", &renamed);
                set_string(&mut metadata, "prefix", renamed);
                renamed_accounts += 1;
            }
        }

        if same_index.is_none() {
            let occupied_prefixes = accounts
                .iter()
                .map(prefix_of)
                .chain(providers.iter().map(id_of))
                .collect::<BTreeSet<_>>();
            let prefix = prefix_of(&metadata).to_owned();
            if occupied_prefixes.contains(prefix.as_str()) {
                let renamed = unique_segment(&prefix, &mut reserved);
                set_string(&mut metadata, "prefix", renamed);
                if original_existing.is_none() {
                    renamed_accounts += 1;
                }
            }
        }

        let imported_id = id_of(&metadata).to_owned();
        let imported_prefix = prefix_of(&metadata).to_owned();
        reserved.insert(imported_id.clone());
        reserved.insert(imported_prefix.clone());
        prefix_map.insert(source_prefix, imported_prefix);
        imported_auth.insert(imported_id.clone(), auth);
        set_string(&mut metadata, "auth_file", "");
        if let Some(index) = same_index {
            accounts[index] = metadata;
        } else {
            replace_or_push(&mut accounts, metadata);
        }
    }
    target
        .as_object_mut()
        .expect("target object checked")
        .insert("accounts".to_owned(), Value::Array(accounts));

    let mut presentations = target
        .get("catalog_presentations")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    if let Some(source_presentations) = source
        .get("catalog_presentations")
        .and_then(Value::as_object)
    {
        for (route, presentation) in source_presentations {
            let destination = match route.split_once('/') {
                Some((prefix, slug)) => format!(
                    "{}/{}",
                    prefix_map.get(prefix).map_or(prefix, String::as_str),
                    slug
                ),
                None => route.clone(),
            };
            presentations.insert(destination, presentation.clone());
        }
    }
    target
        .as_object_mut()
        .expect("target object checked")
        .insert(
            "catalog_presentations".to_owned(),
            Value::Object(presentations),
        );

    let mut target =
        normalize_configuration(Some(&target)).map_err(|_| MigrationError::StateUpdateFailed)?;
    let path_config = target.clone();
    if let Some(accounts) = target.get_mut("accounts").and_then(Value::as_array_mut) {
        for account in accounts {
            let id = id_of(account).to_owned();
            if imported_auth.contains_key(&id) {
                let path = account_auth_path(&path_config, &id, config_path)
                    .map_err(|_| MigrationError::StateUpdateFailed)?;
                set_string(account, "auth_file", path.to_string_lossy());
            }
        }
    }
    target =
        normalize_configuration(Some(&target)).map_err(|_| MigrationError::StateUpdateFailed)?;

    let summary = MigrationImportSummary {
        accounts: imported_auth.len(),
        providers: source_provider_count,
        models: source_model_count,
        renamed_accounts,
    };
    Ok((target, imported_auth, summary))
}

/// Decrypt, validate, merge and durably import a Python-compatible `.emp` bundle.
pub fn import_migration_bundle(
    current: &Value,
    bundle: &[u8],
    password: &str,
    config_path: &Path,
    vault: &VaultStore,
) -> MigrationResult<(Value, MigrationImportSummary)> {
    let plaintext = decode_migration(password, bundle)?;
    let payload: Value =
        serde_json::from_slice(&plaintext).map_err(|_| MigrationError::DecryptFailed)?;
    let payload = validate_import_payload(&payload)?;
    let (target, imported_auth, summary) = merge_import(current, payload, config_path, vault)?;

    let result = with_file_transaction(|transaction| {
        for (account_id, auth) in &imported_auth {
            let path = account_auth_path(&target, account_id, config_path)
                .map_err(|_| MigrationError::StateUpdateFailed)?;
            transaction.remember(&path)?;
            vault.write_encrypted_json(&path, auth)?;
        }
        save_configuration_in_transaction(&target, Some(config_path), vault, transaction)
            .map_err(|_| MigrationError::StateUpdateFailed)?;
        load_configuration(Some(config_path)).map_err(|_| MigrationError::StateUpdateFailed)
    })?;
    Ok((result, summary))
}

/// Parse a migration bundle's JSON envelope without decryption.
pub fn parse_envelope(encoded: &[u8]) -> MigrationResult<Map<String, Value>> {
    if encoded.len() > MAX_BUNDLE_BYTES {
        return Err(MigrationError::TooLarge);
    }
    if !encoded.starts_with(MIGRATION_MAGIC) {
        return Err(MigrationError::NotMigrationFormat);
    }
    let body = &encoded[MIGRATION_MAGIC.len()..];
    let body = if body.last() == Some(&b'\n') {
        &body[..body.len() - 1]
    } else {
        body
    };
    let envelope: Value =
        serde_json::from_slice(body).map_err(|_| MigrationError::InvalidEnvelope)?;
    envelope
        .as_object()
        .cloned()
        .ok_or(MigrationError::InvalidEnvelope)
}
