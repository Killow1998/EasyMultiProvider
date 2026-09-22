//! Read-only Codex account catalog integration.
//!
//! This crate owns the filesystem and credential-derived selection rules used
//! to resolve native and subscription models. It does not write Codex state.

use base64::{Engine as _, engine::general_purpose::URL_SAFE};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;
use std::env;
use std::fs;
use std::path::{Path, PathBuf};

pub mod merged_catalog;
pub mod quota;
pub mod quota_history;

const MAX_ACCOUNT_CATALOG_BYTES: u64 = 4 * 1024 * 1024;
const MAX_CONTEXT_WINDOW: u64 = 100_000_000;
const MAX_OWNER_FIELD_CHARS: usize = 512;
const AUTH_DOMAIN: &[u8] = b"emp-native-route-auth\0";

/// Read the original native Codex model catalog.
///
/// Codex's configured cache may contain EMP's merged catalog. In that case
/// Python EMP reads the preserved native snapshot beside it instead.
pub fn load_native_catalog(config: &Value) -> Value {
    let Some(config) = config.as_object() else {
        return empty_catalog();
    };
    load_native_catalog_from_map(config)
}

/// Preserve the native source before Codex caches EMP's merged models response.
pub fn preserve_native_catalog(config: &Value) -> Result<(), emp_state::FilesystemError> {
    let native = load_native_catalog(config);
    if native
        .get("models")
        .and_then(Value::as_array)
        .is_none_or(Vec::is_empty)
    {
        return Ok(());
    }
    let Some(config) = config.as_object() else {
        return Ok(());
    };
    let path = native_catalog_path(config);
    let destination = path
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .join("easy-multi-provider")
        .join("native-catalog.json");
    emp_state::filesystem::write_catalog_json(&destination, &native)
}

fn load_native_catalog_from_map(config: &Map<String, Value>) -> Value {
    let path = native_catalog_path(config);
    let Some(value) = read_catalog(&path) else {
        return empty_catalog();
    };
    let generated = value
        .get("etag")
        .and_then(Value::as_str)
        .is_some_and(|etag| etag.trim_matches('"').starts_with("emp-"));
    if !generated {
        return value;
    }
    read_catalog(
        &path
            .parent()
            .unwrap_or_else(|| Path::new("."))
            .join("easy-multi-provider")
            .join("native-catalog.json"),
    )
    .unwrap_or_else(empty_catalog)
}

/// Resolve one native/subscription model with Python-compatible cache
/// ownership and context-window projection.
///
/// `account_headers` keeps credential storage outside this crate. It must
/// return the Authorization and optional chatgpt-account-id headers derived
/// from the selected account's current encrypted auth document.
pub fn subscription_route_model<F>(
    config: &Map<String, Value>,
    slug: &str,
    account: Option<&Map<String, Value>>,
    mut account_headers: F,
) -> Option<Map<String, Value>>
where
    F: FnMut(&Map<String, Value>) -> Option<BTreeMap<String, String>>,
{
    let catalog = match account {
        Some(account) => account_catalog(config, account, &mut account_headers),
        None => load_native_catalog_from_map(config),
    };
    let item = catalog
        .get("models")?
        .as_array()?
        .iter()
        .filter_map(Value::as_object)
        .find(|model| {
            model.get("slug").and_then(Value::as_str) == Some(slug)
                && model.get("supported_in_api").and_then(Value::as_bool) != Some(false)
        })?;
    let mut model = item.clone();
    let windows = account
        .and_then(|account| account.get("model_context_windows"))
        .or_else(|| config.get("native_model_context_windows"));
    apply_subscription_context(&mut model, windows);
    Some(model)
}

/// Produce the content-free owner marker used in per-account catalog caches.
pub fn native_catalog_owner(headers: &BTreeMap<String, String>) -> String {
    let account = header(headers, "chatgpt-account-id")
        .filter(|value| !value.is_empty() && value.chars().count() <= MAX_OWNER_FIELD_CHARS)
        .unwrap_or("missing-account");
    let credential = header(headers, "authorization").unwrap_or_default();
    let owner_hint = verified_owner_hint(credential);
    let parts = if !owner_hint.is_empty() && account != "missing-account" {
        vec![account, owner_hint.as_str()]
    } else {
        vec![
            account,
            if credential.is_empty() {
                "missing-authorization"
            } else {
                credential
            },
        ]
    };
    let mut digest = Sha256::new();
    digest.update(AUTH_DOMAIN);
    for part in parts {
        digest.update(part.as_bytes());
        digest.update([0]);
    }
    format!("sha256:{:x}", digest.finalize())
}

/// Derive the bounded credential headers consumed by catalog ownership.
pub fn account_auth_headers(auth: &Value) -> Option<BTreeMap<String, String>> {
    let auth = auth.as_object()?;
    let tokens = auth
        .get("tokens")
        .and_then(Value::as_object)
        .unwrap_or(auth);
    let access_token = tokens.get("access_token")?.as_str()?;
    if access_token.is_empty() {
        return None;
    }
    let mut headers =
        BTreeMap::from([("Authorization".to_owned(), format!("Bearer {access_token}"))]);
    let account_id = tokens
        .get("account_id")
        .or_else(|| auth.get("account_id"))
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty());
    if let Some(account_id) = account_id {
        headers.insert("chatgpt-account-id".to_owned(), account_id.to_owned());
    }
    Some(headers)
}

