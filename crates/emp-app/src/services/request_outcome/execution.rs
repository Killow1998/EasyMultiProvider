use super::{DeliveryCancellation, RequestGuard};
use crate::services::observation::diagnostics::schema;
use emp_domain::ResolvedRoute;
use serde_json::json;

impl RequestGuard<'_> {
    pub(crate) fn claude_error(&mut self, error: &crate::services::claude_cli::ClaudeCliError) {
        use crate::services::claude_cli::{ClaudeCliError, failure_details};
        self.pending_mut().event["error_origin"] =
            json!(crate::services::error_origin::claude(error));
        match error {
            ClaudeCliError::Router(error) => self.router_error(error),
            ClaudeCliError::Disconnected => self.disconnected_before_delivery(),
            ClaudeCliError::Failure("claude_cli_interrupted") => {
                self.record_cancellation_before_delivery(DeliveryCancellation::Interrupted);
                self.interrupted();
            }
            ClaudeCliError::Failure(code) => {
                self.pending_mut().event["error_code"] = json!(code);
                self.status(failure_details(code).0, code);
            }
            ClaudeCliError::ShuttingDown => self.status(503, "server_shutting_down"),
        }
    }
    pub(crate) fn dispatch(&mut self) {
        let journal = &self.services.diagnostics.journal;
        let pending = self.pending_mut();
        pending.event["attempts"] = json!(
            pending.event["attempts"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1)
        );
        let now = crate::util::system_now() as u64;
        let route = json!({"at":now,
            "upstream_model":schema::id(&pending.event["upstream_model"]),
            "protocol":schema::id(&pending.event["resolved_protocol"])});
        let routes = pending.event["attempt_routes"].as_array_mut().unwrap();
        if routes.len() < 16 {
            routes.push(route);
        }
        crate::services::observation::record_attempt(
            journal,
            &pending.event,
            if pending.buffered {
                "cli_execution"
            } else {
                "router_dispatch"
            },
        );
        self.publish_active();
    }

    pub(crate) fn candidate(&mut self, route: &ResolvedRoute) {
        let pending = self.pending_mut();
        pending.event["resolved_protocol"] = json!(route.protocol);
        pending.event["dialect"] = json!(route.dialect);
        pending.event["endpoint_fingerprint"] = json!(route.endpoint_fingerprint);
        pending.event["deployment_identity"] = json!(route.deployment_identity);
        pending.event["upstream_model"] = json!(route.upstream_model);
        pending.identity = crate::services::activity::ActivityIdentity::from_route(route);
        self.publish_active();
    }

    pub(crate) fn owner(&mut self, owner: &str) {
        if !owner.is_empty() {
            self.pending_mut().event["usage_owner"] = json!(owner);
        }
    }

    fn retry_fact(
        &mut self,
        reason: &str,
        status: u16,
        delay: Option<std::time::Duration>,
        retry_after: Option<u64>,
    ) {
        let pending = self.pending_mut();
        let retries = pending.event["retries"].as_array_mut().unwrap();
        if retries.len() < 16 {
            retries.push(
                json!({"reason":reason,"status":status,"at":crate::util::system_now() as u64,
                    "delay_ms":delay.map(|value| value.as_millis() as u64),
                    "retry_after_seconds":retry_after}),
            );
        }
        self.publish_active();
    }

    pub(crate) fn retry(
        &mut self,
        attempt: usize,
        delay: std::time::Duration,
        error: &emp_router::RouterError,
        fallback: bool,
    ) {
        let reason = if fallback {
            let pending = self.pending_mut();
            pending.event["protocol_decision"] = json!("fallback_rejection");
            pending.event["fallback_reason"] = json!("protocol_rejection");
            "protocol_rejection"
        } else {
            error.error_class().as_str()
        };
        self.retry_fact(
            reason,
            error.status(),
            Some(delay),
            error.retry_after_seconds(),
        );
        let journal = &self.services.diagnostics.journal;
        crate::services::observation::retry_scheduled(
            journal,
            &self.pending_mut().event["request_id"],
            attempt,
            delay,
            error,
            fallback,
        );
    }

    pub(crate) fn native_retry(&mut self, decision: emp_router::native_http::NativeRetryDecision) {
        use emp_router::native_http::NativeRetryReason;
        let reason = match decision.reason {
            NativeRetryReason::Network => "network",
            NativeRetryReason::AccountRefresh => "account_refresh",
            NativeRetryReason::ReasoningFallback => "reasoning_fallback",
            NativeRetryReason::InlineInputReplay => "inline_input_replay",
        };
        if decision.retry {
            self.retry_fact(reason, decision.status, None, None);
        }
        let journal = &self.services.diagnostics.journal;
        journal.event(
            "info",
            "native_retry_decision",
            &json!({
                "request_id":schema::id(&self.pending_mut().event["request_id"]),
                "status":decision.status,"retry":decision.retry,"reason":reason,
            }),
        );
    }
}
