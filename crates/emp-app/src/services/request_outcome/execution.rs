//! Request-owned execution facts. Activity is a bounded projection, never the ledger source.
use super::*;

impl RequestOutcome<'_> {
    pub(crate) fn claude_error(&mut self, error: &crate::services::claude_cli::ClaudeCliError) {
        use crate::services::claude_cli::{ClaudeCliError, failure_details};
        self.event["error_origin"] = json!(crate::services::error_origin::claude(error));
        match error {
            ClaudeCliError::Router(error) => self.router_error(error),
            ClaudeCliError::Disconnected => self.disconnected(),
            ClaudeCliError::Failure("claude_cli_interrupted") => self.interrupted(),
            ClaudeCliError::Failure(code) => {
                self.event["error_code"] = json!(code);
                self.status(failure_details(code).0, code);
            }
            ClaudeCliError::ShuttingDown => self.status(503, "server_shutting_down"),
        }
    }
    pub(crate) fn dispatch(&mut self) {
        self.event["attempts"] = json!(
            self.event["attempts"]
                .as_u64()
                .unwrap_or(0)
                .saturating_add(1)
        );
        let now = crate::util::system_now() as u64;
        let route = json!({"at":now,
            "upstream_model":emp_state::diagnostics::schema::id(&self.event["upstream_model"]),
            "protocol":emp_state::diagnostics::schema::id(&self.event["resolved_protocol"])});
        let routes = self.event["attempt_routes"].as_array_mut().unwrap();
        if routes.len() < 16 {
            routes.push(route);
        }
        crate::services::observation::record_attempt(
            self.state,
            &self.event,
            if self.buffered {
                "cli_execution"
            } else {
                "router_dispatch"
            },
        );
        self.publish(false);
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

    pub(crate) fn owner(&mut self, owner: &str) {
        if !owner.is_empty() {
            self.event["usage_owner"] = json!(owner);
        }
    }

    fn retry_fact(
        &mut self,
        reason: &str,
        status: u16,
        delay: Option<std::time::Duration>,
        retry_after: Option<u64>,
    ) {
        let retries = self.event["retries"].as_array_mut().unwrap();
        if retries.len() < 16 {
            retries.push(
                json!({"reason":reason,"status":status,"at":crate::util::system_now() as u64,
                    "delay_ms":delay.map(|value| value.as_millis() as u64),
                    "retry_after_seconds":retry_after}),
            );
        }
        self.publish(false);
    }

    pub(crate) fn retry(
        &mut self,
        attempt: usize,
        delay: std::time::Duration,
        error: &emp_router::RouterError,
        fallback: bool,
    ) {
        let reason = if fallback {
            self.event["protocol_decision"] = json!("fallback_rejection");
            self.event["fallback_reason"] = json!("protocol_rejection");
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
        crate::services::observation::retry_scheduled(
            self.state,
            &self.event["request_id"],
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
        self.state.backend.diagnostics.journal.event(
            "info",
            "native_retry_decision",
            &json!({
                "request_id":emp_state::diagnostics::schema::id(&self.event["request_id"]),
                "status":decision.status,"retry":decision.retry,"reason":reason,
            }),
        );
    }
}
