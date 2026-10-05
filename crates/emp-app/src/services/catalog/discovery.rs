//! Provider discovery and selection own credentials, serialization and commits.
use crate::app::ServerState;
use crate::services::configuration::ChangeError;
use crate::services::observation::operation::{OperationObservation, observe};
use emp_router::discovery::discover_models;
use emp_state::provider_api_key;
use serde_json::{Value, json};

pub(crate) enum DiscoveryError {
    Invalid(String),
    Unavailable,
    Upstream(emp_router::RouterError),
}

impl From<ChangeError> for DiscoveryError {
    fn from(error: ChangeError) -> Self {
        match error {
            ChangeError::Invalid(message) => Self::Invalid(message),
            ChangeError::Unavailable => Self::Unavailable,
        }
    }
}

pub(crate) fn metadata(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> Result<Value, DiscoveryError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        "model_metadata",
        |receipt| metadata_inner(state, body, receipt),
    )
}

fn metadata_inner(
    state: &ServerState,
    body: &Value,
    receipt: &mut OperationObservation,
) -> Result<Value, DiscoveryError> {
    let Some(provider_id) = body.get("provider").and_then(Value::as_str) else {
        return Err(DiscoveryError::Invalid(
            "provider and model are required".into(),
        ));
    };
    let Some(mut model) = body.get("model").and_then(Value::as_str).map(str::to_owned) else {
        return Err(DiscoveryError::Invalid(
            "provider and model are required".into(),
        ));
    };
    let provider = configured_provider(state, provider_id)?;
    if let Some(upstream) = model.strip_prefix(&format!("{provider_id}/")) {
        model = upstream.to_owned();
    }
    let Some(provider) = provider.as_object() else {
        return Err(DiscoveryError::Unavailable);
    };
    let result = receipt.step("fetch_metadata", || {
        state
            .backend
            .transport
            .runtime
            .block_on(emp_router::discovery::model_metadata(
                &state.backend.transport.client,
                provider,
                &model,
            ))
    });
    result.map_err(DiscoveryError::Upstream)
}

pub(crate) fn discover(
    request_id: Option<&str>,
    state: &ServerState,
    body: &Value,
) -> Result<Value, DiscoveryError> {
    observe(
        &state.backend.diagnostics,
        request_id,
        "model_discovery",
        |receipt| {
            receipt.check("runtime_catalog_matches_target", None);
            discover_inner(state, body, receipt)
        },
    )
}

fn discover_inner(
    state: &ServerState,
    body: &Value,
    receipt: &mut OperationObservation,
) -> Result<Value, DiscoveryError> {
    let Some(provider_id) = body
        .get("provider")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
    else {
        return Err(DiscoveryError::Invalid("provider is required".into()));
    };
    let provider = configured_provider(state, provider_id)?;
    let discovered = {
        let _guard = match state.backend.configuration.discovery_lock.lock() {
            Ok(guard) => guard,
            Err(_) => return Err(DiscoveryError::Unavailable),
        };
        match receipt.step("discover_models", || {
            state.backend.transport.runtime.block_on(discover_models(
                &state.backend.transport.client,
                provider.as_object().expect("normalized provider"),
            ))
        }) {
            Ok(models) => models,
            Err(error) => return Err(DiscoveryError::Upstream(error)),
        }
    };
    let Some(selected) = body.get("selected").filter(|value| !value.is_null()) else {
        return Ok(
            json!({"provider":provider_id,"protocol":provider["protocol"],"available":discovered.len(),"models":discovered,"added":0}),
        );
    };
    match receipt.step("commit_selection", || {
        crate::services::catalog::select_models(state, provider_id, &discovered, selected)
    }) {
        Ok(selected) => {
            receipt.check("configuration_and_catalog_committed", Some(true));
            Ok(json!({
                "provider":provider_id,"protocol":provider["protocol"],"available":selected.available,
                "added":selected.added,"hidden":selected.hidden,"catalog_path":selected.catalog_path,
                "model_count":selected.model_count,
            }))
        }
        Err(error) => Err(error.into()),
    }
}

fn configured_provider(state: &ServerState, provider_id: &str) -> Result<Value, DiscoveryError> {
    let mut provider = {
        let config = match state.backend.configuration.read() {
            Ok(config) => config,
            Err(_) => return Err(DiscoveryError::Unavailable),
        };
        let Some(provider) = config
            .get("providers")
            .and_then(Value::as_array)
            .and_then(|providers| {
                providers.iter().find(|provider| {
                    provider.get("id").and_then(Value::as_str) == Some(provider_id)
                })
            })
            .filter(|provider| provider.get("enabled") != Some(&Value::Bool(false)))
        else {
            return Err(DiscoveryError::Invalid(format!(
                "provider is missing or disabled: {provider_id}"
            )));
        };
        provider.clone()
    };
    provider["api_key"] = json!(provider_api_key(
        &provider,
        &state.backend.configuration.vault
    ));
    Ok(provider)
}
