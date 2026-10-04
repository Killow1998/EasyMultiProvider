//! Completion state for one routed operation. Each terminal outcome updates
//! routing eligibility, usage/diagnostics and management activity exactly once.
use crate::app::ServerState;
use crate::services::observation::{request_tokens_per_second, retry_scheduled};
use emp_core::ResolvedRoute;
use emp_state::usage::{account_owner, reported_usage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::time::Instant;
mod shape;

#[cfg(all(test, feature = "hotpath"))]
pub(crate) fn profile_request_bytes(body: &Value) -> usize {
    shape::request_bytes(body)
}

pub(crate) struct RequestOutcome<'a> {
    state: &'a ServerState,
    identity: Option<crate::services::activity::ActivityIdentity>,
    started: Instant,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    event: Value,
    finalized: bool,
}
impl<'a> RequestOutcome<'a> {
    pub(crate) fn new(
        state: &'a ServerState,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        owner: Option<&str>,
        operation: &str,
    ) -> Self {
        let category = match route
            .provider
            .value()
            .get("auth_mode")
            .and_then(Value::as_str)
        {
            Some("account") => "subscription",
            Some("forward" | "native") => "native",
            _ => "external",
        };
        let owner = if category == "external" {
            route.provider_id.clone()
        } else {
            owner
                .map(str::to_owned)
                .unwrap_or_else(|| account_owner(incoming))
        };
        let owner = if owner.is_empty() {
            format!("unconfirmed:{}", route.provider_id)
        } else {
            owner
        };
        let turn = body
            .as_object()
            .and_then(|body| emp_history::request_history_anchor(body, incoming).ok())
            .and_then(|anchor| anchor.turn_id)
            .unwrap_or_default();
        let mut event = json!({"route":operation,"usage_category":category,"usage_owner":owner,"upstream_model":route.upstream_model,"route_model":route.requested_model,"usage_turn":turn,"service_tier":body["service_tier"].as_str().filter(|s|!s.is_empty()).unwrap_or("default"),
            "provider_id":route.provider_id,"model_id":route.requested_model,"client_model":body["model"],"resolved_protocol":route.protocol,"dialect":route.dialect,"route_source":route.source,"endpoint_fingerprint":route.endpoint_fingerprint,"deployment_identity":route.deployment_identity,
            "transport":if body["stream"]==true{"sse"}else{"http"},"protocol_decision":"explicit","fallback_reason":"none","model_trace_source":"emp_dispatch","request_bytes":shape::request_bytes(body),"performance_schema":3,"speed_mode":if matches!(body["service_tier"].as_str(),Some("fast"|"priority"|"ultrafast")){"fast"}else{"standard"}});
        event
            .as_object_mut()
            .unwrap()
            .extend(shape::facts(body, incoming).as_object().unwrap().clone());
        let record = emp_state::diagnostics::schema::route_record(
            &event,
            &state.backend.diagnostics.journal,
        );
        state.backend.diagnostics.journal.event("info", "model_operation_started", &json!({
            "request_id":record["request_id"], "protocol":record["protocol"], "transport":record["transport"],
            "provider_id":record["provider_id"], "model_id":record["model_id"], "route":operation,
        }));
        let identity = crate::services::activity::ActivityIdentity::from_route(route);
        state
            .backend
            .activity
            .observe_request(&event, identity.as_ref(), false);
        Self {
            state,
            identity,
            started: Instant::now(),
            first_token: None,
            last_token: None,
            finalized: false,
            event,
        }
    }
    pub(crate) fn observe(&mut self, event: &Value) {
        if self.finalized {
            return;
        }
        let now = Instant::now();
        let response = event
            .get("response")
            .filter(|value| value.is_object())
            .unwrap_or(event);
        if self.event["dialect"] == "codex_native" {
            self.reported_model(response["model"].as_str().filter(|s| !s.is_empty()));
        }
        let kind = event["type"].as_str().unwrap_or("");
        if let Some(status) = response["status"]
            .as_str()
            .filter(|status| matches!(*status, "completed" | "failed" | "incomplete"))
        {
            self.event["response_status"] = json!(status);
        }
        let (output, tool) = crate::services::events::stream_event_activity(event);
        if output {
            self.event["output_emitted"] = json!(true);
        }
        if tool {
            self.event["tool_activity"] = json!(true);
        }
        if matches!(
            kind,
            "response.output_text.delta"
                | "response.refusal.delta"
                | "response.function_call_arguments.delta"
                | "response.custom_tool_call_input.delta"
        ) && event["delta"]
            .as_str()
            .is_some_and(|delta| !delta.is_empty())
        {
            self.first_token.get_or_insert(now);
            self.last_token = Some(now);
        }
        if !kind.is_empty() && self.event.get("upstream_first_event_ms").is_none() {
            self.event["upstream_first_event_ms"] =
                json!(now.duration_since(self.started).as_millis() as u64);
        }
        self.event
            .as_object_mut()
            .unwrap()
            .extend(reported_usage(event));
        if kind.is_empty() && self.event["dialect"] == "codex_native" {
            // Native complete responses retain their HTTP boundary verbatim.
        } else if response["status"] == "completed" || kind == "response.completed" {
            self.status(200, "none");
        } else if response["status"] == "incomplete" || kind == "response.incomplete" {
            self.status(
                200,
                match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "output_limit",
                    Some("content_filter") => "content_filter",
                    _ => "stream_incomplete",
                },
            );
        } else if response["status"] == "failed" || matches!(kind, "response.failed" | "error") {
            self.status(502, "stream_error");
        }
        if matches!(
            event["type"].as_str(),
            Some("response.completed" | "response.incomplete" | "response.failed" | "error")
        ) {
            self.event["terminal_event_observed"] = json!(true);
            self.event["phase"] = json!("terminal_validation");
            self.finish();
        }
    }
    pub(crate) fn started_at(mut self, started: Instant) -> Self {
        self.started = started;
        self
    }
    pub(crate) fn reported_model(&mut self, model: Option<&str>) {
        if let Some(model) = model
            && self.event["response_model"] != model
        {
            self.event["response_model"] = json!(model);
            self.event["response_model_source"] = json!("upstream_response");
            self.publish(false);
        }
    }
    pub(crate) fn dispatch(&self) {
        let mut event = self.event.clone();
        event["dispatch_started"] = json!(true);
        self.state
            .backend
            .activity
            .observe_request(&event, self.identity.as_ref(), false);
    }
    pub(crate) fn candidate(&mut self, route: &ResolvedRoute) {
        self.event["resolved_protocol"] = json!(route.protocol);
        self.event["dialect"] = json!(route.dialect);
        self.event["endpoint_fingerprint"] = json!(route.endpoint_fingerprint);
        self.event["deployment_identity"] = json!(route.deployment_identity);
        self.event["upstream_model"] = json!(route.upstream_model);
        self.identity = crate::services::activity::ActivityIdentity::from_route(route);
        self.publish(false);
    }
    pub(crate) fn retry(
        &mut self,
        attempt: usize,
        delay: std::time::Duration,
        error: &emp_router::RouterError,
        fallback: bool,
    ) {
        if fallback {
            self.event["protocol_decision"] = json!("fallback_rejection");
            self.event["fallback_reason"] = json!("protocol_rejection");
        }
        retry_scheduled(
            self.state,
            &self.event["request_id"],
            attempt,
            delay,
            error,
            fallback,
        );
    }
    pub(crate) fn transport(mut self, transport: &str) -> Self {
        self.event["transport"] = json!(transport);
        self.publish(false);
        self
    }
    pub(crate) fn status(&mut self, status: u16, error: &str) {
        self.event["status"] = json!(status);
        self.event["error_class"] = json!(error);
        self.event["success"] = json!((200..300).contains(&status) && error == "none");
    }
    pub(crate) fn http_status(&mut self, status: u16) {
        self.status(
            status,
            if (200..300).contains(&status) {
                "none"
            } else {
                emp_transport::status_error_class(Some(status)).as_str()
            },
        );
    }
    pub(crate) fn router_error(&mut self, error: &emp_router::RouterError) {
        self.status(error.status(), error.error_class().as_str());
        self.event["failure_reason"] = json!(error.failure_reason());
    }
    pub(crate) fn native_error(&mut self, error: &emp_router::native_http::NativeHttpError) {
        self.status(
            error.status,
            error.body["error"]["type"]
                .as_str()
                .unwrap_or_else(|| emp_transport::status_error_class(Some(error.status)).as_str()),
        );
        self.event["failure_reason"] = error.body["error"]["failure_reason"].clone();
    }
    pub(crate) fn disconnected(&mut self) {
        self.event["status"] = Value::Null;
        self.event["error_class"] = json!("client_disconnect");
        self.event["success"] = json!(false);
    }
    pub(crate) fn finish(&mut self) {
        if !self.finalized {
            let duration_ms = self.started.elapsed().as_millis() as u64;
            self.event["duration_ms"] = json!(duration_ms);
            if let Some(first) = self.first_token {
                self.event["ttft_ms"] =
                    json!(first.duration_since(self.started).as_millis() as u64);
                let generation = self
                    .last_token
                    .unwrap_or(first)
                    .duration_since(first)
                    .as_millis() as u64;
                self.event["generation_ms"] = json!(generation);
            }
            self.event
                .as_object_mut()
                .expect("route observation is an object")
                .remove("tokens_per_second");
            if self.event["success"] == true
                && let Some(rate) =
                    request_tokens_per_second(&self.event["output_tokens"], &json!(duration_ms))
            {
                self.event["tokens_per_second"] = json!(rate);
            }
            self.state.backend.auto_review.record_result(
                self.event["client_model"].as_str().unwrap_or_default(),
                self.event["provider_id"].as_str().unwrap_or_default(),
                self.event["success"] == true,
                self.event["failure_reason"].as_str().unwrap_or_default(),
                self.event["error_class"].as_str().unwrap_or_default(),
            );
            crate::services::observation::record_completion(self.state, &self.event);
            self.publish(true);
            self.finalized = true;
        }
    }

    fn publish(&self, finished: bool) {
        self.state
            .backend
            .activity
            .observe_request(&self.event, self.identity.as_ref(), finished);
    }
}
impl Drop for RequestOutcome<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}