pub fn account_catalog<F>(
    config: &Map<String, Value>,
    account: &Map<String, Value>,
    account_headers: &mut F,
) -> Value
where
    F: FnMut(&Map<String, Value>) -> Option<BTreeMap<String, String>>,
{
    let candidate = account
        .get("auth_file")
        .and_then(Value::as_str)
        .filter(|path| !path.is_empty())
        .and_then(|path| Path::new(path).parent())
        .map(|parent| parent.join("models_cache.json"));
    let Some(path) = candidate else {
        return load_native_catalog_from_map(config);
    };
    let Ok(metadata) = fs::symlink_metadata(&path) else {
        return load_native_catalog_from_map(config);
    };
    if metadata.file_type().is_symlink()
        || !metadata.is_file()
        || metadata.len() > MAX_ACCOUNT_CATALOG_BYTES
    {
        return load_native_catalog_from_map(config);
    }
    let Some(cached) = read_catalog(&path) else {
        return load_native_catalog_from_map(config);
    };
    let Some(headers) = account_headers(account) else {
        return load_native_catalog_from_map(config);
    };
    let owner_matches = cached.get("account_owner").and_then(Value::as_str)
        == Some(native_catalog_owner(&headers).as_str());
    let base_matches = cached.get("base_url").unwrap_or(&Value::Null)
        == config.get("codex_base_url").unwrap_or(&Value::Null);
    if owner_matches && base_matches {
        cached
    } else {
        load_native_catalog_from_map(config)
    }
}

fn native_catalog_path(config: &Map<String, Value>) -> PathBuf {
    let configured = config
        .get("native_catalog_path")
        .and_then(Value::as_str)
        .unwrap_or_default();
    if configured.is_empty() {
        return home_dir().join(".codex").join("models_cache.json");
    }
    expand_user(Path::new(configured))
}

fn home_dir() -> PathBuf {
    env::var_os("HOME")
        .or_else(|| env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_default()
}

fn expand_user(path: &Path) -> PathBuf {
    let Some(raw) = path.to_str() else {
        return path.to_path_buf();
    };
    let Some(rest) = raw.strip_prefix("~/").or_else(|| raw.strip_prefix("~\\")) else {
        return path.to_path_buf();
    };
    home_dir().join(rest)
}

fn read_catalog(path: &Path) -> Option<Value> {
    let parsed: Value = serde_json::from_slice(&fs::read(path).ok()?).ok()?;
    parsed
        .as_object()
        .filter(|catalog| catalog.get("models").is_some_and(Value::is_array))?;
    Some(parsed)
}

fn empty_catalog() -> Value {
    json!({"models": []})
}

fn apply_subscription_context(model: &mut Map<String, Value>, windows: Option<&Value>) {
    if model
        .get("effective_context_window_percent")
        .is_none_or(Value::is_null)
    {
        model.insert(
            "effective_context_window_percent".to_owned(),
            Value::from(95),
        );
    }
    let slug = model
        .get("slug")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let requested = windows
        .and_then(Value::as_object)
        .and_then(|windows| windows.get(slug))
        .and_then(positive_integer);
    let maximum = if model
        .get("max_context_window")
        .is_some_and(|value| !value.is_null())
    {
        model.get("max_context_window").and_then(context_limit)
    } else {
        model.get("context_window").and_then(context_limit)
    };
    if let (Some(requested), Some(maximum)) = (requested, maximum) {
        model.insert(
            "context_window".to_owned(),
            Value::from(requested.min(maximum)),
        );
        model.remove("auto_compact_token_limit");
    }
}

fn positive_integer(value: &Value) -> Option<u64> {
    value.as_u64().filter(|value| *value > 0)
}

fn context_limit(value: &Value) -> Option<u64> {
    positive_integer(value).filter(|value| *value <= MAX_CONTEXT_WINDOW)
}

fn header<'a>(headers: &'a BTreeMap<String, String>, wanted: &str) -> Option<&'a str> {
    headers
        .iter()
        .find(|(name, _)| name.eq_ignore_ascii_case(wanted))
        .map(|(_, value)| value.trim())
}

fn verified_owner_hint(authorization: &str) -> String {
    let Some((scheme, token)) = authorization.trim().split_once(' ') else {
        return String::new();
    };
    if !scheme.eq_ignore_ascii_case("bearer") {
        return String::new();
    }
    let parts = token.trim().split('.').collect::<Vec<_>>();
    if parts.len() != 3 || parts.iter().any(|part| part.is_empty()) {
        return String::new();
    }
    let mut payload = parts[1].to_owned();
    payload.extend(std::iter::repeat_n('=', (4 - payload.len() % 4) % 4));
    let Ok(decoded) = URL_SAFE.decode(payload) else {
        return String::new();
    };
    let Ok(claims) = serde_json::from_slice::<Value>(&decoded) else {
        return String::new();
    };
    let Some(auth) = claims
        .get("https://api.openai.com/auth")
        .and_then(Value::as_object)
    else {
        return String::new();
    };
    auth.get("chatgpt_user_id")
        .or_else(|| auth.get("user_id"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty() && value.chars().count() <= MAX_OWNER_FIELD_CHARS)
        .unwrap_or_default()
        .to_owned()
}
