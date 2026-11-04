//! Read the CPA observer's cached, caller-scoped subscription windows.
use crate::app::ServerState;
use emp_state::provider_api_key;
use emp_transport::{HttpClient, HttpMethod};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};
use time::{OffsetDateTime, format_description::well_known::Rfc3339};
use url::Url;

#[derive(Clone, Deserialize, Serialize)]
struct Window {
    remaining_percent: Option<f64>,
    resets_at: Option<String>,
}

#[derive(Clone, Deserialize, Serialize)]
struct Account {
    id: String,
    enabled: bool,
    stale: bool,
    status: String,
    as_of: Option<String>,
    windows: BTreeMap<String, Window>,
}

#[derive(Deserialize)]
struct Reply {
    scope: String,
    provider: String,
    checked_at: Option<String>,
    accounts: Vec<Account>,
}

#[derive(Clone, Serialize)]
struct Observation {
    observed_at: f64,
    quota: Option<Value>,
    error_code: Option<&'static str>,
}

#[derive(Default)]
pub(crate) struct PoolQuotas(Mutex<BTreeMap<String, Observation>>);

fn supported(provider: &Value) -> bool {
    provider["execution_backend"] == "claude_cli" && provider["auth_mode"] != "claude_login"
}

fn identity(provider: &Value, key: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&provider["id"], &provider["base_url"], key,))
                .expect("CPA quota identity")
        )
    )
}

pub(crate) fn snapshot(state: &ServerState, config: &Value) -> Value {
    let cache = state
        .backend
        .provider_quotas
        .0
        .lock()
        .expect("CPA quota cache");
    Value::Object(config["providers"].as_array().expect("normalized providers").iter()
        .filter(|provider| supported(provider))
        .map(|provider| {
            let key = provider_api_key(provider, &state.backend.configuration.vault);
            let identity = identity(provider, &key);
            (provider["id"].as_str().expect("provider ID").into(), json!({
                "identity":identity,"refreshable":provider["enabled"] != false && !key.is_empty(),
                "observation":cache.get(&identity),
            }))
        }).collect())
}

async fn query(client: &HttpClient, base: &str, key: &str) -> Result<Value, &'static str> {
    let base = base.trim_end_matches('/');
    let base = base.strip_suffix("/v1").unwrap_or(base);
    let url = Url::parse(&format!("{base}/v1/quota")).map_err(|_| "cpa_quota_invalid_response")?;
    let response = client
        .open(
            HttpMethod::Get,
            url.as_str(),
            BTreeMap::from([
                ("Authorization".into(), format!("Bearer {key}")),
                ("Accept".into(), "application/json".into()),
            ]),
            None,
            false,
        )
        .await
        .map_err(|_| "cpa_quota_connection_failed")?;
    match response.status() {
        200 => {}
        401 | 403 => return Err("cpa_quota_access_denied"),
        404 | 405 => return Err("cpa_quota_unsupported"),
        429 => return Err("cpa_quota_rate_limited"),
        _ => return Err("cpa_quota_unavailable"),
    }
    let bytes = response
        .read_limited(2 * 1024 * 1024)
        .await
        .map_err(|_| "cpa_quota_connection_failed")?;
    let reply: Reply = serde_json::from_slice(&bytes).map_err(|_| "cpa_quota_invalid_response")?;
    if reply.scope != "shared_pool"
        || reply.provider != "claude"
        || reply
            .accounts
            .iter()
            .flat_map(|account| account.windows.values())
            .any(|window| {
                window
                    .remaining_percent
                    .is_some_and(|value| !(0.0..=100.0).contains(&value))
            })
    {
        return Err("cpa_quota_invalid_response");
    }
    let now = crate::util::system_now();
    let mut limits = serde_json::Map::new();
    for (name, kind, minutes) in [
        ("five_hour", "primary", 300),
        ("seven_day", "secondary", 10080),
    ] {
        let windows = reply
            .accounts
            .iter()
            .filter(|account| account.enabled && !account.stale)
            .filter_map(|account| account.windows.get(name))
            .filter(|window| window.remaining_percent.is_some())
            .collect::<Vec<_>>();
        let remaining = (!windows.is_empty()).then(|| {
            windows
                .iter()
                .map(|window| window.remaining_percent.expect("known quota"))
                .sum::<f64>()
                / windows.len() as f64
        });
        let reset = windows
            .iter()
            .filter_map(|window| {
                let reset = window.resets_at.as_deref()?;
                let timestamp = OffsetDateTime::parse(reset, &Rfc3339)
                    .ok()?
                    .unix_timestamp();
                (timestamp as f64 > now).then_some(timestamp)
            })
            .min();
        limits.insert(
            kind.into(),
            json!({"used_percent":remaining.map(|value|100.0-value),
            "window_minutes":minutes,"resets_at":reset}),
        );
    }
    Ok(
        json!({"checked_at":reply.checked_at,"accounts":reply.accounts,
        "rate_limits_by_limit_id":{"claude":limits}}),
    )
}

pub(crate) fn refresh(state: &ServerState, id: &str, force: bool) -> Option<Value> {
    let provider = state.backend.configuration.read().ok()?["providers"]
        .as_array()?
        .iter()
        .find(|provider| {
            provider["id"] == id && provider["enabled"] != false && supported(provider)
        })?
        .clone();
    let key = provider_api_key(&provider, &state.backend.configuration.vault);
    let identity = identity(&provider, &key);
    let now = crate::util::system_now();
    if !force
        && let Some(cached) = state
            .backend
            .provider_quotas
            .0
            .lock()
            .expect("CPA quota cache")
            .get(&identity)
            .filter(|cached| now - cached.observed_at < 300.0)
    {
        return Some(json!({"identity":identity,"observation":cached}));
    }
    let result = if key.is_empty() {
        Err("cpa_quota_key_missing")
    } else {
        state.backend.transport.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                query(
                    &state.backend.transport.client,
                    provider["base_url"].as_str().expect("provider URL"),
                    &key,
                ),
            )
            .await
            .unwrap_or(Err("cpa_quota_timeout"))
        })
    };
    let observation = Observation {
        observed_at: crate::util::system_now(),
        quota: result.as_ref().ok().cloned(),
        error_code: result.err(),
    };
    state.backend.diagnostics.journal.event(
        "info",
        "cpa_quota_refresh",
        &json!({"provider_id":id,"error_code":observation.error_code}),
    );
    state
        .backend
        .provider_quotas
        .0
        .lock()
        .expect("CPA quota cache")
        .insert(identity.clone(), observation.clone());
    state
        .backend
        .management_events
        .publish(super::management_events::Change::Quota);
    Some(json!({"identity":identity,"observation":observation}))
}
