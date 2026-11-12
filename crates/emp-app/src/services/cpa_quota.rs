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

mod history;

const REFRESH_INTERVAL_SECONDS: i64 = 300;

#[derive(Clone, Deserialize, Serialize)]
struct Window {
    remaining_percent: Option<f64>,
    resets_at: Option<String>,
    #[serde(skip)]
    reset_timestamp: Option<i64>,
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
struct Quota {
    checked_at: Option<String>,
    accounts: Vec<Account>,
    rate_limits_by_limit_id: BTreeMap<&'static str, Value>,
}

#[derive(Clone, Serialize)]
struct Observation {
    observed_at: f64,
    quota: Option<Quota>,
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

async fn query(client: &HttpClient, base: &str, key: &str) -> Result<Quota, &'static str> {
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
    let mut reply: Reply =
        serde_json::from_slice(&bytes).map_err(|_| "cpa_quota_invalid_response")?;
    if reply.scope != "shared_pool" || reply.provider != "claude" {
        return Err("cpa_quota_invalid_response");
    }
    for window in reply
        .accounts
        .iter_mut()
        .flat_map(|account| account.windows.values_mut())
    {
        if window
            .remaining_percent
            .is_some_and(|value| !(0.0..=100.0).contains(&value))
        {
            return Err("cpa_quota_invalid_response");
        }
        window.reset_timestamp = window
            .resets_at
            .as_deref()
            .and_then(|value| OffsetDateTime::parse(value, &Rfc3339).ok())
            .map(|value| value.unix_timestamp());
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
            .filter_map(|account| {
                let window = account.windows.get(name)?;
                let weekly_block = account
                    .windows
                    .get("seven_day")
                    .filter(|weekly| name == "five_hour" && weekly.remaining_percent == Some(0.0));
                Some((window, weekly_block))
            })
            .filter(|(window, _)| window.remaining_percent.is_some())
            .collect::<Vec<_>>();
        let remaining = (!windows.is_empty()).then(|| {
            windows
                .iter()
                .map(|(window, weekly_block)| {
                    if weekly_block.is_some() {
                        0.0
                    } else {
                        window.remaining_percent.expect("known quota")
                    }
                })
                .sum::<f64>()
                / windows.len() as f64
        });
        let reset = windows
            .iter()
            .filter_map(|(window, _)| window.reset_timestamp)
            .filter(|timestamp| *timestamp as f64 > now)
            .min();
        limits.insert(
            kind.into(),
            json!({"used_percent":remaining.map(|value|100.0-value),
            "window_minutes":minutes,"resets_at":reset}),
        );
    }
    Ok(Quota {
        checked_at: reply.checked_at,
        accounts: reply.accounts,
        rate_limits_by_limit_id: BTreeMap::from([("claude", Value::Object(limits))]),
    })
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
            .filter(|cached| now - cached.observed_at < REFRESH_INTERVAL_SECONDS as f64)
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
    let observed_at = crate::util::system_now();
    let history_saved = result.as_ref().is_ok_and(|quota| {
        history::record(
            &state.backend.accounts.quota_history,
            &identity,
            quota,
            observed_at as i64,
        )
        .is_ok()
    });
    let observation = Observation {
        observed_at,
        quota: result.as_ref().ok().cloned(),
        error_code: result.err(),
    };
    state.backend.diagnostics.journal.event(
        "info",
        "cpa_quota_refresh",
        &json!({"provider_id":id,"error_code":observation.error_code,"history_saved":history_saved}),
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

pub(crate) fn sample_due(state: &ServerState) {
    let ids: Vec<String> = match state.backend.configuration.read() {
        Ok(config) => config["providers"]
            .as_array()
            .expect("normalized providers")
            .iter()
            .filter(|provider| provider["enabled"] != false && supported(provider))
            .map(|provider| provider["id"].as_str().expect("provider ID").to_owned())
            .collect(),
        Err(_) => return,
    };
    for id in ids {
        if state.shutdown.load(std::sync::atomic::Ordering::Acquire) {
            break;
        }
        let _ = refresh(state, &id, false);
    }
}

pub(crate) fn history(
    state: &ServerState,
    id: &str,
    start: i64,
    end: i64,
) -> Option<Result<Value, &'static str>> {
    let provider = state.backend.configuration.read().ok()?["providers"]
        .as_array()
        .expect("normalized providers")
        .iter()
        .find(|provider| provider["id"] == id && supported(provider))?
        .clone();
    let key = provider_api_key(&provider, &state.backend.configuration.vault);
    let identity = identity(&provider, &key);
    let mut result = history::query(&state.backend.accounts.quota_history, &identity, start, end);
    if let Ok(payload) = &mut result {
        let cache = state
            .backend
            .provider_quotas
            .0
            .lock()
            .expect("CPA quota cache");
        let accounts = cache
            .get(&identity)
            .and_then(|item| item.quota.as_ref())
            .map(|quota| {
                quota
                    .accounts
                    .iter()
                    .map(|account| account.id.as_str())
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        payload["account_ids"] = json!(accounts);
    }
    Some(result)
}
