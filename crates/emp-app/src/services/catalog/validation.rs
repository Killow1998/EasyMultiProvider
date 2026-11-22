use super::sources::CatalogSources;
use crate::app::ServerState;
use emp_codex::subscription_contexts::validate_subscription_contexts;
use serde_json::{Value, json};
use std::collections::BTreeSet;

pub(crate) fn validate_update(
    state: &ServerState,
    updated: &Value,
    previous: &Value,
) -> Result<(), String> {
    let mut base = updated.clone();
    base["model_groups"] = json!([]);
    let sources = CatalogSources::load(state, &mut base);
    validate_subscription_contexts(updated, Some(previous), &sources.native, &sources.accounts)
        .map_err(|error| error.to_string())?;
    let catalog = sources.merged(&base);
    let slugs: BTreeSet<_> = catalog["models"]
        .as_array()
        .expect("composed models")
        .iter()
        .filter_map(|model| model["slug"].as_str())
        .collect();
    let native_plan = state
        .backend
        .accounts
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.as_ref()?["plan_type"].as_str().map(str::to_owned));
    let previous_groups = previous["model_groups"]
        .as_array()
        .expect("normalized groups");
    let models = updated["models"].as_array().expect("normalized models");
    let accounts = base["accounts"].as_array().expect("normalized accounts");
    for group in updated["model_groups"]
        .as_array()
        .expect("normalized groups")
    {
        if slugs.contains(group["id"].as_str().expect("normalized group ID")) {
            return Err("Aggregate model ID is already in use".into());
        }
        let previous_sources = previous_groups
            .iter()
            .find(|old| old["id"] == group["id"])
            .map(|old| old["sources"].as_array().expect("normalized sources"));
        let mut family = None;
        let mut plans = BTreeSet::new();
        for source in group["sources"].as_array().expect("normalized sources") {
            let source = source.as_str().expect("normalized source");
            if !slugs.contains(source) {
                if previous_sources.is_some_and(|sources| sources.iter().any(|old| old == source)) {
                    continue;
                }
                return Err(format!("Aggregate source is unavailable: {source}"));
            }
            let external = models.iter().find(|model| model["id"] == source);
            if group["policy"] == "balanced" {
                if external.is_some() {
                    return Err("Quota balance requires Codex subscription sources; use priority or round robin for API sources".into());
                }
                let account = source.split_once('/').and_then(|(prefix, _)| {
                    accounts.iter().find(|account| account["prefix"] == prefix)
                });
                let plan = if let Some(account) = account {
                    account["quota"]["plan_type"].as_str()
                } else {
                    native_plan.as_deref()
                };
                if let Some(plan) = plan {
                    plans.insert(plan);
                }
            }
            let current = external
                .and_then(|model| {
                    model["family_id"]
                        .as_str()
                        .filter(|value| !value.is_empty())
                        .or_else(|| model["upstream_id"].as_str())
                })
                .unwrap_or_else(|| source.rsplit('/').next().expect("source"));
            if family.is_some_and(|family| family != current) {
                return Err("Aggregate sources must refer to the same model".into());
            }
            family = Some(current);
        }
        if plans.len() > 1 {
            return Err("Quota balance requires accounts on the same subscription plan".into());
        }
    }
    Ok(())
}
