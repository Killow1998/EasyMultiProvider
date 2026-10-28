//! Passive request receipts. This owner knows diagnostics, not routing, retries,
//! account state or usage accounting. A receipt ends at the downstream boundary;
//! model outcomes keep their existing, separate upstream/accounting boundary.
use crate::services::request_facts::reasoning_effort;
use emp_core::ResolvedRoute;
use emp_history::HistoryError;
use emp_state::diagnostics::Diagnostics;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;

#[derive(Clone, Copy)]
pub(crate) enum Phase {
    Authentication,
    ReadBody,
    ResolveRoute,
    ValidateInput,
    PrepareHistory,
    PrepareDestination,
    Execute,
}

impl Phase {
    fn name(self) -> &'static str {
        match self {
            Self::Authentication => "caller_access",
            Self::ReadBody => "read_body",
            Self::ResolveRoute => "resolve_route",
            Self::ValidateInput => "validate_input",
            Self::PrepareHistory => "prepare_history",
            Self::PrepareDestination => "prepare_destination",
            Self::Execute => "execute_and_relay",
        }
    }
}

pub(crate) struct RequestObservation {
    diagnostics: Arc<Diagnostics>,
    errors: crate::services::error_presentation::ErrorPresentation,
    ledger: Option<Arc<emp_state::usage::ledger::UsageLedger>>,
    reports_changed: Option<Arc<crate::services::management_events::ManagementEvents>>,
    started: Instant,
    phase_started: Instant,
    execution_started: Option<Instant>,
    phase: Phase,
    fields: Value,
    continuation: Option<super::continuation::NativeContinuation>,
}

impl RequestObservation {
    pub(crate) fn new(
        diagnostics: Arc<Diagnostics>,
        request_id: Option<String>,
        connection_id: Option<&str>,
        transport: &'static str,
        operation: &'static str,
    ) -> Self {
        let now = Instant::now();
        let fields = json!({
            "request_id":request_id, "connection_id":connection_id,
            "transport":transport, "requested_transport":transport, "operation":operation,
            "phase_ms":{}, "write_failed":false, "writes_completed":0,
            "terminal_written":false, "downstream_terminal":"unknown",
            "response_status":null,
        });
        diagnostics
            .journal
            .event("info", "request_started", &fields);
        Self {
            diagnostics,
            errors: Default::default(),
            ledger: None,
            reports_changed: None,
            started: now,
            phase_started: now,
            execution_started: None,
            phase: Phase::Authentication,
            fields,
            continuation: None,
        }
    }

    pub(crate) fn with_errors(
        mut self,
        errors: crate::services::error_presentation::ErrorPresentation,
    ) -> Self {
        self.errors = errors;
        self
    }
    pub(crate) fn errors(&self) -> &crate::services::error_presentation::ErrorPresentation {
        &self.errors
    }
    pub(crate) fn with_ledger(
        mut self,
        ledger: Arc<emp_state::usage::ledger::UsageLedger>,
        events: Arc<crate::services::management_events::ManagementEvents>,
    ) -> Self {
        self.ledger = Some(ledger);
        self.reports_changed = Some(events);
        self
    }

    pub(crate) fn headers(
        &self,
        mut incoming: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        // A caller's case variant must not shadow EMP's correlation identifier.
        incoming.retain(|name, _| {
            !name.eq_ignore_ascii_case("x-emp-request-id")
                && !name.eq_ignore_ascii_case("x-emp-probe")
        });
        if let Some(id) = self.fields["request_id"].as_str() {
            incoming.insert("X-EMP-Request-ID".into(), id.into());
        }
        if let Some(kind) = self.fields["operation"].as_str()
            && emp_state::diagnostics::MODEL_TEST_OPERATIONS.contains(&kind)
        {
            incoming.insert("X-EMP-Probe".into(), kind.into());
        }
        incoming
    }

    pub(crate) fn probe(&mut self, kind: &str) {
        self.fields["operation"] = json!(kind);
    }

