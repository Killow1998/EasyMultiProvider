//! Import side of Python-compatible migration bundles.

use super::*;

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

pub(super) fn id_of(value: &Value) -> &str {
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
