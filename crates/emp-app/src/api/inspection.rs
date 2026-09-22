//! Read-only management data used by existing model testing and limit displays.
use crate::app::ServerState;
use crate::http::request::Request;
use crate::http::response::{json_error_response, response, status_text};
use crate::web::VISION_TEST_IMAGE_BYTES;
use base64::Engine as _;
use serde_json::json;

pub(crate) fn read_request(request: Request<'_>, state: &ServerState) -> Vec<u8> {
    let (payload, headers) = match request.raw_path() {
        "/api/models/vision-test-image" => (
            json!({"data_url":format!("data:image/png;base64,{}", base64::engine::general_purpose::STANDARD.encode(VISION_TEST_IMAGE_BYTES))}),
            vec![("Cache-Control", "no-store")],
        ),
        "/api/capabilities" => {
            let config = match state.backend.configuration.config.lock() {
                Ok(config) => config.clone(),
                Err(_) => {
                    return json_error_response(
                        409,
                        status_text(409),
                        "capability state is unavailable",
                        None,
                        &[],
                    );
                }
            };
            let mut records = Vec::new();
            for model in config["models"]
                .as_array()
                .into_iter()
                .flatten()
                .filter(|item| item["enabled"] != false)
            {
                let Some(provider) = config["providers"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .find(|item| item["id"] == model["provider"] && item["enabled"] != false)
                else {
                    continue;
                };
                let (Some(provider), Some(model)) = (provider.as_object(), model.as_object())
                else {
                    continue;
                };
                let mut record = emp_core::capability_view::capability_record(provider, model);
                let protocol = record["capabilities"]["effective_protocol"]["value"]
                    .as_str()
                    .unwrap_or("unknown");
                let context = emp_history::context::status(provider, model, protocol);
                record["context"] = context;
                records.push(record);
            }
            (json!({"capabilities":records}), vec![])
        }
        "/api/request-limits" => {
            let snapshot = match state.backend.transport.request_limits.snapshot() {
                Ok(snapshot) => snapshot,
                Err(_) => {
                    return json_error_response(
                        503,
                        status_text(503),
                        "request limits are unavailable",
                        None,
                        &[],
                    );
                }
            };
            (
                serde_json::to_value(snapshot).expect("limit snapshot is serializable"),
                vec![],
            )
        }
        _ => unreachable!("inspection paths are selected by the HTTP dispatcher"),
    };
    response(
        "HTTP/1.1 200 OK",
        "application/json",
        &serde_json::to_vec(&payload).expect("inspection JSON"),
        &headers,
    )
}
