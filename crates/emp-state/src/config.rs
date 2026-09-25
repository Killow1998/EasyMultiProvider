//! Exact, bounded configuration helpers shared with the Python implementation.
//!
//! Pure account, provider, model and top-level configuration normalization is
//! implemented here. Filesystem canonicalization, import merging and persistence
//! remain separate state transitions so callers cannot mistake parsing for I/O.

use crate::accounts::{normalize_account, normalize_context_windows, normalize_hidden_models};
use crate::filesystem::{
    FileTransaction, FilesystemError, VaultStore, atomic_write_config, with_file_transaction,
};
use crate::model_values::{
    input_modalities_known, normalize_input_modalities, normalize_output_modalities,
    normalize_reasoning_levels, normalize_supported_protocols, output_modalities_known,
    supported_protocols_known,
};
use emp_core::{deployment_identity, endpoint_fingerprint};
use serde_json::{Map, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fmt;
use std::fs;
use std::path::{Path, PathBuf};

pub const CONFIG_PATH_ENV: &str = "EASY_MULTI_PROVIDER_CONFIG";
const REASONING_SUMMARIES: [&str; 3] = ["auto", "show", "hide"];
const MASKED_API_KEY: &str = "\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}\u{2022}";
/// A stable Python-visible configuration failure without private input data.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigError {
    message: String,
    python_type: &'static str,
}

impl ConfigError {
    pub(crate) fn new(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            python_type: "ConfigError",
        }
    }

    fn python(python_type: &'static str, message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            python_type,
        }
    }

    /// Exception class produced by the Python compatibility oracle.
    pub fn python_type(&self) -> &'static str {
        self.python_type
    }
}

impl fmt::Display for ConfigError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for ConfigError {}

impl From<FilesystemError> for ConfigError {
    fn from(error: FilesystemError) -> Self {
        Self::new(error.to_string())
    }
}

pub type ConfigResult<T> = Result<T, ConfigError>;

mod presentation;
pub use presentation::{
    normalize_catalog_presentations, normalize_codex_runtime_sources,
    normalize_subscription_search, public_configuration_with_file_status,
};

mod catalog_etag;
pub use catalog_etag::{canonical_catalog_json, catalog_etag};

mod provider_url;
use provider_url::{hostname, parse_provider_url, string_value, userinfo};
pub use provider_url::{normalize_provider_base_url, normalize_provider_id};

mod context_calibration;
pub use context_calibration::normalize_context_calibrations;
use context_calibration::{
    MAX_CONTEXT_WINDOW, model_python_int_or_zero, normalize_created_at, python_int,
    python_int_conversion_error,
};

mod model_provider;
use model_provider::{
    MODEL_BOOLEAN_CAPABILITIES, TOP_LEVEL_PROVENANCE_FIELDS, json_truthy, python_capability_trim,
    safe_capability_identity, valid_python_iso_timestamp,
};
pub use model_provider::{normalize_model, normalize_model_capability_sources, normalize_provider};

const DEFAULT_HOST: &str = "127.0.0.1";
const DEFAULT_PORT: i64 = 4200;
const DEFAULT_CODEX_BASE_URL: &str = "https://chatgpt.com/backend-api/codex";

fn default_native_catalog_path() -> String {
    let home = if cfg!(windows) {
        env::var_os("USERPROFILE").or_else(|| env::var_os("HOME"))
    } else {
        env::var_os("HOME").or_else(|| env::var_os("USERPROFILE"))
    };
    let mut path = home.map_or_else(PathBuf::new, PathBuf::from);
    path.push(".codex");
    path.push("models_cache.json");
    path.to_string_lossy().into_owned()
}

fn default_configuration() -> Value {
    serde_json::json!({
        "host": DEFAULT_HOST,
        "port": DEFAULT_PORT,
        "native_catalog_path": default_native_catalog_path(),
        "account_store_path": "state/accounts",
        "secret_store_path": "state/secrets",
        "codex_base_url": DEFAULT_CODEX_BASE_URL,
        "accounts": [],
        "providers": [],
        "models": [],
        "native_hidden_models": [],
        "native_model_context_windows": {},
        "catalog_presentations": {},
        "catalog_family_presentations": {},
        "subscription_search": {"enabled": false, "account_id": ""},
        "codex_runtime_sources": ["auto"],
    })
}

fn configuration_string(raw: Option<&Value>, default: &str, field: &str) -> ConfigResult<String> {
    match raw {
        None => Ok(default.to_owned()),
        value => string_value(value, field, false),
    }
}

