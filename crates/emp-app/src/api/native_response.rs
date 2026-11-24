use crate::server::http::error::HttpReply;
use emp_router::native_http::{NativeCompleteResponse, NativeHttpError};

pub(crate) fn complete_response(
    result: Result<NativeCompleteResponse, NativeHttpError>,
) -> HttpReply {
    let result = match result {
        Ok(result) => result,
        Err(error) => return error_reply(error),
    };
    result.headers.into_iter().fold(
        HttpReply::new(result.status, result.body).with_content_type(result.content_type),
        |reply, (name, value)| reply.with_header(name, value),
    )
}

pub(crate) fn error_reply(mut error: NativeHttpError) -> HttpReply {
    error.headers.remove("x-models-etag");
    error.headers.into_iter().fold(
        HttpReply::new(
            error.status,
            serde_json::to_vec(&error.body).expect("safe native error JSON"),
        ),
        |reply, (name, value)| reply.with_header(name, value),
    )
}
