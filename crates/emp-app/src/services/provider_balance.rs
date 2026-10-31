//! Official provider balances. Reads never run on the inference or startup path.
mod adapters;
use crate::app::ServerState;
use adapters::{Balance, official};
use emp_state::provider_api_key;
use serde::Serialize;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, sync::Mutex, time::Duration};

#[derive(Clone, Serialize)]
struct Observation {
    observed_at: f64,
    balance: Option<Balance>,
    error_code: Option<&'static str>,
}

#[derive(Default)]
pub(crate) struct Balances(Mutex<BTreeMap<String, Observation>>);

fn identity(provider: &Value, key: &str) -> String {
    format!(
        "{:x}",
        Sha256::digest(
            serde_json::to_vec(&(&provider["id"], &provider["base_url"], key,))
                .expect("balance identity JSON")
        )
    )
}

pub(crate) fn snapshot(state: &ServerState, config: &Value) -> Value {
    let cache = state
        .backend
        .provider_balances
        .0
        .lock()
        .expect("balance cache");
    let mut results = serde_json::Map::new();
    for provider in config["providers"]
        .as_array()
        .expect("normalized providers")
    {
        if provider["execution_backend"] == "claude_cli" {
            continue;
        }
        let Some(service) = official(provider["base_url"].as_str().expect("provider URL")) else {
            continue;
        };
        let key = provider_api_key(provider, &state.backend.configuration.vault);
        let identity = identity(provider, &key);
        results.insert(provider["id"].as_str().expect("provider ID").into(), json!({
            "identity":identity,
            "billing_url":service.billing_url,
            "refreshable":service.adapter.is_some() && provider["enabled"] != false && !key.is_empty(),
            "observation":cache.get(&identity),
        }));
    }
    Value::Object(results)
}

pub(crate) fn refresh(state: &ServerState, id: &str, force: bool) -> Option<Value> {
    let provider = {
        let config = state.backend.configuration.read().ok()?;
        config["providers"]
            .as_array()?
            .iter()
            .find(|provider| provider["id"] == id && provider["enabled"] != false)?
            .clone()
    };
    if provider["execution_backend"] == "claude_cli" {
        return None;
    }
    let service = official(provider["base_url"].as_str().expect("provider URL"))?;
    let adapter = service.adapter?;
    let key = provider_api_key(&provider, &state.backend.configuration.vault);
    let identity = identity(&provider, &key);
    let now = crate::util::system_now();
    if !force {
        let cache = state
            .backend
            .provider_balances
            .0
            .lock()
            .expect("balance cache");
        if let Some(cached) = cache
            .get(&identity)
            .filter(|cached| now - cached.observed_at < 60.0)
        {
            return Some(json!({"identity":identity,"observation":cached}));
        }
    }
    let result = if key.is_empty() {
        Err("balance_key_missing")
    } else {
        state.backend.transport.runtime.block_on(async {
            tokio::time::timeout(
                Duration::from_secs(5),
                adapters::query(
                    &state.backend.transport.client,
                    adapter,
                    provider["base_url"].as_str().expect("provider URL"),
                    &key,
                ),
            )
            .await
            .unwrap_or(Err("balance_timeout"))
        })
    };
    let observation = Observation {
        observed_at: crate::util::system_now(),
        balance: result.as_ref().ok().cloned(),
        error_code: result.err(),
    };
    state.backend.diagnostics.journal.event(
        if observation.error_code.is_some() {
            "warning"
        } else {
            "info"
        },
        "provider_balance_refresh",
        &json!({"provider_id":id,"error_code":observation.error_code}),
    );
    state
        .backend
        .provider_balances
        .0
        .lock()
        .expect("balance cache")
        .insert(identity.clone(), observation.clone());
    Some(json!({"identity":identity,"observation":observation}))
}