fn validate_codex_base_url(raw: Option<&Value>) -> ConfigResult<String> {
    let value = configuration_string(raw, DEFAULT_CODEX_BASE_URL, "codex_base_url")?;
    if value.is_empty() {
        return Err(ConfigError::new("codex_base_url is required"));
    }
    let value = value.trim_end_matches('/');
    let parsed = parse_provider_url(value)
        .ok_or_else(|| ConfigError::new("codex_base_url must be an http(s) URL"))?;
    if !matches!(parsed.scheme.as_str(), "http" | "https") || parsed.netloc.is_empty() {
        return Err(ConfigError::new("codex_base_url must be an http(s) URL"));
    }
    let (username, password) = userinfo(parsed.netloc);
    if !username.is_empty() || password.is_some_and(|password| !password.is_empty()) {
        return Err(ConfigError::new(
            "codex_base_url must not contain URL credentials",
        ));
    }
    if !parsed.query.is_empty() || !parsed.fragment.is_empty() {
        return Err(ConfigError::new(
            "codex_base_url must not contain a query or fragment",
        ));
    }
    let host = hostname(parsed.netloc).unwrap_or("").to_ascii_lowercase();
    let loopback = host == "localhost"
        || host
            .parse::<std::net::IpAddr>()
            .is_ok_and(|address| address.is_loopback());
    if parsed.scheme == "http" && !loopback {
        return Err(ConfigError::new(
            "codex_base_url must use HTTPS unless it targets loopback",
        ));
    }
    Ok(value.to_owned())
}

fn python_iterable(raw: Option<&Value>) -> ConfigResult<Vec<Value>> {
    let Some(raw) = raw else {
        return Ok(Vec::new());
    };
    match raw {
        Value::Array(values) => Ok(values.clone()),
        Value::String(value) => Ok(value
            .chars()
            .map(|character| Value::String(character.to_string()))
            .collect()),
        Value::Object(values) => Ok(values.keys().cloned().map(Value::String).collect()),
        Value::Null => Err(ConfigError::python(
            "TypeError",
            "'NoneType' object is not iterable",
        )),
        Value::Bool(_) => Err(ConfigError::python(
            "TypeError",
            "'bool' object is not iterable",
        )),
        Value::Number(value) if value.is_i64() || value.is_u64() => Err(ConfigError::python(
            "TypeError",
            "'int' object is not iterable",
        )),
        Value::Number(_) => Err(ConfigError::python(
            "TypeError",
            "'float' object is not iterable",
        )),
    }
}

fn account_error(error: impl fmt::Display) -> ConfigError {
    ConfigError::python("AccountError", error.to_string())
}

mod paths;
pub(crate) use paths::{
    account_root, expand_home, expand_home_default_native_catalog_path, path_python_resolve,
};
use paths::{canonical_secret_root, percent_encode};
pub use paths::{
    canonicalize_account_paths, canonicalize_private_paths, config_path, generated_catalog_path,
    resolve_user_path,
};

mod merge;
pub use merge::{
    merge_web_update, merge_web_update_with_time, observed_at_now, remember_resolved_protocol,
    remember_resolved_protocol_at,
};

/// Save a normalized configuration and its derived secret files atomically.
pub fn save_configuration(
    config: &Value,
    path: Option<&Path>,
    vault: &VaultStore,
) -> ConfigResult<PathBuf> {
    with_file_transaction(|transaction| {
        save_configuration_in_transaction(config, path, vault, transaction)
    })
}

/// Add a configuration save to a caller-owned transaction.
///
/// This is used by larger state changes such as migration imports. The caller
/// owns commit and rollback; this function never ends the transaction early.
pub fn save_configuration_in_transaction(
    config: &Value,
    path: Option<&Path>,
    vault: &VaultStore,
    transaction: &mut FileTransaction,
) -> ConfigResult<PathBuf> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => config_path(),
    };

    let mut previous_secret_files = Vec::new();
    if path.exists()
        && let Ok(previous) = load_configuration(Some(&path))
    {
        let mut values = previous
            .get("providers")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|provider| {
                provider
                    .get("api_key_file")
                    .and_then(Value::as_str)
                    .filter(|value| !value.is_empty())
                    .map(str::to_owned)
            })
            .collect::<Vec<_>>();
        values.sort();
        values.dedup();
        previous_secret_files = values;
    }

    let mut config = normalize_configuration(Some(config))?;
    canonicalize_private_paths(&mut config, &path)?;
    let secret_root = canonical_secret_root(&config, &path);
    if let Some(Value::Array(providers)) = config.get_mut("providers") {
        for provider in providers {
            let Some(object) = provider.as_object_mut() else {
                return Err(ConfigError::new("each provider must be an object"));
            };
            let api_key = object
                .get("api_key")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_owned();
            if api_key.is_empty() || api_key == MASKED_API_KEY {
                continue;
            }
            let Some(id) = object.get("id").and_then(Value::as_str) else {
                return Err(ConfigError::new("provider.id must be a string"));
            };
            let secret_path = secret_root.join(format!("{}.key.enc", percent_encode(id)));
            transaction.remember(&secret_path)?;
            vault.write_encrypted_text(&secret_path, &api_key)?;
            object.insert("api_key".to_owned(), Value::String(String::new()));
            object.insert(
                "api_key_file".to_owned(),
                Value::String(secret_path.to_string_lossy().into_owned()),
            );
        }
    }

    transaction.remember(&path)?;
    let serialized = serde_json::to_vec_pretty(&config)
        .map_err(|_| ConfigError::new("configuration could not be serialized"))?;
    let mut serialized = serialized;
    serialized.push(b'\n');
    atomic_write_config(&path, &serialized)?;

    let current_secret_files = config
        .get("providers")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|provider| {
            provider
                .get("api_key_file")
                .and_then(Value::as_str)
                .filter(|value| !value.is_empty())
                .map(str::to_owned)
        })
        .collect::<BTreeSet<_>>();
    for obsolete in previous_secret_files
        .into_iter()
        .filter(|value| !current_secret_files.contains(value))
    {
        let obsolete_path = PathBuf::from(&obsolete);
        transaction.remember(&obsolete_path)?;
        match fs::remove_file(&obsolete_path) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            // Python deliberately treats obsolete-secret cleanup as best effort.
            Err(_) => {}
        }
    }
    Ok(path)
}

