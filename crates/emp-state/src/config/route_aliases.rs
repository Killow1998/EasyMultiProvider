//! Client-facing route names resolve to the original persisted model IDs.
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn aggregate_alias(config: &Value, group: &Value) -> Option<String> {
    let prefix = group["name_prefix"]
        .as_str()
        .filter(|prefix| !prefix.is_empty() && !prefix.contains('/'))?;
    let source = group["sources"].as_array()?.first()?.as_str()?;
    let models = config["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let family = super::aggregates::source_family(models, source);
    let name = config["catalog_family_presentations"]
        .get(family)
        .and_then(|presentation| presentation["catalog_alias"].as_str())
        .filter(|name| !name.is_empty())
        .unwrap_or(family);
    Some(format!("{prefix} · {name}"))
}

fn register_alias(
    aliases: &mut BTreeMap<String, Option<String>>,
    canonical: &BTreeSet<&str>,
    alias: String,
    route: &str,
) {
    if canonical.contains(alias.as_str()) && alias != route {
        return;
    }
    aliases
        .entry(alias)
        .and_modify(|owner| {
            if owner.as_deref() != Some(route) {
                *owner = None;
            }
        })
        .or_insert_with(|| Some(route.to_owned()));
}

pub fn client_route_aliases(config: &Value) -> BTreeMap<String, String> {
    let models = config["models"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    let canonical: BTreeSet<_> = models
        .iter()
        .chain(config["model_groups"].as_array().into_iter().flatten())
        .filter_map(|model| model["id"].as_str())
        .collect();
    let prefixes: BTreeSet<_> = config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|account| account["prefix"].as_str())
        .collect();
    let mut aliases: BTreeMap<String, Option<String>> = BTreeMap::new();
    for group in config["model_groups"].as_array().into_iter().flatten() {
        let Some(route) = group["id"].as_str() else {
            continue;
        };
        for alias in group["route_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
        {
            if alias
                .split_once('/')
                .is_some_and(|(prefix, _)| prefixes.contains(prefix))
            {
                continue;
            }
            register_alias(&mut aliases, &canonical, alias.to_owned(), route);
        }
    }
    let retained: BTreeSet<_> = aliases.keys().cloned().collect();
    for provider in config["providers"].as_array().into_iter().flatten() {
        let id = provider["id"].as_str().unwrap_or_default();
        let name = provider["name"].as_str().unwrap_or(id);
        let names: BTreeSet<_> = provider["route_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(std::iter::once(name))
            .collect();
        for model in models.iter().filter(|model| model["provider"] == id) {
            let route = model["id"].as_str().unwrap_or_default();
            let Some(suffix) = route.strip_prefix(&format!("{id}/")) else {
                continue;
            };
            for name in &names {
                if prefixes.contains(name) {
                    continue;
                }
                let alias = format!("{name}/{suffix}");
                if !retained.contains(&alias) {
                    register_alias(&mut aliases, &canonical, alias, route);
                }
            }
        }
    }
    for group in config["model_groups"].as_array().into_iter().flatten() {
        let Some(route) = group["id"].as_str() else {
            continue;
        };
        if let Some(alias) = aggregate_alias(config, group) {
            if retained.contains(&alias) {
                continue;
            }
            if alias
                .split_once('/')
                .is_some_and(|(prefix, _)| prefixes.contains(prefix))
            {
                continue;
            }
            register_alias(&mut aliases, &canonical, alias, route);
        }
    }
    aliases
        .into_iter()
        .filter_map(|(alias, route)| route.map(|route| (alias, route)))
        .collect()
}

/// Resolve a readable subscription name without changing its persisted prefix.
pub fn resolve_client_model(config: &Value, model: &str) -> Option<String> {
    resolve_with_aliases(config, &client_route_aliases(config), model)
}

fn resolve_with_aliases(
    config: &Value,
    aliases: &BTreeMap<String, String>,
    model: &str,
) -> Option<String> {
    let accounts = config["accounts"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    if config["models"]
        .as_array()
        .into_iter()
        .flatten()
        .any(|entry| entry["id"] == model)
        || config["model_groups"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|entry| entry["id"] == model)
        || accounts.iter().any(|account| {
            account["prefix"].as_str().is_some_and(|prefix| {
                model
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
        })
    {
        return None;
    }

    let mut route = aliases.get(model).cloned();
    for account in accounts {
        let prefix = account["prefix"].as_str().unwrap_or_default();
        let names = account["route_names"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .chain(account["name"].as_str());
        for name in names {
            let Some(suffix) = model
                .strip_prefix(name)
                .and_then(|suffix| suffix.strip_prefix('/'))
            else {
                continue;
            };
            if suffix.is_empty() {
                continue;
            }
            let candidate = format!("{prefix}/{suffix}");
            if route
                .as_ref()
                .is_some_and(|existing| existing != &candidate)
            {
                return None;
            }
            route = Some(candidate);
        }
    }
    route
}

pub fn client_catalog(config: &Value, mut catalog: Value) -> Value {
    let aliases = client_route_aliases(config);
    let records = |kind: &str| {
        config[kind]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default()
    };
    let accounts = records("accounts");
    let providers = records("providers");
    let groups = records("model_groups");
    let models = records("models");
    for model in catalog["models"].as_array_mut().into_iter().flatten() {
        let route = model["slug"].as_str().unwrap_or_default();
        let alias = if let Some(group) = groups.iter().find(|group| group["id"] == route) {
            aggregate_alias(config, group)
        } else if let Some(account) = accounts.iter().find(|account| {
            account["prefix"].as_str().is_some_and(|prefix| {
                route
                    .strip_prefix(prefix)
                    .is_some_and(|suffix| suffix.starts_with('/'))
            })
        }) {
            let prefix = account["prefix"].as_str().unwrap_or_default();
            let name = account["name"].as_str().unwrap_or(prefix);
            Some(format!("{name}{}", &route[prefix.len()..]))
        } else {
            models
                .iter()
                .find(|configured| configured["id"] == route)
                .and_then(|configured| {
                    providers
                        .iter()
                        .find(|provider| provider["id"] == configured["provider"])
                })
                .and_then(|provider| {
                    let id = provider["id"].as_str().unwrap_or_default();
                    let name = provider["name"].as_str().unwrap_or(id);
                    route
                        .strip_prefix(&format!("{id}/"))
                        .map(|suffix| format!("{name}/{suffix}"))
                })
        };
        if let Some(alias) = alias
            && resolve_with_aliases(config, &aliases, &alias).as_deref() == Some(route)
        {
            model["slug"] = alias.into();
        }
    }
    catalog
}