    pub(crate) fn phase(&mut self, phase: Phase) {
        self.close_phase();
        if matches!(phase, Phase::Execute) {
            self.execution_started.get_or_insert(self.phase_started);
        }
        self.phase = phase;
        self.diagnostics.journal.event(
            "info",
            "request_phase_started",
            &json!({
                "request_id":self.fields["request_id"], "phase":phase.name(),
                "elapsed_ms":self.started.elapsed().as_millis() as u64,
            }),
        );
    }

    pub(crate) fn execution_started(&self) -> Instant {
        self.execution_started.unwrap_or(self.started)
    }

    fn close_phase(&mut self) {
        let elapsed = self.phase_started.elapsed().as_millis() as u64;
        let field = &mut self.fields["phase_ms"][self.phase.name()];
        *field = json!(field.as_u64().unwrap_or(0).saturating_add(elapsed));
        self.phase_started = Instant::now();
    }

    pub(crate) fn body(
        &mut self,
        reasoning: Option<&Value>,
        input: Option<&Value>,
        metadata: Option<&Value>,
        stream: Option<&Value>,
    ) {
        self.fields["requested_effort"] = json!(reasoning_effort(reasoning, input, metadata));
        if self.fields["transport"] == "http" && crate::util::python_truthy(stream) {
            self.fields["transport"] = json!("sse");
            self.fields["requested_transport"] = json!("sse");
        }
    }

    pub(crate) fn native_prefill_sent(&mut self, bytes: usize) {
        let facts = self.native_continuation();
        facts.prefill_requests += 1;
        facts.prefill_application_bytes =
            facts.prefill_application_bytes.saturating_add(bytes as u64);
    }

    pub(crate) fn native_continuation(&mut self) -> &mut super::continuation::NativeContinuation {
        self.continuation.get_or_insert_with(Default::default)
    }

    pub(crate) fn selected(&mut self, route: &ResolvedRoute) {
        self.errors.select(route);
        let mut fields = route_fields(route, &self.diagnostics);
        self.fields.as_object_mut().unwrap().extend(fields.clone());
        fields.insert("request_id".into(), self.fields["request_id"].clone());
        fields.insert(
            "requested_effort".into(),
            self.fields["requested_effort"].clone(),
        );
        self.diagnostics
            .journal
            .event("info", "request_route_selected", &Value::Object(fields));
    }

    /// Record the local failure before delivery, independently of whether the
    /// client socket accepts the error. Never infer history failures from
    /// arbitrary upstream JSON or normal previous-response fallback events.
    pub(crate) fn history_failed(&mut self, error: &HistoryError) {
        self.fields["error_origin"] = json!("emp");
        let diagnostic = error.diagnostic();
        let failure = json!({
            "category":diagnostic.category, "reason":diagnostic.reason,
            "phase":self.phase.name(),
        });
        self.fields["history_failure"] = failure;
        self.diagnostics.journal.event(
            "warning",
            "history_reconstruction_failed",
            &json!({
                "request_id":self.fields["request_id"],
                "connection_id":self.fields["connection_id"],
                "category":diagnostic.category, "reason":diagnostic.reason,
                "phase":self.phase.name(),
                "elapsed_ms":self.started.elapsed().as_millis() as u64,
            }),
        );
    }

    /// Return the original result unchanged; observation cannot change control flow.
    /// A successful local write is not an acknowledgement from the client.
    pub(crate) fn written<T, E>(&mut self, result: Result<T, E>, terminal: bool) -> Result<T, E> {
        if result.is_ok() {
            let count = self.fields["writes_completed"].as_u64().unwrap_or(0);
            self.fields["writes_completed"] = json!(count.saturating_add(1));
            if terminal {
                self.fields["terminal_written"] = json!(true);
            }
        } else {
            self.fields["write_failed"] = json!(true);
        }
        result
    }

