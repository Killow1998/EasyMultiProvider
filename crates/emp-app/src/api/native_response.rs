//! Native wire rendering; forwarding services preserve status, body and headers.
use crate::http::response::{response, status_text};
use emp_router::native_http::{NativeCompleteResponse, NativeHttpError};

pub(crate) fn complete_response(
    result: Result<NativeCompleteResponse, NativeHttpError>,
) -> Vec<u8> {
    let result = match result {
        Ok(result) => result,
        Err(error) => return error_response(error),
    };
    let headers = result
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    response(
        &format!("HTTP/1.1 {} {}", result.status, status_text(result.status)),
        &result.content_type,
        &result.body,
        &headers,
    )
}

pub(crate) fn error_response(mut error: NativeHttpError) -> Vec<u8> {
    error.headers.remove("x-models-etag");
    let headers = error
        .headers
        .iter()
        .map(|(name, value)| (name.as_str(), value.as_str()))
        .collect::<Vec<_>>();
    response(
        &format!("HTTP/1.1 {} {}", error.status, status_text(error.status)),
        "application/json",
        &serde_json::to_vec(&error.body).expect("safe native error JSON"),
        &headers,
    )
}
