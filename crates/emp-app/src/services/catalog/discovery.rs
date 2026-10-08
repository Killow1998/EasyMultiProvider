//! Provider discovery and selection own credentials, serialization and commits.
use crate::app::ServerState;
use crate::services::configuration::ChangeError;
use crate::services::observation::operation::{OperationObservation, observe};
use emp_router::discovery::discover_models;
use serde_json::{Value, json};

pub(crate) enum DiscoveryError {
    Invalid(String),
    Unavailable,
    Upstream(emp_router::RouterError),
    LocalCli(&'static str),
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
    let _guard = state
        .backend
        .configuration
        .discovery_lock
        .lock()
        .map_err(|_| DiscoveryError::Unavailable)?;
    let saved = if body["cached"] == true {
        receipt.step("read_saved_models", || {
            super::provider_models::load(state, &provider).ok_or_else(|| {
                DiscoveryError::Invalid("Update the model list before saving a selection".into())
            })
        })?
    } else {
        match receipt.step("discover_models", || {
            if provider["execution_backend"] == "claude_cli"
                && provider["auth_mode"] == "claude_login"
            {
                crate::services::claude_cli::model_query::discover(state)
                    .map_err(DiscoveryError::LocalCli)
            } else {
                state
                    .backend
                    .transport
                    .runtime
                    .block_on(discover_models(
                        &state.backend.transport.client,
                        provider.as_object().expect("normalized provider"),
                    ))
                    .map_err(DiscoveryError::Upstream)
            }
        }) {
            Ok(models) => {
                let config = state
                    .backend
                    .configuration
                    .read()
                    .map_err(|_| DiscoveryError::Unavailable)?;
                if !super::provider_models::unchanged(state, &config, &provider) {
                    return Err(DiscoveryError::Invalid(
                        "Service changed; update the model list".into(),
                    ));
                }
                receipt
                    .step("save_model_list", || {
                        super::provider_models::save(state, &provider, models)
                    })
                    .map_err(|_| DiscoveryError::Unavailable)?
            }
            Err(error) => return Err(error),
        }
    };
    let Some(selected) = body.get("selected").filter(|value| !value.is_null()) else {
        return Ok(
            json!({"provider":provider_id,"protocol":provider["protocol"],"available":saved.models.len(),"models":saved.models,"updated_at":saved.updated_at,"added":0}),
        );
    };
    match receipt.step("commit_selection", || {
        crate::services::catalog::select_models(
            state,
            provider_id,
            &saved.models,
            selected,
            &provider,
        )
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
    let config = state
        .backend
        .configuration
        .read()
        .map_err(|_| DiscoveryError::Unavailable)?;
    super::provider_models::provider(state, &config, provider_id).ok_or_else(|| {
        DiscoveryError::Invalid(format!("provider is missing or disabled: {provider_id}"))
    })
}

pub(crate) fn saved_models(
    state: &ServerState,
    provider_id: &str,
) -> Result<Value, DiscoveryError> {
    let provider = configured_provider(state, provider_id)?;
    let saved = super::provider_models::load(state, &provider);
    Ok(match saved {
        Some(saved) => json!({"models":saved.models,"updated_at":saved.updated_at,"cached":true}),
        None => json!({"models":[],"updated_at":null,"cached":false}),
    })
}
