use crate::app::ServerState;
use crate::services::catalog::response_catalog_etag;
use crate::services::disconnect::DisconnectMonitor;
use crate::services::disconnect::DisconnectRace;
use crate::services::request_outcome::RequestOutcome;
use emp_core::ResolvedRoute;
use emp_router::ProjectionIds;
use emp_router::native_http::NativeRouter;
use emp_router::native_http::NativeStream;
use emp_router::native_http::NativeWebSocketPlan;
use emp_router::native_http::{NativeCompleteResponse, NativeHttpError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::Mutex;
mod auth;
use auth::resolve_headers;
pub(crate) mod images;

pub(crate) fn plaintext_collaboration(config: &Value) -> bool {
    config["external_collaboration"] != "off"
}

fn replace_catalog_etag(state: &ServerState, headers: &mut BTreeMap<String, String>) {
    headers.remove("x-models-etag");
    if let Some(etag) = response_catalog_etag(state) {
        headers.insert("X-Models-Etag".to_owned(), etag);
    }
}

pub(crate) fn open_stream_result<'a>(
    state: &'a ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    previous_usage: Option<RequestOutcome<'a>>,
) -> Result<(NativeStream, RequestOutcome<'a>), NativeHttpError> {
    match open_stream_result_with_monitor(
        state,
        route,
        config,
        body,
        incoming,
        ids,
        None,
        previous_usage,
    )? {
        CancellableNativeStreamOpen::Opened(stream) => Ok(*stream),
        CancellableNativeStreamOpen::Disconnected => {
            unreachable!("non-cancellable native open cannot observe a disconnect")
        }
    }
}

pub(crate) enum CancellableNativeStreamOpen<'a> {
    Opened(Box<(NativeStream, RequestOutcome<'a>)>),
    Disconnected,
}

pub(crate) fn open_stream_cancellable<'a>(
    state: &'a ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: &mut DisconnectMonitor,
) -> Result<CancellableNativeStreamOpen<'a>, NativeHttpError> {
    open_stream_result_with_monitor(
        state,
        route,
        config,
        body,
        incoming,
        ids,
        Some(monitor),
        None,
    )
}

#[expect(
    clippy::too_many_arguments,
    reason = "Native WS fallback must transfer its existing receipt alongside the stream inputs"
)]
fn open_stream_result_with_monitor<'a>(
    state: &'a ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    ids: &ProjectionIds,
    monitor: Option<&mut DisconnectMonitor>,
    previous_usage: Option<RequestOutcome<'a>>,
) -> Result<CancellableNativeStreamOpen<'a>, NativeHttpError> {
    let mut usage = previous_usage
        .unwrap_or_else(|| RequestOutcome::new(state, route, body, incoming, None, "responses"));
    let result = {
        let receipt = Mutex::new(&mut usage);
        let on_attempt = || receipt.lock().unwrap().dispatch();
        let on_retry = |decision| receipt.lock().unwrap().native_retry(decision);
        let router = NativeRouter::new(&state.backend.transport.client)
            .with_retry_observer(&on_retry)
            .with_attempt_observer(&on_attempt);
        let open = router.open_stream(
            route,
            body.as_object().unwrap(),
            plaintext_collaboration(config),
            ids,
            |refresh| {
                let headers = resolve_headers(state, route, incoming, true, refresh)?;
                receipt.lock().unwrap().native_credentials(
                    route,
                    &headers.headers,
                    headers.native_account.as_deref(),
                );
                Ok(headers.headers)
            },
        );
        crate::services::disconnect::raced(&state.backend.transport.runtime, monitor, open)
    };
    let result = match result {
        DisconnectRace::Ready(result) => result,
        DisconnectRace::Disconnected => {
            crate::services::observation::request_cancelled(state, route, incoming);
            usage.disconnected();
            return Ok(CancellableNativeStreamOpen::Disconnected);
        }
    };
    match result {
        Ok(mut stream) => {
            replace_catalog_etag(state, &mut stream.headers);
            Ok(CancellableNativeStreamOpen::Opened(Box::new((
                stream, usage,
            ))))
        }
        Err(mut error) => {
            usage.native_error(&error);
            crate::services::failure_feedback::annotate_native(&mut error, route);
            Err(error)
        }
    }
}

pub(crate) fn complete(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<NativeCompleteResponse, NativeHttpError> {
    buffered(state, route, config, body, incoming, "responses")
}

pub(crate) fn compact(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<NativeCompleteResponse, NativeHttpError> {
    buffered(state, route, config, body, incoming, "compact")
}

fn buffered(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
    operation: &str,
) -> Result<NativeCompleteResponse, NativeHttpError> {
    let mut usage = RequestOutcome::new(state, route, body, incoming, None, operation);
    let result = {
        let receipt = Mutex::new(&mut usage);
        let on_attempt = || receipt.lock().unwrap().dispatch();
        let on_retry = |decision| receipt.lock().unwrap().native_retry(decision);
        let router = NativeRouter::new(&state.backend.transport.client)
            .with_retry_observer(&on_retry)
            .with_attempt_observer(&on_attempt);
        let mut headers = |refresh| {
            let headers = resolve_headers(state, route, incoming, false, refresh)?;
            receipt.lock().unwrap().native_credentials(
                route,
                &headers.headers,
                headers.native_account.as_deref(),
            );
            Ok(headers.headers)
        };
        let runtime = &state.backend.transport.runtime;
        if operation == "compact" {
            runtime.block_on(router.execute_compact(
                route,
                body.as_object().unwrap(),
                plaintext_collaboration(config),
                &mut headers,
            ))
        } else {
            runtime.block_on(router.execute_complete(
                route,
                body.as_object().unwrap(),
                plaintext_collaboration(config),
                true,
                &mut headers,
            ))
        }
    };
    match result {
        Ok(mut result) => {
            usage.http_status(result.status);
            usage.upstream_observation(&result.observation);
            if let Ok(value) = serde_json::from_slice::<Value>(&result.body) {
                usage.observe(&value);
                if operation == "responses" {
                    crate::services::context::record_event(
                        state,
                        route,
                        body,
                        &serde_json::json!({"type":format!("response.{}",value["status"].as_str().unwrap_or("unknown")),"response":value}),
                    );
                }
            }
            replace_catalog_etag(state, &mut result.headers);
            Ok(result)
        }
        Err(mut error) => {
            usage.native_error(&error);
            if operation == "responses" {
                crate::services::context::record_event(
                    state,
                    route,
                    body,
                    &serde_json::json!({"type":"error","error":error.body["error"]}),
                );
            }
            crate::services::failure_feedback::annotate_native(&mut error, route);
            Err(error)
        }
    }
}

pub(crate) fn websocket_plan(
    state: &ServerState,
    route: &ResolvedRoute,
    config: &Value,
    body: &Value,
    incoming: &BTreeMap<String, String>,
) -> Result<(NativeWebSocketPlan, Option<String>), NativeHttpError> {
    let headers = resolve_headers(state, route, incoming, true, false)?;
    NativeRouter::new(&state.backend.transport.client)
        .prepare_websocket(
            route,
            body.as_object().unwrap(),
            plaintext_collaboration(config),
            headers.headers,
        )
        .map(|plan| (plan, headers.native_account))
}
