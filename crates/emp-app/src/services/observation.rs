//! One request observation feeds durable usage and safe diagnostics.
use crate::app::ServerState;
use crate::util::system_now;
use emp_core::ResolvedRoute;
use emp_state::usage::{account_owner, ledger::UsageLedger, reported_usage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
mod shape;

pub(crate) struct Observation {
    ledger: Arc<UsageLedger>,
    diagnostics: Arc<emp_state::diagnostics::Diagnostics>,
    started: Instant,
    first_token: Option<Instant>,
    last_token: Option<Instant>,
    event: Value,
    finalized: bool,
}
impl Observation {
    pub(crate) fn new(
        state: &ServerState,
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
            "transport":if body["stream"]==true{"sse"}else{"http"},"protocol_decision":"explicit","fallback_reason":"none","model_trace_source":"emp_dispatch","request_bytes":shape::request_bytes(body),"performance_schema":2,"speed_mode":if matches!(body["service_tier"].as_str(),Some("fast"|"priority"|"ultrafast")){"fast"}else{"standard"}});
        event
            .as_object_mut()
            .unwrap()
            .extend(shape::facts(body, incoming).as_object().unwrap().clone());
        Self {
            diagnostics: Arc::clone(&state.backend.diagnostics),
            started: Instant::now(),
            first_token: None,
            last_token: None,
            finalized: false,
            ledger: Arc::clone(&state.backend.usage.ledger),
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
        if let Some(model) = response["model"].as_str().filter(|s| !s.is_empty()) {
            self.event["response_model"] = json!(model);
            self.event["response_model_source"] = json!("upstream_response");
        }
        let kind = event["type"].as_str().unwrap_or("");
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
    pub(crate) fn transport(mut self, transport: &str) -> Self {
        self.event["transport"] = json!(transport);
        self
    }
    pub(crate) fn status(&mut self, status: u16, error: &str) {
        self.event["status"] = json!(status);
        self.event["error_class"] = json!(error);
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
    }
    pub(crate) fn finish(&mut self) {
        if !self.finalized {
            self.event["duration_ms"] = json!(self.started.elapsed().as_millis() as u64);
            if let Some(first) = self.first_token {
                self.event["ttft_ms"] =
                    json!(first.duration_since(self.started).as_millis() as u64);
                let generation = self
                    .last_token
                    .unwrap_or(first)
                    .duration_since(first)
                    .as_millis() as u64;
                self.event["generation_ms"] = json!(generation);
                if generation >= 500
                    && self.event["error_class"] == "none"
                    && let (Some(output), Some(reasoning)) = (
                        self.event["output_tokens"].as_u64(),
                        self.event["reasoning_tokens"].as_u64(),
                    )
                    && let Some(measured) = output.checked_sub(reasoning + 1).filter(|v| *v > 0)
                {
                    self.event["tokens_per_second"] = json!(
                        (measured as f64 * 100_000.0 / generation as f64).round_ties_even() / 100.0
                    );
                }
            }
            self.ledger.record(&self.event, system_now());
            self.diagnostics.record(&self.event);
            self.finalized = true;
        }
    }
}
impl Drop for Observation {
    fn drop(&mut self) {
        self.finish();
    }
}
