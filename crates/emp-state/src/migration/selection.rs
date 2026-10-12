//! Pure selection of services, model visibility and optional subscription preferences.
use super::{MigrationError, MigrationResult};
use serde_json::{Map, Value, json};
use std::collections::{BTreeMap, BTreeSet};

fn id(value: &Value) -> &str {
    value["id"].as_str().unwrap_or_default()
}
fn name(value: &Value) -> Option<&str> {
    value["name"]
        .as_str()
        .map(str::trim)
        .filter(|v| !v.is_empty())
}
fn models(value: &Value) -> &[Value] {
    value["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
}
fn records(value: &Value, field: &str) -> Vec<Value> {
    value[field].as_array().cloned().unwrap_or_default()
}

pub fn select_migration_payload(
    source: &Value,
    selections: &[Value],
    context: bool,
    rename: bool,
) -> MigrationResult<Value> {
    if selections.is_empty() || selections.len() > 1000 {
        return Err(MigrationError::InvalidConfiguration);
    }
    let mut accounts = BTreeMap::new();
    let mut providers = BTreeMap::new();
    for selection in selections {
        if id(selection).is_empty() {
            return Err(MigrationError::InvalidConfiguration);
        }
        let choices = match selection["kind"].as_str() {
            Some("account") => &mut accounts,
            Some("provider") => &mut providers,
            _ => return Err(MigrationError::InvalidConfiguration),
        };
        if choices
            .insert(id(selection).to_owned(), selection)
            .is_some()
        {
            return Err(MigrationError::InvalidConfiguration);
        }
    }
    let native = accounts.remove("@native");
    let native_id = if let Some(choice) = native {
        let record = source["accounts"]
            .as_array()
            .and_then(|items| {
                items
                    .iter()
                    .rev()
                    .find(|record| record["metadata"]["name"] == "Native login")
            })
            .ok_or(MigrationError::AccountCredentialsUnavailable)?;
        let key = id(&record["metadata"]).to_owned();
        accounts.insert(key.clone(), choice);
        Some(key)
    } else {
        None
    };
    let mut payload = source.clone();
    let mut selected_accounts = Vec::new();
    let mut previews = Map::new();
    for mut record in records(source, "accounts") {
        let key = id(&record["metadata"]).to_owned();
        let Some(choice) = accounts.get(&key) else {
            continue;
        };
        let meta = record["metadata"]
            .as_object_mut()
            .ok_or(MigrationError::InvalidAccountRecord)?;
        if let Some(value) = name(choice) {
            meta.insert("name".into(), json!(value));
        }
        let choices = models(choice);
        if !choices.is_empty() {
            meta.insert(
                "hidden_models".into(),
                json!(
                    choices
                        .iter()
                        .filter(|m| m["selected"] == false)
                        .map(id)
                        .collect::<Vec<_>>()
                ),
            );
        }
        if context {
            let windows = meta
                .entry("model_context_windows")
                .or_insert(json!({}))
                .as_object_mut()
                .ok_or(MigrationError::InvalidConfiguration)?;
            for model in choices {
                if let Some(value) = model.get("context_window") {
                    let number = value
                        .as_u64()
                        .or_else(|| value.as_str().and_then(|text| text.parse().ok()));
                    if value.is_null() || value == "" || value == 0 {
                        windows.remove(id(model));
                    } else if let Some(number) = number.filter(|v| *v > 0 && *v <= 1_000_000_000) {
                        windows.insert(id(model).into(), json!(number));
                    } else {
                        return Err(MigrationError::InvalidConfiguration);
                    }
                }
            }
        } else {
            meta.remove("model_context_windows");
        }
        let mut snapshot = choices.to_vec();
        for model in &mut snapshot {
            let object = model
                .as_object_mut()
                .ok_or(MigrationError::InvalidConfiguration)?;
            if !context {
                object.remove("context_window");
            }
            if !rename {
                object.insert(
                    "name".into(),
                    model_original_name(choice, id_from_object(object)),
                );
            }
        }
        previews.insert(key, json!(snapshot));
        selected_accounts.push(record);
    }
    if selected_accounts.len() != accounts.len() {
        return Err(MigrationError::InvalidAccounts);
    }
    let config = payload
        .get_mut("config")
        .and_then(Value::as_object_mut)
        .ok_or(MigrationError::InvalidConfiguration)?;
    config.insert(
        "accounts".into(),
        json!(
            selected_accounts
                .iter()
                .map(|a| a["metadata"].clone())
                .collect::<Vec<_>>()
        ),
    );
    let mut selected_providers = Vec::new();
    for mut provider in records(&source["config"], "providers") {
        let Some(choice) = providers.get(id(&provider)) else {
            continue;
        };
        if let Some(value) = name(choice) {
            provider["name"] = json!(value);
        }
        if let Some(object) = provider.as_object_mut() {
            object.remove("api_key");
            object.remove("api_key_file");
        }
        selected_providers.push(provider);
    }
    if selected_providers.len() != providers.len() {
        return Err(MigrationError::UnknownProviderKey);
    }
    let selected_models: BTreeMap<_, _> = providers
        .values()
        .flat_map(|choice| models(choice))
        .map(|m| (id(m).to_owned(), m))
        .collect();
    let mut external = Vec::new();
    for mut model in records(&source["config"], "models") {
        if !providers.contains_key(model["provider"].as_str().unwrap_or_default()) {
            continue;
        }
        if let Some(choice) = selected_models.get(id(&model)) {
            if choice["selected"] == false {
                continue;
            }
            if let Some(value) = name(choice) {
                model["display_name"] = json!(value);
            }
            if let Some(value) = choice["context_window"].as_u64().filter(|v| *v > 0) {
                model["context_window"] = json!(value);
            }
        }
        external.push(model);
    }
    config.insert("providers".into(), json!(selected_providers));
    config.insert("models".into(), json!(external));
    if !context {
        config.remove("native_model_context_windows");
    }
    let prefixes: BTreeSet<_> = selected_accounts
        .iter()
        .filter_map(|a| a["metadata"]["prefix"].as_str().map(|s| format!("{s}/")))
        .collect();
    let native_models: BTreeSet<_> = native
        .into_iter()
        .flat_map(models)
        .map(id)
        .map(str::to_owned)
        .collect();
    let model_ids: BTreeSet<_> = external.iter().map(id).map(str::to_owned).collect();
    let mut families: BTreeSet<_> = external
        .iter()
        .filter_map(|m| m["family_id"].as_str().or(m["upstream_id"].as_str()))
        .map(str::to_owned)
        .collect();
    families.extend(
        accounts
            .values()
            .flat_map(|s| models(s))
            .map(id)
            .map(str::to_owned),
    );
    if let Some(presentations) = config
        .get_mut("catalog_presentations")
        .and_then(Value::as_object_mut)
    {
        presentations.retain(|route, _| {
            native_models.contains(route)
                || model_ids.contains(route)
                || prefixes.iter().any(|prefix| route.starts_with(prefix))
        });
        if !rename {
            for (route, value) in presentations.iter_mut() {
                if (route.starts_with("gpt-") || prefixes.iter().any(|p| route.starts_with(p)))
                    && let Some(v) = value.as_object_mut()
                {
                    v.remove("catalog_alias");
                }
            }
        }
    }
    if let Some(presentations) = config
        .get_mut("catalog_family_presentations")
        .and_then(Value::as_object_mut)
    {
        presentations.retain(|family, _| families.contains(family));
        if !rename {
            for value in presentations.values_mut() {
                if let Some(v) = value.as_object_mut() {
                    v.remove("catalog_alias");
                }
            }
        }
    }
    if rename {
        let presentations = config
            .entry("catalog_presentations")
            .or_insert(json!({}))
            .as_object_mut()
            .ok_or(MigrationError::InvalidConfiguration)?;
        for account in &selected_accounts {
            let meta = &account["metadata"];
            let choice = accounts[id(meta)];
            for model in models(choice) {
                if let Some(value) = name(model) {
                    let route = format!(
                        "{}/{}",
                        meta["prefix"].as_str().unwrap_or(id(meta)),
                        id(model)
                    );
                    let presentation = presentations
                        .entry(route)
                        .or_insert(json!({}))
                        .as_object_mut()
                        .ok_or(MigrationError::InvalidConfiguration)?;
                    presentation.insert("catalog_alias".into(), json!(value));
                }
            }
        }
    }
    if let (Some(key), Some(choice)) = (native_id, native)
        && let Some(target) = choice["export_id"].as_str()
    {
        let reserved: BTreeSet<_> = records(&source["config"], "accounts")
            .into_iter()
            .chain(records(&source["config"], "providers"))
            .flat_map(|r| {
                [
                    r["id"].as_str().unwrap_or_default().to_owned(),
                    r["prefix"].as_str().unwrap_or_default().to_owned(),
                ]
            })
            .collect();
        if target.len() != 4
            || !target.bytes().all(|b| b.is_ascii_alphanumeric())
            || reserved.contains(target)
        {
            return Err(MigrationError::InvalidConfiguration);
        }
        let record = selected_accounts
            .iter_mut()
            .find(|r| id(&r["metadata"]) == key)
            .ok_or(MigrationError::InvalidAccounts)?;
        let old_prefix = record["metadata"]["prefix"]
            .as_str()
            .unwrap_or(&key)
            .to_owned();
        record["metadata"]["id"] = json!(target);
        record["metadata"]["prefix"] = json!(target);
        if let Some(snapshot) = previews.remove(&key) {
            previews.insert(target.into(), snapshot);
        }
        if let Some(presentations) = config
            .get_mut("catalog_presentations")
            .and_then(Value::as_object_mut)
        {
            *presentations = std::mem::take(presentations)
                .into_iter()
                .map(|(route, value)| {
                    let mapped = if let Some(tail) = route.strip_prefix(&format!("{old_prefix}/")) {
                        format!("{target}/{tail}")
                    } else if native_models.contains(&route) {
                        format!("{target}/{route}")
                    } else {
                        route
                    };
                    (mapped, value)
                })
                .collect();
        }
        config.insert(
            "accounts".into(),
            json!(
                selected_accounts
                    .iter()
                    .map(|a| a["metadata"].clone())
                    .collect::<Vec<_>>()
            ),
        );
    }
    for record in &mut selected_accounts {
        record["metadata"]
            .as_object_mut()
            .unwrap()
            .remove("auth_file");
    }
    payload["accounts"] = json!(selected_accounts);
    payload["model_previews"] = json!(previews);
    if let Some(keys) = payload
        .get_mut("provider_keys")
        .and_then(Value::as_object_mut)
    {
        keys.retain(|key, _| providers.contains_key(key));
    }
    Ok(payload)
}

fn id_from_object(value: &Map<String, Value>) -> &str {
    value.get("id").and_then(Value::as_str).unwrap_or_default()
}
fn model_original_name(choice: &Value, key: &str) -> Value {
    models(choice)
        .iter()
        .find(|m| id(m) == key)
        .and_then(|m| m.get("original_name"))
        .filter(|v| v.as_str().is_some_and(|s| !s.is_empty()))
        .cloned()
        .unwrap_or(json!(key))
}
