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
