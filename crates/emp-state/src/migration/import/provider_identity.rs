//! Preserve distinct provider connections when a migration reuses their IDs.
use super::*;

pub(super) fn map_providers(
    current: &Value,
    source: &mut Value,
    keys: &mut Map<String, Value>,
    vault: &VaultStore,
) -> BTreeMap<String, String> {
    let existing = current["providers"].as_array().cloned().unwrap_or_default();
    let incoming = source["providers"].as_array().cloned().unwrap_or_default();
    let mut reserved = BTreeSet::new();
    for config in [&*source, current] {
        for field in ["providers", "accounts"] {
            for record in config[field].as_array().into_iter().flatten() {
                reserved.insert(id_of(record).to_owned());
                if !prefix_of(record).is_empty() {
                    reserved.insert(prefix_of(record).to_owned());
                }
            }
        }
    }
    let mut known = existing.clone();
    let mut mapping = BTreeMap::new();
    for provider in incoming {
        let id = id_of(&provider);
        let key = keys.get(id).and_then(Value::as_str).unwrap_or("");
        let collision = existing.iter().find(|record| id_of(record) == id);
        let matched = known.iter().find(|record| {
            !key.trim().is_empty()
                && record["base_url"].as_str().is_some()
                && record["base_url"] == provider["base_url"]
                && record
                    .get("execution_backend")
                    .and_then(Value::as_str)
                    .unwrap_or("http")
                    == provider
                        .get("execution_backend")
                        .and_then(Value::as_str)
                        .unwrap_or("http")
                && crate::config::provider_api_key(record, vault) == key
        });
        let occupied_route = current["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|record| prefix_of(record) == id || id_of(record) == id);
        let target = if let Some(record) = matched {
            id_of(record).to_owned()
        } else if collision.is_some() || occupied_route {
            unique_segment(id, &mut reserved)
        } else {
            id.to_owned()
        };
        let mut planned = provider.clone();
        set_string(&mut planned, "id", target.clone());
        set_string(&mut planned, "api_key", key);
        known.push(planned);
        mapping.insert(id.to_owned(), target);
    }
    if let Some(providers) = source["providers"].as_array_mut() {
        for provider in providers {
            let target = mapping[id_of(provider)].clone();
            set_string(provider, "id", target);
        }
    }
    if let Some(models) = source["models"].as_array_mut() {
        for model in models {
            let old = model["provider"].as_str().unwrap_or("");
            let Some(target) = mapping.get(old).filter(|target| target.as_str() != old) else {
                continue;
            };
            let suffix = id_of(model)
                .strip_prefix(&format!("{old}/"))
                .unwrap_or_else(|| model["upstream_id"].as_str().unwrap_or(""));
            let route = format!("{target}/{suffix}");
            set_string(model, "id", route);
            set_string(model, "provider", target);
        }
    }
    *keys = std::mem::take(keys)
        .into_iter()
        .map(|(id, key)| (mapping.get(&id).cloned().unwrap_or(id), key))
        .collect();
    mapping
}
