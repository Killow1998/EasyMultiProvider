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

fn verify_model_capability_migration(source: &Value, target: &Value) -> MigrationResult<()> {
    let imported = target
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|model| {
            model
                .get("id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
                .map(|id| (id, model))
        })
        .collect::<BTreeMap<_, _>>();

    for model in source
        .get("models")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = model
            .get("id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or(MigrationError::ModelLost)?;
        let destination = imported.get(id).ok_or(MigrationError::ModelLost)?;
        for field in MODEL_CAPABILITY_MIGRATION_FIELDS {
            let source_value = model.get(field).filter(|value| !value.is_null());
            let destination_value = destination.get(field).filter(|value| !value.is_null());
            if source_value != destination_value {
                return Err(MigrationError::ModelCapabilityChanged(field));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod model_capability_migration_tests {
    use super::{
        MODEL_CAPABILITY_MIGRATION_FIELDS, MigrationError, verify_model_capability_migration,
    };
    use serde_json::json;

    #[test]
    fn verifier_rejects_missing_models_and_each_changed_capability_field() {
        let source = json!({"models": [{"id": "demo/model"}]});
        assert_eq!(
            verify_model_capability_migration(&source, &json!({"models": []})),
            Err(MigrationError::ModelLost)
        );

        for field in MODEL_CAPABILITY_MIGRATION_FIELDS {
            let mut source_model = json!({"id": "demo/model"});
            let mut target_model = json!({"id": "demo/model"});
            source_model
                .as_object_mut()
                .expect("model object")
                .insert(field.to_owned(), json!({"observed": "source"}));
            target_model
                .as_object_mut()
                .expect("model object")
                .insert(field.to_owned(), json!({"observed": "target"}));
            let source = json!({"models": [source_model]});
            let target = json!({"models": [target_model]});
            assert_eq!(
                verify_model_capability_migration(&source, &target),
                Err(MigrationError::ModelCapabilityChanged(field)),
                "capability field {field} must be protected"
            );
        }
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
    verify_model_capability_migration(&source, &target)?;

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

/// One export category from the Python configuration migration format.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum ExportGroup {
    Native,
    Subscriptions,
    External,
}

impl ExportGroup {
    fn from_str(value: &str) -> Option<Self> {
        match value {
            "native" => Some(Self::Native),
            "subscriptions" => Some(Self::Subscriptions),
            "external" => Some(Self::External),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Native => "native",
            Self::Subscriptions => "subscriptions",
            Self::External => "external",
        }
    }
}

/// Categories selected for export; `None` selects all three.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExportGroups {
    native: bool,
    subscriptions: bool,
    external: bool,
}

impl ExportGroups {
    /// Validate an explicit category list without adding missing categories.
    pub fn from_list(groups: &[&str]) -> MigrationResult<Self> {
        let mut selected = Self::all();
        selected.native = false;
        selected.subscriptions = false;
        selected.external = false;
        for group in groups {
            match ExportGroup::from_str(group) {
                Some(ExportGroup::Native) => selected.native = true,
                Some(ExportGroup::Subscriptions) => selected.subscriptions = true,
                Some(ExportGroup::External) => selected.external = true,
                None => return Err(MigrationError::InvalidExportGroup),
            }
        }
        if selected == Self::none() {
            return Err(MigrationError::InvalidExportGroup);
        }
        Ok(selected)
    }

    const fn all() -> Self {
        Self {
            native: true,
            subscriptions: true,
            external: true,
        }
    }

    const fn none() -> Self {
        Self {
            native: false,
            subscriptions: false,
            external: false,
        }
    }

    fn contains(self, group: ExportGroup) -> bool {
        match group {
            ExportGroup::Native => self.native,
            ExportGroup::Subscriptions => self.subscriptions,
            ExportGroup::External => self.external,
        }
    }

    fn as_btree(self) -> BTreeSet<ExportGroup> {
        [
            ExportGroup::Native,
            ExportGroup::Subscriptions,
            ExportGroup::External,
        ]
        .into_iter()
        .filter(|group| self.contains(*group))
        .collect()
    }
}

/// Return configuration values that belong only to the selected categories.
///
/// The Python implementation keys native availability from the local Codex
/// model cache and subscription prefixes from account metadata. Family
/// presentations are intentionally preserved when native or subscription data
/// is selected, even if that cache is absent.
pub fn select_export_config(
    config: &Value,
    groups: Option<&ExportGroups>,
) -> MigrationResult<Value> {
    let selected = groups.copied().unwrap_or_else(ExportGroups::all);
    let mut result = config.clone();
    if selected == ExportGroups::all() {
        return Ok(result);
    }

    let providers = config
        .get("providers")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let provider_groups = providers
        .iter()
        .map(|provider| {
            let id = id_of(provider).to_owned();
            let group = if provider.get("auth_mode").and_then(Value::as_str) == Some("forward") {
                ExportGroup::Native
            } else {
                ExportGroup::External
            };
            (id, group)
        })
        .collect::<BTreeMap<_, _>>();
    let account_prefixes = config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|account| account.get("prefix").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    let models = config
        .get("models")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();
    let model_by_id = models
        .iter()
        .map(|model| (id_of(model).to_owned(), model))
        .collect::<BTreeMap<_, _>>();
    let native_models = if selected.contains(ExportGroup::Native)
        || selected.contains(ExportGroup::Subscriptions)
    {
        load_native_catalog(config)
            .get("models")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    let native_slugs = native_models
        .iter()
        .filter_map(|model| model.get("slug").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();

    let route_group = |route: &str| -> Option<ExportGroup> {
        if let Some(model) = model_by_id.get(route) {
            return model
                .get("provider")
                .and_then(Value::as_str)
                .and_then(|provider| provider_groups.get(provider))
                .copied();
        }
        let prefix = route.split('/').next().unwrap_or(route);
        if account_prefixes.contains(prefix) {
            return Some(ExportGroup::Subscriptions);
        }
        if let Some(group) = provider_groups.get(prefix) {
            return Some(*group);
        }
        if !route.contains('/') || native_slugs.contains(route) {
            return Some(ExportGroup::Native);
        }
        None
    };

    if let Some(object) = result.as_object_mut() {
        if !selected.contains(ExportGroup::Subscriptions) {
            object.insert("accounts".to_owned(), Value::Array(Vec::new()));
        }

        let selected_providers = providers
            .iter()
            .filter(|provider| {
                provider_groups
                    .get(id_of(provider))
                    .is_some_and(|group| selected.contains(*group))
            })
            .cloned()
            .collect::<Vec<_>>();
        object.insert(
            "providers".to_owned(),
            Value::Array(selected_providers.clone()),
        );

        let selected_models = models
            .iter()
            .filter(|model| route_group(id_of(model)).is_some_and(|group| selected.contains(group)))
            .cloned()
            .collect::<Vec<_>>();
        object.insert("models".to_owned(), Value::Array(selected_models.clone()));

        if let Some(Value::Object(presentations)) = config.get("catalog_presentations") {
            let selected_presentations = presentations
                .iter()
                .filter(|(route, _)| {
                    route_group(route.as_str()).is_some_and(|group| selected.contains(group))
                })
                .map(|(route, value)| (route.clone(), value.clone()))
                .collect::<Map<_, _>>();
            object.insert(
                "catalog_presentations".to_owned(),
                Value::Object(selected_presentations),
            );
        }

        let mut families = selected_models
            .iter()
            .map(|model| model_family_identity(model, id_of(model)))
            .collect::<BTreeSet<_>>();
        if selected.contains(ExportGroup::Native) || selected.contains(ExportGroup::Subscriptions) {
            families.extend(native_models.iter().filter_map(|model| {
                if model.is_object() {
                    Some(model_family_identity(
                        model,
                        model
                            .get("slug")
                            .and_then(Value::as_str)
                            .unwrap_or_default(),
                    ))
                } else {
                    None
                }
            }));
            let external_families = models
                .iter()
                .filter(|model| {
                    model
                        .get("provider")
                        .and_then(Value::as_str)
                        .and_then(|provider| provider_groups.get(provider))
                        == Some(&ExportGroup::External)
                })
                .map(|model| model_family_identity(model, id_of(model)))
                .collect::<BTreeSet<_>>();
            if let Some(Value::Object(existing)) = config.get("catalog_family_presentations") {
                families.extend(
                    existing
                        .keys()
                        .filter(|family| !external_families.contains(*family))
                        .cloned(),
                );
            }
        }
        if let Some(Value::Object(existing)) = config.get("catalog_family_presentations") {
            let selected_families = existing
                .iter()
                .filter(|(family, _)| families.contains(family.as_str()))
                .map(|(family, value)| (family.clone(), value.clone()))
                .collect::<Map<_, _>>();
            object.insert(
                "catalog_family_presentations".to_owned(),
                Value::Object(selected_families),
            );
        }

        if !selected.contains(ExportGroup::Native) {
            object.insert("native_hidden_models".to_owned(), Value::Array(Vec::new()));
            object.insert(
                "native_model_context_windows".to_owned(),
                Value::Object(Map::new()),
            );
        }
        if !selected.contains(ExportGroup::Native) && !selected.contains(ExportGroup::Subscriptions)
        {
            object.insert(
                "subscription_search".to_owned(),
                serde_json::json!({"enabled": false, "account_id": ""}),
            );
        }
    } else {
        return Err(MigrationError::InvalidConfiguration);
    }
    Ok(result)
}

fn model_family_identity(model: &Value, fallback: &str) -> String {
    for field in ["family_id", "upstream_id"] {
        if let Some(value) = model.get(field).and_then(Value::as_str) {
            let value = value.trim();
            if !value.is_empty() {
                return value.to_owned();
            }
        }
    }
    fallback.to_owned()
}

fn load_native_catalog(config: &Value) -> Value {
    let configured = config
        .get("native_catalog_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let path = if configured.is_empty() {
        crate::config::expand_home_default_native_catalog_path()
    } else {
        crate::config::expand_home(Path::new(configured))
    };
    let Ok(value) = fs::read(&path) else {
        return serde_json::json!({"models": []});
    };
    if value.len() > MAX_NATIVE_CATALOG_BYTES {
        return serde_json::json!({"models": []});
    }
    let parsed: Value = match serde_json::from_slice(&value) {
        Ok(value) => value,
        Err(_) => return serde_json::json!({"models": []}),
    };
    let valid = parsed.get("models").is_some_and(Value::is_array);
    if !valid {
        return serde_json::json!({"models": []});
    }
    let etag = parsed
        .get("etag")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim_matches('"');
    if etag.starts_with("emp-") {
        let redirected = path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("easy-multi-provider")
            .join("native-catalog.json");
        let redirected = fs::read(&redirected)
            .ok()
            .and_then(|bytes| serde_json::from_slice::<Value>(&bytes).ok())
            .filter(|value| value.get("models").is_some_and(Value::is_array));
        return redirected.unwrap_or_else(|| serde_json::json!({"models": []}));
    }
    parsed
}

fn metadata_without_auth_file(account: &Value) -> Value {
    let mut metadata = normalize_account(account).expect("validated account");
    if let Some(object) = metadata.as_object_mut() {
        object.remove("auth_file");
    }
    metadata
}

fn portable_config(config: &Value) -> Value {
    let mut result = config.clone();
    let Some(object) = result.as_object_mut() else {
        return result;
    };
    object.insert("host".to_owned(), Value::String("127.0.0.1".to_owned()));
    object.insert(
        "native_catalog_path".to_owned(),
        Value::String("~/.codex/models_cache.json".to_owned()),
    );
    object.insert(
        "account_store_path".to_owned(),
        Value::String("state/accounts".to_owned()),
    );
    object.insert(
        "secret_store_path".to_owned(),
        Value::String("state/secrets".to_owned()),
    );
    object.insert("accounts".to_owned(), Value::Array(Vec::new()));
    if let Some(accounts) = config.get("accounts").and_then(Value::as_array) {
        object.insert(
            "accounts".to_owned(),
            Value::Array(
                accounts
                    .iter()
                    .map(metadata_without_auth_file)
                    .collect::<Vec<_>>(),
            ),
        );
    }
    if let Some(providers) = config.get("providers").and_then(Value::as_array) {
        object.insert(
            "providers".to_owned(),
            Value::Array(
                providers
                    .iter()
                    .map(|provider| {
                        let mut provider = provider.clone();
                        if let Some(object) = provider.as_object_mut() {
                            object.remove("api_key_file");
                            object.insert("api_key".to_owned(), Value::String(String::new()));
                        }
                        provider
                    })
                    .collect::<Vec<_>>(),
            ),
        );
    }
    result
}

fn random_salt() -> MigrationResult<[u8; SALT_BYTES]> {
    let mut salt = [0_u8; SALT_BYTES];
    getrandom::getrandom(&mut salt).map_err(|_| MigrationError::SaltGenerationFailed)?;
    Ok(salt)
}

fn native_auth(path: &Path) -> MigrationResult<Option<Value>> {
    if !path.exists() {
        return Ok(None);
    }
    if path.is_symlink() {
        return Err(MigrationError::NativeCredentialsUnavailable);
    }
    let metadata = path
        .metadata()
        .map_err(|_| MigrationError::NativeCredentialsUnavailable)?;
    if !metadata.is_file() || metadata.len() > MAX_NATIVE_AUTH_BYTES as u64 {
        return Err(MigrationError::NativeCredentialsUnavailable);
    }
    let raw = fs::read(path).map_err(|_| MigrationError::NativeCredentialsUnavailable)?;
    let raw = raw.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(&raw);
    let auth: Value =
        serde_json::from_slice(raw).map_err(|_| MigrationError::NativeCredentialsUnavailable)?;
    validate_auth_json(&auth).map_err(|_| MigrationError::NativeCredentialsUnavailable)?;
    Ok(Some(auth))
}

fn provider_key(provider: &Value, vault: &VaultStore) -> Result<String, MigrationError> {
    let Some(object) = provider.as_object() else {
        return Ok(String::new());
    };
    if let Some(value) = object.get("api_key").and_then(Value::as_str)
        && !value.is_empty()
    {
        return Ok(value.to_owned());
    }
    let Some(path) = object.get("api_key_file").and_then(Value::as_str) else {
        return Ok(String::new());
    };
    if path.is_empty() {
        return Ok(String::new());
    }
    if Path::new(path).is_symlink() {
        return Ok(String::new());
    }
    let key = vault
        .read_encrypted_text(Path::new(path))
        .map_err(|_| MigrationError::ProviderCredentialsUnavailable)?;
    Ok(key.trim().to_owned())
}

fn native_account_id(config: &Value) -> String {
    let mut reserved = config
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| provider.get("id").and_then(Value::as_str))
        .map(str::to_owned)
        .collect::<BTreeSet<_>>();
    for account in config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        if let Some(id) = account.get("id").and_then(Value::as_str) {
            reserved.insert(id.to_owned());
        }
        if let Some(prefix) = account.get("prefix").and_then(Value::as_str) {
            reserved.insert(prefix.to_owned());
        }
    }
    let mut suffix = 2;
    let mut candidate = "native-login".to_owned();
    while reserved.contains(&candidate) {
        candidate = format!("native-login-{suffix}");
        suffix += 1;
    }
    candidate
}

/// Return a Python-compatible encrypted migration payload and summary.
pub fn export_migration_bundle_with_summary(
    config: &Value,
    password: &str,
    vault: &VaultStore,
    groups: Option<&ExportGroups>,
    native_auth_path: Option<&Path>,
) -> MigrationResult<(Vec<u8>, MigrationExportSummary)> {
    normalize_password(password)?;
    let selected = groups.copied().unwrap_or_else(ExportGroups::all);
    let selected_list = selected.as_btree();
    let mut config = select_export_config(config, groups)?;

    let mut accounts = Vec::new();
    for raw in config
        .get("accounts")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let metadata =
            normalize_account(raw).map_err(|_| MigrationError::AccountCredentialsUnavailable)?;
        let auth_file = metadata
            .get("auth_file")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        if auth_file.is_empty() {
            return Err(MigrationError::AccountCredentialsUnavailable);
        }
        if Path::new(&auth_file).is_symlink() {
            return Err(MigrationError::AccountCredentialsUnavailable);
        }
        let metadata = metadata_without_auth_file(raw);
        let auth = vault
            .read_encrypted_json(Path::new(&auth_file))
            .map_err(|_| MigrationError::AccountCredentialsUnavailable)?;
        validate_auth_json(&auth).map_err(|_| MigrationError::AccountCredentialsUnavailable)?;
        accounts.push(serde_json::json!({"metadata": metadata, "auth": auth}));
    }

    let mut native_included = false;
    if let Some(path) = native_auth_path
        && selected.contains(ExportGroup::Native)
        && let Some(auth) = native_auth(path)?
    {
        native_included = true;
        let account_id = native_account_id(&config);
        let raw_account = serde_json::json!({
            "id": account_id,
            "prefix": account_id,
            "name": "Native login",
            "hidden_models": config.get("native_hidden_models").cloned().unwrap_or_default(),
            "model_context_windows": config.get("native_model_context_windows").cloned().unwrap_or_default(),
        });
        let account = normalize_account(&raw_account)
            .map_err(|_| MigrationError::AccountCredentialsUnavailable)?;
        let mut accounts_array = config
            .get("accounts")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        accounts_array.push(account.clone());
        if let Some(object) = config.as_object_mut() {
            object.insert("accounts".to_owned(), Value::Array(accounts_array));
        }
        let metadata = metadata_without_auth_file(&account);
        accounts.push(serde_json::json!({"metadata": metadata, "auth": auth}));
    }

    let mut provider_keys = Map::new();
    for provider in config
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
    {
        let id = id_of(provider);
        let value = provider_key(provider, vault)?;
        let has_file = provider
            .get("api_key_file")
            .and_then(Value::as_str)
            .is_some_and(|value| !value.is_empty());
        if has_file && value.is_empty() {
            return Err(MigrationError::ProviderCredentialsUnavailable);
        }
        if !value.is_empty() {
            provider_keys.insert(id.to_owned(), Value::String(value));
        }
    }

    let payload = serde_json::json!({
        "schema": MIGRATION_SCHEMA,
        "version": MIGRATION_VERSION,
        "config": portable_config(&config),
        "accounts": accounts,
        "provider_keys": Value::Object(provider_keys),
    });
    let plaintext =
        serde_json::to_vec(&payload).map_err(|_| MigrationError::InvalidConfiguration)?;
    let salt = random_salt()?;
    let bundle = encode_migration(password, &salt, &plaintext)?;
    let mut group_names = selected_list
        .into_iter()
        .map(ExportGroup::as_str)
        .collect::<Vec<_>>();
    group_names.sort_unstable();
    let summary = MigrationExportSummary {
        accounts: accounts.len(),
        providers: config
            .get("providers")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        models: config
            .get("models")
            .and_then(Value::as_array)
            .map_or(0, Vec::len),
        groups: group_names,
        native_login_included: native_included,
        native_login_missing: selected.contains(ExportGroup::Native) && !native_included,
    };
    Ok((bundle, summary))
}

/// Return a Python-compatible encrypted migration bundle.
pub fn export_migration_bundle(
    config: &Value,
    password: &str,
    vault: &VaultStore,
    groups: Option<&ExportGroups>,
    native_auth_path: Option<&Path>,
) -> MigrationResult<Vec<u8>> {
    Ok(export_migration_bundle_with_summary(config, password, vault, groups, native_auth_path)?.0)
}
