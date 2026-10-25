//! One session owns its route identity, scope, socket and reconnect suppression.
use super::super::ObservedWebSocket;
use super::NativeSession;
use crate::app::ServerState;
use crate::services::native_connections::RouteKey;
use crate::services::observation::continuation::{ContinuationReason, NativeContinuation};
use emp_router::native_http::NativeWebSocketPlan;
use emp_router::native_metadata::native_response_headers;
use emp_transport::{ClientWebSocket, WebSocketError};
use serde_json::Value;
use std::collections::BTreeMap;
use std::time::Duration;

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

pub(super) struct NativeUpstreamConnection {
    url: String,
    proxy: Option<String>,
    identity: BTreeMap<String, String>,
    plaintext_collaboration: bool,
    pub(super) client: ClientWebSocket,
    pub(super) replay_inline_ids: bool,
}

impl NativeSession {
    pub(in crate::api::websocket) fn check_scope(
        &self,
        dialect: emp_core::Dialect,
        scope: &(Option<String>, Option<String>),
        previous: Option<&Value>,
        facts: &mut NativeContinuation,
    ) -> Option<ContinuationReason> {
        facts.has_prior = previous.is_some_and(|id| !id.is_null());
        facts.local_prior_matches = previous
            .and_then(Value::as_str)
            .map(|id| self.last_response_id.as_deref() == Some(id));
        facts.upstream_connection_available = Some(self.upstream.is_some());
        facts.scope_matches = self
            .last_response_id
            .as_ref()
            .map(|_| scope == &self.last_scope);
        let reason = if facts.has_prior && dialect != emp_core::Dialect::CodexNative {
            Some(ContinuationReason::UnsupportedDialect)
        } else if facts.has_prior && facts.scope_matches == Some(false) {
            Some(ContinuationReason::ScopeMismatch)
        } else {
            None
        };
        facts.reason = reason;
        reason
    }

    pub(super) fn prepare_connection(
        &mut self,
        state: &ServerState,
        plan: &NativeWebSocketPlan,
        previous: Option<&str>,
        facts: &mut NativeContinuation,
    ) -> Result<(RouteKey, bool), ContinuationReason> {
        let Self {
            upstream: native_upstream,
            last_response_id: last_native_response_id,
            http_only_routes,
            fallback_active,
            connected_before,
            ..
        } = self;
        let plan_identity = native_websocket_identity_headers(&plan.headers);
        let proxy = state
            .backend
            .transport
            .client
            .websocket_proxy_for(&plan.url);
        let endpoint_matches = native_upstream.as_ref().map(|upstream| {
            // Incremental turns inherit their socket's tool catalog. A policy
            // change requires full-history recovery on a fresh connection.
            upstream.url == plan.url
                && upstream.plaintext_collaboration == plan.plaintext_collaboration
                && proxy.as_ref().is_ok_and(|proxy| proxy == &upstream.proxy)
        });
        let identity_matches = native_upstream
            .as_ref()
            .map(|upstream| upstream.identity == plan_identity);
        let route_matches = endpoint_matches == Some(true) && identity_matches == Some(true);
        {
            facts.route_matches = endpoint_matches;
            facts.identity_matches = identity_matches;
            facts.connection_reused = Some(false);
        }
        if previous
            .is_some_and(|id| !route_matches || last_native_response_id.as_deref() != Some(id))
        {
            let reason = if *fallback_active {
                ContinuationReason::IncrementalFallbackUnavailable
            } else if endpoint_matches == Some(false) {
                ContinuationReason::RouteMismatch
            } else if identity_matches == Some(false) {
                ContinuationReason::IdentityMismatch
            } else if native_upstream.is_none() && *connected_before {
                ContinuationReason::UpstreamDisconnected
            } else {
                ContinuationReason::PriorResponseMismatch
            };
            facts.reason = Some(reason);
            return Err(reason);
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
            facts.connect_attempts += 1;
            match ClientWebSocket::connect_with_proxy(
                &plan.url,
                &plan.headers,
                Duration::from_secs(15),
                selected_proxy.as_deref(),
            ) {
                Ok(client) => {
                    facts.connections_opened += 1;
                    facts.handshake_status = Some(101);
                    *connected_before = true;
                    *fallback_active = false;
                    *native_upstream = Some(NativeUpstreamConnection {
                        url: plan.url.clone(),
                        proxy: selected_proxy.clone(),
                        identity: plan_identity.clone(),
                        plaintext_collaboration: plan.plaintext_collaboration,
                        client,
                        replay_inline_ids: false,
                    });
                    connected_now = true;
                }
                Err(error) => {
                    if error.is_upgrade_rejection() {
                        facts.handshake_status = Some(error.status());
                    }
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
        Ok((route_key, connected_now))
    }
}

impl NativeUpstreamConnection {
    pub(super) fn metadata(
        &self,
        plan: &NativeWebSocketPlan,
        connected_now: bool,
        websocket: &mut ObservedWebSocket<'_, '_>,
    ) -> Result<(), WebSocketError> {
        let selected = native_response_headers(
            &serde_json::json!({"headers":self.client.response_headers()}),
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
        if !state_headers.is_empty() {
            websocket.send_json(
                &serde_json::json!({"type":"response.metadata","headers":state_headers}),
            )?;
        }
        Ok(())
    }
}
