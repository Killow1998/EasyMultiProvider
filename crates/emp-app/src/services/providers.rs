use crate::app::ServerState;
use emp_domain::ResolvedRoute;
use emp_state::VaultStore;
use emp_state::config::{remember_resolved_protocol_with_registry, remember_route_protocol};
use emp_state::provider_api_key;
use serde_json::{Value, json};

pub(crate) fn hydrate_provider_keys(config: &mut Value, vault: &VaultStore) {
    let Some(providers) = config.get_mut("providers").and_then(Value::as_array_mut) else {
        return;
    };
    for provider in providers {
        let key = provider_api_key(provider, vault);
        if let Some(provider) = provider.as_object_mut() {
            provider.insert("api_key".to_owned(), Value::String(key));
        }
    }
}

pub(crate) fn persist_protocol_observation(state: &ServerState, route: &ResolvedRoute) {
    let Ok(mut config) = state.backend.configuration.edit() else {
        return;
    };
    let Ok(Some(updated)) = remember_resolved_protocol_with_registry(
        &config,
        &route.provider_id,
        &route.requested_model,
        route.protocol.as_config_str(),
        state.backend.configuration.official_models,
    ) else {
        return;
    };
    let _ = config.commit(&updated);
}

pub(crate) fn save_learned_protocol(state: &ServerState, route: &ResolvedRoute, observed_at: &str) {
    let configuration = &state.backend.configuration;
    let Ok(mut config) = configuration.edit() else {
        return;
    };
    match remember_route_protocol(&config, route, observed_at, configuration.official_models) {
        Ok(Some(updated)) => {
            let _ = config.commit(&updated);
        }
        Ok(None) => state.backend.diagnostics.journal.event(
            "warning",
            "protocol_observation_discarded",
            &json!({
                "provider_id": route.provider_id,
                "model_id": route.requested_model,
                "reason": "route_identity_changed",
            }),
        ),
        Err(_) => {}
    }
}
