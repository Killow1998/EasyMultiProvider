use super::{ConfigError, ConfigResult};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn normalize(raw: Option<&Value>, models: &[Value]) -> ConfigResult<Value> {
    let Some(raw) = raw else {
        return Ok(json!([]));
    };
    let groups = raw
        .as_array()
        .ok_or_else(|| ConfigError::new("model_groups must be a list"))?;
    let mut ids = BTreeSet::new();
    let mut prefixes = BTreeSet::new();
    let mut normalized = Vec::new();
    for group in groups {
        let id = group["id"]
            .as_str()
            .filter(|id| {
                id.starts_with("auto/")
                    && id.len() > 5
                    && id.len() <= 128
                    && id
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || b"/-_.".contains(&b))
            })
            .ok_or_else(|| ConfigError::new("Aggregate model IDs must start with auto/"))?;
        if !ids.insert(id) {
            return Err(ConfigError::new("Aggregate model IDs must be unique"));
        }
        let name_key = if group.get("name_prefix").is_some() {
            "name_prefix"
        } else {
            "name"
        };
        let name = group[name_key]
            .as_str()
            .or_else(|| (name_key == "name").then_some(id))
            .ok_or_else(|| ConfigError::new("Aggregate display prefix must be text"))?;
        if name.len() > 256 || name.chars().any(char::is_control) {
            return Err(ConfigError::new("Invalid aggregate name"));
        }
        let policy = group["policy"].as_str().unwrap_or("priority");
        if !["priority", "balanced", "round_robin"].contains(&policy) {
            return Err(ConfigError::new("Unknown aggregate routing policy"));
        }
        let sources = group["sources"]
            .as_array()
            .ok_or_else(|| ConfigError::new("Choose aggregate sources"))?;
        let mut seen = BTreeSet::new();
        for source in sources {
            let source = source
                .as_str()
                .filter(|s| {
                    !s.is_empty()
                        && s.len() <= 256
                        && !s.starts_with("auto/")
                        && !s.chars().any(char::is_control)
                })
                .ok_or_else(|| ConfigError::new("Aggregate sources must be model route IDs"))?;
            if !seen.insert(source) {
                return Err(ConfigError::new("Aggregate sources must be unique"));
            }
        }
        if sources.len() < 2 {
            return Err(ConfigError::new("Choose at least two aggregate sources"));
        }
        if name_key == "name_prefix" {
            let source = sources[0].as_str().expect("normalized source");
            let family = models
                .iter()
                .find(|model| model["id"] == source)
                .and_then(|model| {
                    ["family_id", "upstream_id"].into_iter().find_map(|field| {
                        model[field]
                            .as_str()
                            .map(str::trim)
                            .filter(|value| !value.is_empty())
                    })
                })
                .unwrap_or_else(|| source.rsplit('/').next().expect("source"));
            if !prefixes.insert((family, name)) {
                return Err(ConfigError::new("aggregate.duplicate_prefix"));
            }
        }
        normalized.push(json!({"id":id,(name_key):name,"policy":policy,"sources":sources}));
    }
    Ok(json!(normalized))
}