/// Load, normalize and canonicalize the private paths in a configuration file.
///
/// This is Python's `config.load`. Missing files return the default
/// configuration. A successful UTF-8 JSON decode is passed through full
/// configuration normalization, then through managed-path canonicalization.
///
/// Python preserves the native `OSError` for non-JSON I/O failures while Rust
/// has no exception hierarchy, so this boundary records the corresponding
/// Python-visible error class while keeping a stable message. JSON syntax
/// errors remain `ConfigError`; parser-specific diagnostic wording may differ.
pub fn load_configuration(path: Option<&Path>) -> ConfigResult<Value> {
    let path = match path {
        Some(path) => path.to_path_buf(),
        None => config_path(),
    };
    if !path.exists() {
        return normalize_configuration(None);
    }

    let raw = std::fs::read(&path).map_err(|error| {
        let python_type = match error.kind() {
            std::io::ErrorKind::NotFound => "FileNotFoundError",
            std::io::ErrorKind::PermissionDenied => "PermissionError",
            std::io::ErrorKind::IsADirectory => "IsADirectoryError",
            _ => "OSError",
        };
        ConfigError::python(python_type, "configuration file could not be read")
    })?;
    let raw = String::from_utf8(raw).map_err(|_| {
        ConfigError::python(
            "UnicodeDecodeError",
            "configuration file is not valid UTF-8",
        )
    })?;
    let value = serde_json::from_str::<Value>(&raw).map_err(|error| {
        ConfigError::new(format!("invalid JSON in {}: {}", path.display(), error))
    })?;
    let mut config = normalize_configuration(Some(&value))?;
    canonicalize_private_paths(&mut config, &path)?;
    Ok(config)
}

