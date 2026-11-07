//! Resolve a conversation's source before authentication or history preparation.
use crate::app::ServerState;
use emp_core::{ResolvedRoute, RouteResolutionError, RouteSource, resolve_route};
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
        let mut candidates = Vec::new();
        let mut owners = BTreeSet::new();
        let cooling = state.backend.aggregate_availability.cooling();
        let native_quota = state
            .backend
            .accounts
            .native_quota
            .lock()
            .ok()
            .and_then(|quota| quota.clone());
        for source in group["sources"].as_array().expect("normalized sources") {
            let source = source.as_str().expect("normalized source");
            let Ok(route) = resolve_route(config, source, |config, slug, account| {
                super::catalog::subscription_model(state, config, slug, account)
            }) else {
                continue;
            };
            let (owner, quota, plan) = match route.source {
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
                        if emp_integration::local_auth::is_local(&auth)
                            || group["policy"] == "balanced"
                        {
                            continue;
                        }
                        (
                            emp_state::usage::identity_hash(&json!({"native_api_key":key})),
                            None,
                            None,
                        )
                    } else if emp_state::accounts::validate_auth_json(&auth)
                        .ok()
                        .and_then(|auth| emp_codex::account_auth_headers(&auth))
                        .is_some()
                    {
                        (
                            emp_state::accounts::account_auth_identity(&auth),
                            native_quota.as_ref(),
                            native_quota
                                .as_ref()
                                .and_then(|quota| quota["plan_type"].as_str()),
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
                    // Borrow the stable configuration record, not the request-local route snapshot.
                    let quota = config["accounts"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .find(|item| item["id"] == account["id"])
                        .and_then(|account| account.get("quota"));
                    (
                        emp_state::accounts::account_auth_identity(&auth),
                        quota,
                        quota.and_then(|quota| quota["plan_type"].as_str()),
                    )
                }
                _ => (
                    format!("provider:{}:{}", route.provider_id, route.upstream_model),
                    None,
                    None,
                ),
            };
            let capacity = super::subscription_quota::current_headroom(quota);
            if capacity.is_some_and(|value| value <= 0.0)
                || cooling.contains(&owner)
                || !owners.insert(owner.clone())
            {
                continue;
            }
            candidates.push((route, owner, capacity, plan.map(str::to_owned)));
        }
        if group["policy"] == "balanced" {
            let plans = candidates
                .iter()
                .filter_map(|candidate| candidate.3.as_deref())
                .collect::<BTreeSet<_>>();
            if plans.len() > 1 {
                return Err(RouteResolutionError::ProviderUnavailable(format!(
                    "{requested}: quota balance requires accounts on the same subscription plan"
                )));
            }
            candidates.sort_by(|a, b| b.2.partial_cmp(&a.2).unwrap_or(std::cmp::Ordering::Equal));
        }
        let index = state
            .backend
            .usage
            .ledger
            .bind_aggregate(
                caller,
                thread,
                requested,
                &candidates
                    .iter()
                    .map(|candidate| candidate.1.clone())
                    .collect::<Vec<_>>(),
                group["policy"] == "round_robin",
            )
            .map_err(|_| unavailable())?;
        let (mut route, owner, _, _) = candidates.swap_remove(index);
        let source = route.requested_model.clone();
        route.requested_model = requested.into();
        let mut model = route.model.value().clone();
        model.insert("_emp_aggregate_source".into(), json!(source));
        model.insert("_emp_aggregate_owner".into(), json!(owner));
        // Match the conservative window advertised for this aggregate.
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
        route.model = emp_core::OpaqueJson::new(model).map_err(|_| unavailable())?;
        Ok(route)
    })())
}