    pub(crate) fn event_written<T, E>(
        &mut self,
        event: &Value,
        result: Result<T, E>,
    ) -> Result<T, E> {
        self.record_event_error(event, result.is_ok(), None);
        self.event_type_written(event["type"].as_str().unwrap_or_default(), result)
    }

    pub(crate) fn upstream_event_written<T, E>(
        &mut self,
        event: &Value,
        result: Result<T, E>,
    ) -> Result<T, E> {
        self.record_event_error(event, result.is_ok(), Some("upstream"));
        self.event_type_written(event["type"].as_str().unwrap_or_default(), result)
    }

    fn record_event_error(&mut self, event: &Value, delivered: bool, boundary: Option<&str>) {
        let error = event.get("error").or_else(|| {
            event
                .get("response")
                .and_then(|response| response.get("error"))
        });
        if let Some(error) = error.filter(|error| error.is_object()) {
            let origin = boundary.unwrap_or_else(|| {
                error["origin"]
                    .as_str()
                    .filter(|origin| {
                        matches!(
                            *origin,
                            "emp" | "upstream" | "transport" | "claude_cli" | "client"
                        )
                    })
                    .unwrap_or("unknown")
            });
            self.fields["error_origin"] = json!(origin);
            self.fields["error_code"] = json!(emp_state::diagnostics::schema::id(&error["code"]));
            self.diagnostics.journal.event("warning", "response_error", &json!({
                "request_id":self.fields["request_id"], "connection_id":self.fields["connection_id"],
                "phase":self.phase.name(), "error_origin":origin, "error_code":self.fields["error_code"],
                "delivered":delivered,
            }));
        }
    }

    pub(crate) fn buffered_error(&mut self, body: &[u8], delivered: bool) {
        // Only own bounded error JSON is inspected, never a successful reply.
        if body.len() <= 64 * 1024
            && let Ok(event) = serde_json::from_slice(body)
        {
            self.record_event_error(&event, delivered, None);
        }
    }

    pub(crate) fn event_type_written<T, E>(
        &mut self,
        event_type: &str,
        result: Result<T, E>,
    ) -> Result<T, E> {
        let terminal = match event_type {
            "response.completed" => Some("completed"),
            "response.incomplete" => Some("incomplete"),
            "response.failed" => Some("failed"),
            "error" => Some("error"),
            _ => None,
        };
        if result.is_ok()
            && let Some(kind) = terminal
        {
            self.fields["downstream_terminal"] = json!(kind);
        }
        self.written(result, terminal.is_some())
    }

    pub(crate) fn http_response(&mut self, status: &Value, result: std::io::Result<()>) {
        self.fields["transport"] = json!("http");
        self.fields["response_status"] = status.clone();
        let _ = self.written(result, true);
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if let Some(continuation) = &self.continuation {
            self.fields["native_continuation"] =
                serde_json::to_value(continuation).expect("content-free continuation facts");
            self.fields["native_continuation"]["byte_scope"] = json!("websocket_text_payload");
        }
        self.close_phase();
        self.fields["last_phase"] = json!(self.phase.name());
        self.fields["duration_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.fields["handler_panicked"] = json!(std::thread::panicking());
        self.fields["delivery"] = json!(if self.fields["write_failed"] == true {
            "write_failed"
        } else if self.fields["terminal_written"] == true {
            "terminal_written"
        } else if self.fields["writes_completed"].as_u64().unwrap_or(0) > 0 {
            "partial"
        } else {
            "not_observed"
        });
        if let Some(ledger) = &self.ledger
            && let Some(id) = self.fields["request_id"].as_str()
        {
            match ledger.record_call_delivery(
                id,
                self.fields["operation"].as_str().unwrap_or("responses"),
                &self.fields,
            ) {
                Ok(()) => {
                    if let Some(events) = &self.reports_changed {
                        events.publish(crate::services::management_events::Change::Usage);
                    }
                }
                Err(_) => self.diagnostics.journal.event(
                    "error",
                    "call_delivery_save_failed",
                    &json!({"request_id":id}),
                ),
            }
        }
        self.diagnostics
            .journal
            .event("info", "request_finished", &self.fields);
    }
}

pub(crate) fn request_id(incoming: &BTreeMap<String, String>) -> Value {
    json!(
        incoming
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("x-emp-request-id"))
            .map(|(_, value)| value)
            .filter(|value| value.len() == 16 && value.bytes().all(|b| b.is_ascii_hexdigit()))
    )
}

