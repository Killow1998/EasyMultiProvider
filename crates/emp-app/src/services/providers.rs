use crate::app::ServerState;
use emp_state::VaultStore;
use emp_state::config::{LearnedProtocol, ProtocolSave, remember_learned_protocol};
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

pub(crate) fn save_learned_protocol(state: &ServerState, candidate: Option<LearnedProtocol>) {
    let Some(candidate) = candidate else {
        return;
    };
    let configuration = &state.backend.configuration;
    let Ok(mut config) = configuration.edit() else {
        return;
    };
    match remember_learned_protocol(&config, &candidate, configuration.official_models) {
        Ok(ProtocolSave::Updated(updated)) => {
            let _ = config.commit(&updated);
        }
        Ok(ProtocolSave::IdentityChanged) => state.backend.diagnostics.journal.event(
            "warning",
            "protocol_observation_discarded",
            &json!({
                "provider_id": candidate.provider_id,
                "model_id": candidate.requested_model,
                "reason": "route_identity_changed",
            }),
        ),
        Ok(ProtocolSave::Superseded) | Err(_) => {}
    }
}
