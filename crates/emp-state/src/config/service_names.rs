//! Service name validation and retained routing names.

use serde_json::Value;
use std::collections::BTreeSet;

/// Keep published service names usable by existing Codex conversations.
pub fn preserve_service_route_names(current: &Value, candidate: &mut Value) {
    for kind in ["accounts", "providers"] {
        let old = current[kind]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        for service in candidate
            .get_mut(kind)
            .and_then(Value::as_array_mut)
            .into_iter()
            .flatten()
        {
            let Some(previous) = old.iter().find(|item| item["id"] == service["id"]) else {
                continue;
            };
            let mut names = service["route_names"]
                .as_array()
                .cloned()
                .unwrap_or_default();
            for name in previous["route_names"].as_array().into_iter().flatten() {
                if !names.contains(name) {
                    names.push(name.clone());
                }
            }
            if previous["name"] != service["name"] && !names.contains(&previous["name"]) {
                names.push(previous["name"].clone());
            }
            if !names.is_empty() {
                service["route_names"] = Value::Array(names);
            }
        }
    }
}

/// Reject new name collisions while leaving existing configurations readable.
pub fn validate_service_names(current: &Value, candidate: &Value) -> Result<(), &'static str> {
    fn names<'a>(service: &'a Value, prefix: &str) -> impl Iterator<Item = &'a str> {
        service["route_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(service["name"].as_str())
            .chain(service[prefix].as_str())
    }
    let services = || {
        [("accounts", "prefix"), ("providers", "id")]
            .into_iter()
            .flat_map(|(kind, prefix)| {
                candidate[kind]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .map(move |service| (kind, prefix, service))
            })
    };
    for (kind, prefix_field, service) in services() {
        let previous = current[kind]
            .as_array()
            .into_iter()
            .flatten()
            .find(|item| item["id"] == service["id"]);
        if service["name"].as_str().unwrap_or_default().contains('/')
            && previous.is_none_or(|item| item["name"] != service["name"])
        {
            return Err("core.service_name_contains_slash");
        }
        let retained: BTreeSet<_> = previous
            .into_iter()
            .flat_map(|old| names(old, prefix_field))
            .map(|name| name.trim().to_lowercase())
            .collect();
        let mut introduced: BTreeSet<_> = service["route_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(|name| name.trim().to_lowercase())
            .filter(|name| !retained.contains(name))
            .collect();
        if previous.is_none_or(|old| old["name"] != service["name"]) {
            introduced.insert(
                service["name"]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase(),
            );
        }
        if previous.is_none_or(|old| old[prefix_field] != service[prefix_field]) {
            introduced.insert(
                service[prefix_field]
                    .as_str()
                    .unwrap_or_default()
                    .trim()
                    .to_lowercase(),
            );
        }
        for (other_kind, other_prefix, other) in services() {
            if kind == other_kind && service["id"] == other["id"] {
                continue;
            }
            if names(other, other_prefix)
                .any(|value| introduced.contains(&value.trim().to_lowercase()))
            {
                return Err("core.service_name_already_in_use");
            }
        }
    }
    Ok(())
}
