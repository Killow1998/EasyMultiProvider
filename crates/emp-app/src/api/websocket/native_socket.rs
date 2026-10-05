//! Native upstream connection reuse, incremental ownership and HTTP fallback.
use super::Turn;
use crate::api::failure_response::{safe_failure_reason, stream_error_code};
use crate::app::ServerState;
use crate::services::events::stream_event_activity;
use crate::services::events::terminal_stream_event;
use crate::services::{activity::ActivityGuard, native};
use crate::util::random_hex;
use emp_core::ResolvedRoute;
use emp_router::native_metadata::native_response_headers;
use emp_transport::{ClientWebSocket, FailureClass, public_failure_message};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

#[derive(Default)]
pub(super) struct NativeSession {
    upstream: Option<NativeUpstreamConnection>,
    pub(super) last_response_id: Option<String>,
    pub(super) last_scope: (Option<String>, Option<String>),
    http_only_routes: BTreeSet<(String, Option<String>, BTreeMap<String, String>)>,
}

pub(super) enum NativeTurnResult<'a> {
    Finished,
    Closed,
    HttpFallback(Option<ActivityGuard<'a>>),
}

pub(super) fn native_stream_error_value(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
) -> Value {
    let mut error = serde_json::json!({
        "code":stream_error_code(error_class),
        "message":format!("HTTP {status}: {}",public_failure_message(error_class,failure_reason,status)),
        "status":status,"error_class":error_class.as_str()
    });
    if let Some(reason) = failure_reason {
        error["failure_reason"] = Value::String(safe_failure_reason(reason));
    }
    serde_json::json!({"type":"response.failed","response":{"id":response_id,"object":"response","status":"failed","error":error}})
}

fn native_stream_error_value_for_route(
    status: u16,
    error_class: FailureClass,
    failure_reason: Option<&str>,
    response_id: &str,
    route: &ResolvedRoute,
    output_started: bool,
) -> Value {
    let mut event = native_stream_error_value(status, error_class, failure_reason, response_id);
    event["response"]["error"]["message"] = Value::String(format!(
        "HTTP {status}: {}",
        crate::services::failure_feedback::message(
            route,
            error_class,
            failure_reason,
            status,
            output_started,
            None,
        )
    ));
    event
}

fn native_websocket_identity_headers(
    headers: &BTreeMap<String, String>,
) -> BTreeMap<String, String> {
    headers
        .iter()
        .filter(|(name, _)| {
            matches!(
                name.to_ascii_lowercase().as_str(),
                "authorization" | "chatgpt-account-id"
            )
        })
        .map(|(name, value)| (name.to_ascii_lowercase(), value.clone()))
        .collect()
}

struct NativeUpstreamConnection {
    url: String,
    proxy: Option<String>,
    identity: BTreeMap<String, String>,
    client: ClientWebSocket,
}

