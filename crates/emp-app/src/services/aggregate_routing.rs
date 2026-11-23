//! Resolve a conversation's source before authentication or history preparation.
use crate::app::ServerState;
use emp_domain::{ResolvedRoute, RouteResolutionError, RouteSource, resolve_route};
use emp_modules::{
    Command, Module,
    aggregate::{BindingPlan, Candidate, Policy, Request, Source},
};
use serde_json::{Value, json};
use std::collections::{BTreeMap, BTreeSet};

pub(super) fn resolve(
    state: &ServerState,
    config: &Value,
    body: &serde_json::Map<String, Value>,
    incoming: &BTreeMap<String, String>,
) -> Option<Result<ResolvedRoute, RouteResolutionError>> {
    let requested = body["model"].as_str()?;
    let group = config["model_groups"]
        .as_array()?
        .iter()
        .find(|group| group["id"] == requested)?;
    if !state.backend.modules.active(Module::Aggregate) {
        return Some(Err(RouteResolutionError::ProviderUnavailable(format!(
            "{requested}: Aggregate Routing is disabled"
        ))));
    }
    Some((|| {
        let unavailable = || RouteResolutionError::ProviderUnavailable(requested.into());
        let header = |name: &str| {
            incoming
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        };
        let anchor = emp_history::request_history_anchor(body, incoming).ok();
        let thread = anchor
            .as_ref()
            .and_then(|anchor| anchor.thread_id.as_deref())
            .or_else(|| header("session-id"))
            .ok_or_else(|| {
                RouteResolutionError::ProviderUnavailable(format!(
                    "{requested}: send thread-id or session-id to keep the account fixed"
                ))
            })?;
        let caller = header("x-emp-caller-id").unwrap_or("local");
        let policy = group["policy"]
            .as_str()
            .and_then(Policy::parse)
            .ok_or_else(unavailable)?;
        let mut routes = Vec::new();
        let mut candidates = Vec::new();
        let mut owner_ids = BTreeMap::new();
        let cooling = state.backend.aggregate_availability.cooling();
        let native_quota = state
            .backend
            .accounts
            .native_quota
            .lock()
            .ok()
            .and_then(|quota| quota.clone());
        let tier = body.get("service_tier").and_then(Value::as_str);
        for source in group["sources"].as_array().expect("normalized sources") {
            let source = source.as_str().expect("normalized source");
            let Ok(route) = resolve_route(config, source, |config, slug, account| {
                super::catalog::subscription_model(state, config, slug, account)
            }) else {
                continue;
            };
            if state
                .backend
                .team
                .check_model_route(state, &route, tier)
                .is_err()
            {
                continue;
            }
            let (owner, quota, source) = match route.source {
                RouteSource::ImplicitNative => {
                    let Some(auth) = super::accounts::native_auth_document(
                        &state.backend.accounts.native_auth_path,
                    ) else {
                        continue;
                    };
                    if let Some(key) = auth["OPENAI_API_KEY"]
                        .as_str()
                        .filter(|key| !key.is_empty())
                    {
                        let source = if emp_integration::local_auth::is_local(&auth) {
                            Source::LocalNativeApiKey
                        } else {
                            Source::NativeApiKey
                        };
                        (
                            emp_state::usage::identity_hash(&json!({"native_api_key":key})),
                            None,
                            source,
                        )
                    } else if emp_state::accounts::validate_auth_json(&auth)
                        .ok()
                        .and_then(|auth| emp_codex::account_auth_headers(&auth))
                        .is_some()
                    {
                        (
                            emp_state::accounts::account_auth_identity(&auth),
                            native_quota.as_ref(),
                            Source::Subscription,
                        )
                    } else {
                        continue;
                    }
                }
                RouteSource::SubscriptionAccount => {
                    let account = &route.provider.value()["account"];
                    if account["credential_status"] == "invalid" {
                        continue;
                    }
                    let Some(path) = account["auth_file"].as_str() else {
                        continue;
                    };
                    let Some(auth) = state
                        .backend
                        .configuration
                        .vault
                        .read_encrypted_json(std::path::Path::new(path))
                        .ok()
                        .filter(|auth| emp_codex::account_auth_headers(auth).is_some())
                    else {
                        continue;
                    };
                    let quota = config["accounts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|item| item["id"] == account["id"])
                        .and_then(|account| account.get("quota"));
                    (
                        emp_state::accounts::account_auth_identity(&auth),
                        quota,
                        Source::Subscription,
                    )
                }
                _ => (
                    format!("provider:{}:{}", route.provider_id, route.upstream_model),
                    None,
                    Source::Provider,
                ),
            };
            let next_owner_id = owner_ids.len();
            let owner_id = *owner_ids.entry(owner.clone()).or_insert(next_owner_id);
            candidates.push(Candidate {
                id: routes.len(),
                owner_id,
                source,
                cooling_down: cooling.contains(&owner),
                headroom: super::subscription_quota::current_headroom(quota),
                subscription_plan: quota
                    .and_then(|quota| quota["plan_type"].as_str())
                    .map(str::to_owned),
            });
            routes.push((route, owner));
        }
        let plan: BindingPlan = state
            .backend
            .modules
            .call_typed(
                Module::Aggregate,
                Command::Aggregate {
                    request: Request { policy, candidates },
                },
            )
            .map_err(|error| {
                if error == "mixed_subscription_plans" {
                    RouteResolutionError::ProviderUnavailable(format!(
                        "{requested}: quota balance requires accounts on the same subscription plan"
                    ))
                } else {
                    unavailable()
                }
            })?;
        let mut seen_owners = BTreeSet::new();
        let owners = plan
            .candidate_ids
            .iter()
            .map(|&id| {
                let (_, owner) = routes.get(id)?;
                seen_owners.insert(owner.as_str()).then(|| owner.clone())
            })
            .collect::<Option<Vec<_>>>()
            .ok_or_else(unavailable)?;
        let index = state
            .backend
            .usage
            .ledger
            .bind_aggregate(caller, thread, requested, &owners, plan.rotate)
            .map_err(|_| unavailable())?;
        let (mut route, owner) = routes.swap_remove(plan.candidate_ids[index]);
        let source = route.requested_model.clone();
        route.requested_model = requested.into();
        let mut model = route.model.value().clone();
        model.insert("_emp_aggregate_source".into(), json!(source));
        model.insert("_emp_aggregate_owner".into(), json!(owner));
        if let Some(entry) = super::catalog::server_catalog(state, config)["models"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|entry| entry["slug"] == requested)
        {
            for field in [
                "context_window",
                "max_context_window",
                "auto_compact_token_limit",
            ] {
                if let Some(value) = entry.get(field) {
                    model.insert(field.into(), value.clone());
                }
            }
        }
        route.model = emp_domain::OpaqueJson::new(model).map_err(|_| unavailable())?;
        Ok(route)
    })())
}
