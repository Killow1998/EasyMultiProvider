//! Export side of Python-compatible migration bundles.

use super::*;

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
