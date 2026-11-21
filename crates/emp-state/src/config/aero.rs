use super::{ConfigError, ConfigResult};
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(super) fn normalize(raw: &Value) -> ConfigResult<Value> {
    let profiles = raw
        .as_array()
        .ok_or_else(|| ConfigError::new("aero_profiles must be a list"))?;
    let mut ids = BTreeSet::new();
    let mut names = BTreeSet::new();
    let mut result = Vec::new();
    for profile in profiles {
        let id = profile["id"]
            .as_str()
            .filter(|id| id.starts_with("aero/") && id.len() > 5 && id.len() <= 128)
            .ok_or_else(|| ConfigError::new("Invalid aero profile ID"))?;
        let name = profile["name"]
            .as_str()
            .map(str::trim)
            .filter(|name| {
                !name.is_empty()
                    && name.len() <= 128
                    && !name.contains('/')
                    && !name.chars().any(char::is_control)
            })
            .ok_or_else(|| ConfigError::new("aero.invalid_name"))?;
        if !ids.insert(id) || !names.insert(name) {
            return Err(ConfigError::new("aero.duplicate_name"));
        }
        let mut entry = json!({"id":id,"name":name});
        for role in ["planner", "executor"] {
            let model = profile[role]["model"]
                .as_str()
                .filter(|model| !model.is_empty() && !model.starts_with("aero/"))
                .ok_or_else(|| ConfigError::new("aero.choose_models"))?;
            entry[role] = json!({"model":model});
            if let Some(effort) = profile[role].get("effort") {
                let effort = effort
                    .as_str()
                    .filter(|effort| crate::model_values::is_concrete_reasoning_effort(effort))
                    .ok_or_else(|| ConfigError::new("aero.invalid_effort"))?;
                entry[role]["effort"] = json!(effort);
            }
        }
        let mut retained = Vec::new();
        if let Some(routes) = profile.get("route_names") {
            for route in routes
                .as_array()
                .ok_or_else(|| ConfigError::new("aero route_names must be a list"))?
            {
                let route = route
                    .as_str()
                    .filter(|route| route.starts_with("aero/") && route.len() <= 256)
                    .ok_or_else(|| ConfigError::new("Invalid aero retained route"))?;
                if !retained.iter().any(|value| value == route) {
                    retained.push(json!(route));
                }
            }
        }
        entry["route_names"] = json!(retained);
        result.push(entry);
    }
    let mut owners = std::collections::BTreeMap::new();
    for profile in &result {
        let route = format!(
            "aero/{}",
            profile["name"].as_str().expect("normalized name")
        );
        for route in std::iter::once(route.as_str()).chain(
            profile["route_names"]
                .as_array()
                .expect("normalized retained routes")
                .iter()
                .map(|route| route.as_str().expect("normalized route")),
        ) {
            if owners
                .insert(route.to_owned(), &profile["id"])
                .is_some_and(|owner| owner != &profile["id"])
            {
                return Err(ConfigError::new("aero.duplicate_name"));
            }
        }
    }
    Ok(json!(result))
}

pub(super) fn preserve_names(current: &Value, updated: &mut Value) {
    for profile in updated
        .get_mut("aero_profiles")
        .and_then(Value::as_array_mut)
        .into_iter()
        .flatten()
    {
        let Some(previous) = current["aero_profiles"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|previous| previous["id"] == profile["id"])
        else {
            continue;
        };
        let mut names = previous["route_names"]
            .as_array()
            .cloned()
            .unwrap_or_default();
        if previous["name"] != profile["name"] {
            names.push(json!(format!(
                "aero/{}",
                previous["name"].as_str().expect("normalized previous name")
            )));
        }
        profile["route_names"] = json!(names);
    }
}
