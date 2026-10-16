//! Image eligibility uses both shared subscription capacity and image limits.
use super::*;
use crate::services::accounts::{native_auth_document, regular_file};
use crate::services::subscription_quota::image_headroom;
use emp_core::RouteSource;

fn available(account: &Value) -> bool {
    account["enabled"] != false
        && account["credential_status"] != "invalid"
        && account["auth_file"]
            .as_str()
            .is_some_and(|path| regular_file(std::path::Path::new(path)))
}

fn native_available(state: &ServerState) -> bool {
    native_auth_document(&state.backend.accounts.native_auth_path)
        .and_then(|auth| emp_state::validate_auth_json(&auth).ok())
        .and_then(|auth| emp_codex::account_auth_headers(&auth))
        .is_some()
}

fn image_supported(quota: Option<&Value>) -> bool {
    !quota
        .and_then(|quota| quota["plan_type"].as_str())
        .is_some_and(|plan| plan.eq_ignore_ascii_case("free"))
}

pub(super) fn automatic(
    state: &ServerState,
    config: &Value,
    limited: &BTreeMap<String, Instant>,
) -> Result<String, NativeHttpError> {
    let native_quota = state
        .backend
        .accounts
        .native_quota
        .lock()
        .ok()
        .and_then(|quota| quota.clone());
    let imported = config["accounts"]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|account| available(account))
        .filter_map(|account| Some((account["id"].as_str()?, account.get("quota"))));
    let native = native_available(state).then_some(("@native", native_quota.as_ref()));
    let mut best: Option<(&str, Option<f64>)> = None;
    for (id, quota) in native.into_iter().chain(imported) {
        if limited.contains_key(id) || !image_supported(quota) {
            continue;
        }
        let remaining = image_headroom(quota);
        if remaining.is_some_and(|value| value <= 0.0) {
            continue;
        }
        // Read each balance once; a positive known balance beats unknown.
        // Strict comparison preserves Native/account order on ties.
        if best.is_none_or(|(_, current)| remaining > current) {
            best = Some((id, remaining));
        }
    }
    best.map(|(id, _)| (*id).to_owned()).ok_or_else(|| {
        NativeHttpError::router(
            429,
            "No enabled subscription account has image quota available",
        )
    })
}

pub(super) fn account_route(
    state: &ServerState,
    config: &Value,
    id: &str,
    model: &str,
) -> Result<ResolvedRoute, NativeHttpError> {
    let base = config["codex_base_url"].as_str().unwrap_or_default();
    let (provider, source) = if id == "@native" {
        if !native_available(state) {
            return Err(NativeHttpError::router(
                409,
                "The configured Native image account is unavailable",
            ));
        }
        (
            json!({"id":"codex-native","name":"Native Codex","base_url":base,
            "protocol":"responses","auth_mode":"forward","implicit_native":true}),
            RouteSource::ImplicitNative,
        )
    } else {
        let account = config["accounts"]
            .as_array()
            .into_iter()
            .flatten()
            .find(|account| account["id"] == id)
            .filter(|account| available(account))
            .ok_or_else(|| {
                NativeHttpError::router(
                    409,
                    "The configured image account is disabled, removed or unavailable",
                )
            })?;
        if !image_supported(account.get("quota")) {
            return Err(NativeHttpError::router(
                422,
                "The configured account does not support image generation",
            ));
        }
        (
            json!({"id":id,"name":account["name"],"base_url":base,"protocol":"responses",
            "auth_mode":"account","account":account}),
            RouteSource::SubscriptionAccount,
        )
    };
    image_route(provider.as_object().unwrap(), source, model)
}
