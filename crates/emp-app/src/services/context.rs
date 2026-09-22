//! Persist content-free context evidence after an observed request outcome.
use crate::app::ServerState;
use emp_core::ResolvedRoute;
use serde_json::Value;

pub(crate) fn record(state: &ServerState, route: &ResolvedRoute, body: &Value, success: bool) {
    let Ok(payload) = emp_router::project_external_payload(route, body) else {
        return;
    };
    let assessment = emp_history::context::assess(
        route.provider.value(),
        route.model.value(),
        route.protocol.as_config_str(),
        &payload,
    );
    let Some(estimate) = assessment.input_estimate else {
        return;
    };
    let configuration = &state.backend.configuration;
    let Ok(mut current) = configuration.config.lock() else {
        return;
    };
    let mut updated = current.clone();
    let Some(model) = updated
        .get_mut("models")
        .and_then(Value::as_array_mut)
        .and_then(|models| {
            models
                .iter_mut()
                .find(|model| model["id"] == route.requested_model)
        })
        .and_then(Value::as_object_mut)
    else {
        return;
    };
    // Keep the request's identity even when configuration changed in flight.
    let mut observed = route.model.value().clone();
    if let Some(calibrations) = model.get("context_calibrations") {
        observed.insert("context_calibrations".into(), calibrations.clone());
    }
    if !emp_history::context::update_calibration(
        route.provider.value(),
        &mut observed,
        route.protocol.as_config_str(),
        estimate,
        success,
        &emp_state::observed_at_now(),
    ) {
        return;
    }
    model.insert(
        "context_calibrations".into(),
        observed["context_calibrations"].clone(),
    );
    if emp_state::save_configuration(
        &updated,
        Some(&configuration.config_path),
        &configuration.vault,
    )
    .is_ok()
        && let Ok(saved) = emp_state::load_configuration(Some(&configuration.config_path))
    {
        *current = saved;
    }
}
