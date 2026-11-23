use crate::app::ServerState;
use crate::server::http::error::JsonReply;
use crate::web::{AUDIO_TEST_WAV_BYTES, VISION_TEST_IMAGE_BYTES};
use base64::Engine as _;
use serde_json::json;

pub(crate) fn vision_test_image(_: &ServerState) -> JsonReply {
    let encoded = base64::engine::general_purpose::STANDARD.encode(VISION_TEST_IMAGE_BYTES);
    let body = json!({"data_url":format!("data:image/png;base64,{encoded}")});
    JsonReply::new(200, body.to_string().into_bytes()).with_header("Cache-Control", "no-store")
}

pub(crate) fn audio_test_sound(_: &ServerState) -> JsonReply {
    let encoded = base64::engine::general_purpose::STANDARD.encode(AUDIO_TEST_WAV_BYTES);
    let body = json!({"format":"wav","data":encoded});
    JsonReply::new(200, body.to_string().into_bytes()).with_header("Cache-Control", "no-store")
}

pub(crate) fn capabilities(state: &ServerState) -> JsonReply {
    let config = match state.backend.configuration.read() {
        Ok(config) => config.clone(),
        Err(_) => return JsonReply::error(409, "capability state is unavailable", None),
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
        let (Some(provider), Some(model)) = (provider.as_object(), model.as_object()) else {
            continue;
        };
        let mut record = emp_domain::capability_view::capability_record(provider, model);
        let protocol = record["capabilities"]["effective_protocol"]["value"]
            .as_str()
            .unwrap_or("unknown");
        let context = emp_context::context::status(provider, model, protocol);
        record["context"] = context;
        records.push(record);
    }
    JsonReply::new(
        200,
        json!({"capabilities":records}).to_string().into_bytes(),
    )
}

pub(crate) fn request_limits(state: &ServerState) -> JsonReply {
    match state.backend.transport.request_limits.snapshot() {
        Ok(snapshot) => {
            let value = serde_json::to_value(snapshot).expect("limit snapshot is serializable");
            JsonReply::new(200, serde_json::to_vec(&value).expect("inspection JSON"))
        }
        Err(_) => JsonReply::error(503, "request limits are unavailable", None),
    }
}