impl NativeSession {
    pub(super) fn serve<'a>(
        &mut self,
        state: &'a ServerState,
        turn: &Turn,
        websocket: &mut super::ObservedWebSocket<'_, '_>,
    ) -> NativeTurnResult<'a> {
        let Turn {
            route,
            config,
            body: request_body,
            headers: request_headers,
            scope: request_scope,
            ..
        } = turn;
        let Self {
            upstream: native_upstream,
            last_response_id: last_native_response_id,
            last_scope: last_native_scope,
            http_only_routes,
        } = self;
        let mut native_turn_activity = None;
        let plan = match native::websocket_plan(state, route, config, request_body, request_headers)
        {
            Ok(plan) => plan,
            Err(error) => {
                let _=websocket.send_json(&serde_json::json!({"type":"error","status":error.status,"error":error.body["error"]}));
                return NativeTurnResult::Finished;
            }
        };
        let previous = request_body
            .get("previous_response_id")
            .and_then(Value::as_str);
        let plan_identity = native_websocket_identity_headers(&plan.headers);
        let proxy = state
            .backend
            .transport
            .client
            .websocket_proxy_for(&plan.url);
        let route_matches = native_upstream.as_ref().is_some_and(|upstream| {
            upstream.url == plan.url
                && proxy.as_ref().is_ok_and(|proxy| proxy == &upstream.proxy)
                && upstream.identity == plan_identity
        });
        if previous
            .is_some_and(|id| !route_matches || last_native_response_id.as_deref() != Some(id))
        {
            *last_native_response_id = None;
            let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
            return NativeTurnResult::Finished;
        }
        if !route_matches {
            *native_upstream = None;
            *last_native_response_id = None;
        }
        let route_key = (
            plan.url.clone(),
            proxy.as_ref().ok().cloned().flatten(),
            plan_identity.clone(),
        );
        let mut connected_now = false;
        if native_upstream.is_none()
            && !http_only_routes.contains(&route_key)
            && state
                .backend
                .transport
                .native_connections
                .allowed(&route_key)
            && let Ok(selected_proxy) = &proxy
        {
            match ClientWebSocket::connect_with_proxy(
                &plan.url,
                &plan.headers,
                Duration::from_secs(15),
                selected_proxy.as_deref(),
            ) {
                Ok(client) => {
                    *native_upstream = Some(NativeUpstreamConnection {
                        url: plan.url.clone(),
                        proxy: selected_proxy.clone(),
                        identity: plan_identity.clone(),
                        client,
                    });
                    connected_now = true;
                }
                Err(error) => {
                    // No response.create frame has been sent. Match Python's
                    // HTTP fallback and avoid repeating failed handshakes.
                    if matches!(error.status(), 400 | 404 | 405 | 415 | 426 | 501) {
                        http_only_routes.insert(route_key.clone());
                    }
                    state
                        .backend
                        .transport
                        .native_connections
                        .defer(route_key.clone());
                }
            }
        }
        if let Some(upstream) = native_upstream.as_mut() {
            let client = &mut upstream.client;
            {
                let selected = native_response_headers(
                    &serde_json::json!({"headers":client.response_headers()}),
                    &plan.requested_model,
                    &plan.upstream_model,
                );
                let state_headers = selected
                    .into_iter()
                    .filter(|(name, _)| {
                        !name.eq_ignore_ascii_case("x-models-etag")
                            && (connected_now
                                || name.eq_ignore_ascii_case("openai-model")
                                || name.eq_ignore_ascii_case("x-openai-model"))
                    })
                    .collect::<serde_json::Map<_, _>>();
                if !state_headers.is_empty()
                        && websocket
                            .send_json(&serde_json::json!({"type":"response.metadata","headers":state_headers}))
                            .is_err()
                    {
                        return NativeTurnResult::Closed;
                    }
            }
            let owner = emp_state::usage::account_owner(&plan.headers);
            let mut usage = crate::services::request_outcome::RequestOutcome::new(
                state,
                route,
                &Value::Object(request_body.clone()),
                request_headers,
                Some(&owner),
                "responses",
            )
            .transport("websocket");
            native_turn_activity = Some(state.backend.activity.begin(
                crate::services::activity::ActivityIdentity::from_route(route),
            ));
            usage.dispatch();
            if client.send_json(&plan.payload).is_err() {
                *native_upstream = None;
                *last_native_response_id = None;
                let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                let error = native_stream_error_value_for_route(
                    502,
                    FailureClass::Network,
                    Some("network"),
                    &id,
                    route,
                    false,
                );
                let _ = websocket.send_json(&error);
                return NativeTurnResult::Finished;
            }
            let mut terminal = false;
            let mut terminal_success = false;
            let mut completed_id = None;
            let mut projected_error = false;
            let mut received_upstream_event = false;
            let mut delivered_output = false;
            while let Ok(Some(event)) = client.receive_json() {
                received_upstream_event = true;
                let event = match plan.project_event(&event) {
                    Ok(event) => event,
                    Err(error) => {
                        let _ = websocket.send_json(&serde_json::json!({
                            "type":"error",
                            "status":error.status,
                            "error":error.body["error"]
                        }));
                        projected_error = true;
                        break;
                    }
                };
                if event.get("type").and_then(Value::as_str) == Some("response.completed") {
                    completed_id = event
                        .get("response")
                        .and_then(|response| response.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_owned);
                }
                usage.observe(&event);
                // Native WS terminal policy preserves Python's generic
                // stream failures; only successful turns calibrate here.
                if crate::services::context::outcome(&event) == Some(true) {
                    crate::services::context::record_payload(state, route, &plan.payload, true);
                }
                terminal = terminal_stream_event(&event);
                terminal_success =
                    terminal && crate::services::context::outcome(&event) == Some(true);
                if websocket.send_json(&event).is_err() {
                    return NativeTurnResult::Closed;
                }
                delivered_output |= stream_event_activity(&event).0;
                if terminal {
                    break;
                }
            }
            if terminal {
                state
                    .backend
                    .transport
                    .native_connections
                    .available(&route_key);
                if terminal_success {
                    *last_native_response_id = completed_id;
                    *last_native_scope = request_scope.clone();
                }
                return NativeTurnResult::Finished;
            }
            if !projected_error
                && !received_upstream_event
                && client.peer_close_code() == Some(1009)
                && previous.is_none()
            {
                // Python retries this proven pre-event message-size
                // rejection through the full HTTP/zstd path. Restrict
                // replay to a full request; incremental turns remain
                // owned by the existing previous-response recovery path.
                http_only_routes.insert(route_key.clone());
                *native_upstream = None;
                *last_native_response_id = None;
            } else {
                *native_upstream = None;
                *last_native_response_id = None;
                if projected_error {
                    return NativeTurnResult::Finished;
                }
                if previous.is_some() {
                    let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
                } else {
                    let id = format!("resp_{}", random_hex(16).unwrap_or_else(|_| "0".repeat(32)));
                    let error = native_stream_error_value_for_route(
                        502,
                        FailureClass::StreamIncomplete,
                        Some("stream_incomplete"),
                        &id,
                        route,
                        delivered_output,
                    );
                    let _ = websocket.send_json(&error);
                }
                return NativeTurnResult::Finished;
            }
        }
        if previous.is_some() {
            let _=websocket.send_json(&serde_json::json!({"type":"error","error":{"code":"previous_response_not_found","message":"Previous response was not found. Retrying the full request."}}));
            return NativeTurnResult::Finished;
        }
        NativeTurnResult::HttpFallback(native_turn_activity)
    }
}