pub(crate) fn route_fields(
    route: &ResolvedRoute,
    diagnostics: &Diagnostics,
) -> serde_json::Map<String, Value> {
    let event = json!({"provider_id":route.provider_id,"upstream_model":route.upstream_model,
        "model_id":route.requested_model,"resolved_protocol":route.protocol,"dialect":route.dialect,
        "route_source":route.source,"endpoint_fingerprint":route.endpoint_fingerprint});
    let safe = emp_state::diagnostics::schema::route_record(&event, &diagnostics.journal);
    [
        "provider_id",
        "upstream_model",
        "model_id",
        "protocol",
        "dialect",
        "route_source",
        "endpoint_fingerprint",
    ]
    .into_iter()
    .map(|key| (key.into(), safe[key].clone()))
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn records(root: &std::path::Path) -> Vec<Value> {
        std::fs::read_dir(root.join("logs"))
            .unwrap()
            .flat_map(|entry| {
                std::fs::read_to_string(entry.unwrap().path())
                    .unwrap()
                    .lines()
                    .map(|line| serde_json::from_str(line).unwrap())
                    .collect::<Vec<Value>>()
            })
            .collect()
    }

    #[test]
    fn error_receipts_record_the_boundary_without_trusting_upstream_claims_or_logging_content() {
        let root = tempfile::tempdir().unwrap();
        let diagnostics = Arc::new(Diagnostics::new(&root.path().canonicalize().unwrap()));
        let event = json!({"type":"error","error":{"code":"quota_exhausted","origin":"emp","message":"private-provider-detail"}});
        let mut receipt = RequestObservation::new(
            Arc::clone(&diagnostics),
            Some("0123456789abcdef".into()),
            None,
            "websocket",
            "responses",
        );
        receipt.phase(Phase::Execute);
        receipt
            .upstream_event_written(&event, Ok::<_, ()>(()))
            .unwrap();
        drop(receipt);
        let mut receipt = RequestObservation::new(
            diagnostics,
            Some("fedcba9876543210".into()),
            None,
            "websocket",
            "responses",
        );
        receipt.phase(Phase::ValidateInput);
        let event = json!({"type":"error","error":{"code":"invalid_request","origin":"emp","message":"private-input-detail"}});
        receipt
            .event_written(
                &event,
                Err::<(), _>(std::io::Error::from(std::io::ErrorKind::BrokenPipe)),
            )
            .unwrap_err();
        drop(receipt);
        let journal = records(root.path());
        let failures: Vec<_> = journal
            .iter()
            .filter(|event| event["event"] == "response_error")
            .collect();
        assert_eq!(failures.len(), 2);
        assert_eq!(failures[0]["fields"]["error_origin"], "upstream");
        assert_eq!(failures[0]["fields"]["error_code"], "quota_exhausted");
        assert_eq!(failures[0]["fields"]["delivered"], true);
        assert_eq!(failures[1]["fields"]["error_origin"], "emp");
        assert_eq!(failures[1]["fields"]["phase"], "validate_input");
        assert_eq!(failures[1]["fields"]["delivered"], false);
        assert!(
            !serde_json::to_string(&journal)
                .unwrap()
                .contains("private-")
        );
    }

    #[test]
    fn a_failed_terminal_write_is_not_completion_and_does_not_change_the_error() {
        let root = tempfile::tempdir().unwrap();
        let diagnostics = Arc::new(Diagnostics::new(&root.path().canonicalize().unwrap()));
        let mut receipt = RequestObservation::new(
            diagnostics,
            Some("0123456789abcdef".into()),
            None,
            "sse",
            "responses",
        );
        receipt.phase(Phase::Execute);
        let terminal = json!({"type":"response.completed","response":{"output":"private-answer"}});
        let error = std::io::Error::new(std::io::ErrorKind::BrokenPipe, "private-error");
        let result: std::io::Result<()> = receipt.event_written(&terminal, Err(error));
        assert_eq!(result.unwrap_err().to_string(), "private-error");
        drop(receipt);
        let records = records(root.path());
        let done = records
            .iter()
            .find(|r| r["event"] == "request_finished")
            .unwrap();
        assert_eq!(done["fields"]["delivery"], "write_failed");
        assert_eq!(done["fields"]["terminal_written"], false);
        assert_eq!(done["fields"]["downstream_terminal"], "unknown");
        assert!(done["fields"]["phase_ms"]["caller_access"].is_number());
        assert!(
            !serde_json::to_string(&records)
                .unwrap()
                .contains("private-")
        );
    }

    #[test]
    fn partial_and_successful_delivery_remain_distinct_even_when_dropped() {
        for terminal in [false, true] {
            let root = tempfile::tempdir().unwrap();
            let diagnostics = Arc::new(Diagnostics::new(&root.path().canonicalize().unwrap()));
            let mut receipt =
                RequestObservation::new(diagnostics, None, None, "websocket", "responses");
            let event = json!({"type":if terminal { "response.incomplete" } else { "response.output_text.delta" },"delta":"private-output"});
            assert_eq!(receipt.event_written(&event, Ok::<_, ()>(7)), Ok(7));
            drop(receipt);
            let records = records(root.path());
            let done: Vec<_> = records
                .iter()
                .filter(|r| r["event"] == "request_finished")
                .collect();
            assert_eq!(done.len(), 1);
            assert_eq!(
                done[0]["fields"]["delivery"],
                if terminal {
                    "terminal_written"
                } else {
                    "partial"
                }
            );
            assert_eq!(
                done[0]["fields"]["downstream_terminal"],
                if terminal { "incomplete" } else { "unknown" }
            );
        }
    }

    #[test]
    fn history_reason_survives_a_failed_write_and_unknown_text_is_not_logged() {
        for (reason, category, expected) in [
            (
                "compaction_summary_missing",
                "checkpoint",
                "compaction_summary_missing",
            ),
            (
                "private_history_content",
                "unknown",
                "history_reason_unknown",
            ),
        ] {
            let root = tempfile::tempdir().unwrap();
            let diagnostics = Arc::new(Diagnostics::new(&root.path().canonicalize().unwrap()));
            let mut receipt = RequestObservation::new(
                diagnostics,
                Some("0123456789abcdef".into()),
                None,
                "sse",
                "responses",
            );
            receipt.phase(Phase::PrepareHistory);
            receipt.history_failed(&HistoryError::new(reason));
            let write: std::io::Result<()> = Err(std::io::ErrorKind::BrokenPipe.into());
            assert_eq!(
                receipt
                    .event_type_written("response.failed", write)
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::BrokenPipe
            );
            drop(receipt);
            let records = records(root.path());
            let failure = records
                .iter()
                .find(|r| r["event"] == "history_reconstruction_failed")
                .unwrap();
            let done = records
                .iter()
                .find(|r| r["event"] == "request_finished")
                .unwrap();
            assert_eq!(
                failure["fields"]["request_id"],
                done["fields"]["request_id"]
            );
            assert_eq!(failure["fields"]["reason"], expected);
            assert_eq!(
                done["fields"]["history_failure"],
                json!({"phase":"prepare_history", "category":category, "reason":expected})
            );
            assert_eq!(done["fields"]["delivery"], "write_failed");
            assert!(!serde_json::to_string(&records).unwrap().contains("private"));
        }
    }
}