/// Resolve one request-local provider credential from normalized state.
///
/// Inline values take precedence. Managed encrypted files fail closed to an
/// empty value, matching Python's `config.api_key` boundary without exposing a
/// decryption or filesystem diagnostic to callers.
pub fn provider_api_key(provider: &Value, vault: &VaultStore) -> String {
    let Some(provider) = provider.as_object() else {
        return String::new();
    };
    if let Some(value) = provider
        .get("api_key")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    {
        return value.to_owned();
    }
    let Some(path) = provider
        .get("api_key_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .map(Path::new)
    else {
        return String::new();
    };
    if path.is_symlink() {
        return String::new();
    }
    vault
        .read_encrypted_text(path)
        .map(|value| value.trim().to_owned())
        .unwrap_or_default()
}

/// Normalize the full top-level configuration without filesystem path canonicalization.
///
/// This is the pure Python `config.normalize` contract. Loading, saving, secret
/// extraction, and canonical private paths are separate state transitions.
pub fn normalize_configuration(raw: Option<&Value>) -> ConfigResult<Value> {
    if raw.is_none() || raw.is_some_and(Value::is_null) {
        return Ok(default_configuration());
    }
    let Some(Value::Object(raw)) = raw else {
        return Err(ConfigError::new("configuration must be an object"));
    };

    let host = configuration_string(raw.get("host"), DEFAULT_HOST, "value")?;
    let host = if host.is_empty() {
        DEFAULT_HOST.to_owned()
    } else {
        host
    };
    if host != DEFAULT_HOST {
        return Err(ConfigError::new(
            "host must be 127.0.0.1 for local-only management",
        ));
    }
    let port_raw = raw
        .get("port")
        .cloned()
        .unwrap_or_else(|| Value::from(DEFAULT_PORT));
    let port = python_int(&port_raw).map_err(|_| python_int_conversion_error(&port_raw))?;
    let native_catalog_path = configuration_string(
        raw.get("native_catalog_path"),
        &default_native_catalog_path(),
        "value",
    )?;
    let account_store_path =
        configuration_string(raw.get("account_store_path"), "state/accounts", "value")?;
    let account_store_path = if account_store_path.is_empty() {
        "state/accounts".to_owned()
    } else {
        account_store_path
    };
    let secret_store_path =
        configuration_string(raw.get("secret_store_path"), "state/secrets", "value")?;
    let secret_store_path = if secret_store_path.is_empty() {
        "state/secrets".to_owned()
    } else {
        secret_store_path
    };
    let codex_base_url = validate_codex_base_url(raw.get("codex_base_url"))?;
    if !(1..=65535).contains(&port) {
        return Err(ConfigError::new("port must be between 1 and 65535"));
    }

    let mut accounts = Vec::new();
    for account in python_iterable(raw.get("accounts"))? {
        accounts.push(normalize_account(&account).map_err(account_error)?);
    }
    let account_ids = accounts
        .iter()
        .map(|account| account["id"].as_str().expect("normalized account id"))
        .collect::<BTreeSet<_>>();
    let account_prefixes = accounts
        .iter()
        .map(|account| {
            account["prefix"]
                .as_str()
                .expect("normalized account prefix")
        })
        .collect::<BTreeSet<_>>();
    if account_ids.len() != accounts.len() {
        return Err(ConfigError::new("account ids must be unique"));
    }
    if account_prefixes.len() != accounts.len() {
        return Err(ConfigError::new("account prefixes must be unique"));
    }

    let mut providers = Vec::new();
    for provider in python_iterable(raw.get("providers"))? {
        providers.push(normalize_provider(&provider)?);
    }
    let mut models = Vec::new();
    for model in python_iterable(raw.get("models"))? {
        models.push(normalize_model(&model)?);
    }
    let provider_ids = providers
        .iter()
        .map(|provider| provider["id"].as_str().expect("normalized provider id"))
        .collect::<BTreeSet<_>>();
    if provider_ids.len() != providers.len() {
        return Err(ConfigError::new("provider ids must be unique"));
    }
    let conflicts = account_prefixes
        .intersection(&provider_ids)
        .copied()
        .collect::<Vec<_>>();
    if !conflicts.is_empty() {
        return Err(ConfigError::new(format!(
            "account prefixes conflict with provider ids: {}",
            conflicts.join(", ")
        )));
    }
    let model_ids = models
        .iter()
        .map(|model| model["id"].as_str().expect("normalized model id"))
        .collect::<BTreeSet<_>>();
    if model_ids.len() != models.len() {
        return Err(ConfigError::new("model ids must be unique"));
    }
    let missing = models
        .iter()
        .map(|model| {
            model["provider"]
                .as_str()
                .expect("normalized model provider")
        })
        .filter(|provider| !provider_ids.contains(provider))
        .collect::<BTreeSet<_>>();
    if !missing.is_empty() {
        return Err(ConfigError::new(format!(
            "models reference unknown providers: {}",
            missing.into_iter().collect::<Vec<_>>().join(", ")
        )));
    }

    let native_hidden_models =
        normalize_hidden_models(raw.get("native_hidden_models"), "native_hidden_models")
            .map_err(account_error)?;
    let native_model_context_windows =
        normalize_context_windows(raw.get("native_model_context_windows"))
            .map_err(account_error)?;
    let catalog_presentations = normalize_catalog_presentations(raw.get("catalog_presentations"))?;
    let catalog_family_presentations =
        normalize_catalog_presentations(raw.get("catalog_family_presentations"))?;
    let subscription_search = normalize_subscription_search(raw.get("subscription_search"))?;
    let codex_runtime_sources = if raw.contains_key("codex_runtime_sources") {
        normalize_codex_runtime_sources(raw.get("codex_runtime_sources"))?
    } else {
        normalize_codex_runtime_sources(Some(&serde_json::json!(["auto"])))?
    };

    Ok(serde_json::json!({
        "host": host,
        "port": port,
        "native_catalog_path": native_catalog_path,
        "account_store_path": account_store_path,
        "secret_store_path": secret_store_path,
        "codex_base_url": codex_base_url,
        "accounts": accounts,
        "providers": providers,
        "models": models,
        "native_hidden_models": native_hidden_models,
        "native_model_context_windows": native_model_context_windows,
        "catalog_presentations": catalog_presentations,
        "catalog_family_presentations": catalog_family_presentations,
        "subscription_search": subscription_search,
        "codex_runtime_sources": codex_runtime_sources,
    }))
}
