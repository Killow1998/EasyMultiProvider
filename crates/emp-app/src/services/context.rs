//! Persist content-free context evidence after an observed request outcome.
use crate::app::ServerState;
use emp_core::ResolvedRoute;
use serde_json::Value;

pub(crate) fn payload(state: &ServerState, route: &ResolvedRoute, body: &Value) -> Option<Value> {
    if route.dialect == emp_core::Dialect::CodexNative {
        let body = body.as_object()?;
        let config = state.backend.configuration.config.lock().ok()?.clone();
        let plaintext = config["providers"]
            .as_array()
            .into_iter()
            .flatten()
            .any(|provider| provider["auth_mode"] == "api_key");
        emp_router::native_http::NativeRouter::new(&state.backend.transport.client)
            .prepare_websocket(route, body, plaintext, std::collections::BTreeMap::new())
            .ok()
            .map(|plan| plan.payload)
    } else {
        emp_router::project_external_payload(route, body).ok()
    }
}

pub(crate) fn record(state: &ServerState, route: &ResolvedRoute, body: &Value, success: bool) {
    if let Some(payload) = payload(state, route, body) {
        record_payload(state, route, &payload, success);
    }
}

pub(crate) fn record_payload(
    state: &ServerState,
    route: &ResolvedRoute,
    payload: &Value,
    success: bool,
) {
    let assessment = emp_history::context::assess(
        route.provider.value(),
        route.model.value(),
        route.protocol.as_config_str(),
        payload,
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

pub(crate) fn outcome(event: &Value) -> Option<bool> {
    let kind = event["type"].as_str()?;
    let response = &event["response"];
    if kind == "response.completed"
        && matches!(response["status"].as_str(), None | Some("completed"))
        && (response["error"].is_null()
            || response["error"]
                .as_object()
                .is_some_and(|error| error.is_empty()))
    {
        return Some(true);
    }
    if matches!(kind, "response.failed" | "response.incomplete" | "error") {
        let error = event.get("error").or_else(|| response.get("error"))?;
        if emp_router::is_explicit_context_error(
            200,
            "application/json",
            &serde_json::to_vec(error).ok()?,
        ) {
            return Some(false);
        }
    }
    None
}

pub(crate) fn record_event(
    state: &ServerState,
    route: &ResolvedRoute,
    body: &Value,
    event: &Value,
) {
    if let Some(success) = outcome(event) {
        record(state, route, body, success);
    }
}
