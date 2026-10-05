//! Passive request receipts. This owner knows diagnostics, not routing, retries,
//! account state or usage accounting. A receipt ends at the downstream boundary;
//! model outcomes keep their existing, separate upstream/accounting boundary.
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
    started: Instant,
    phase_started: Instant,
    phase: Phase,
    fields: Value,
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
            started: now,
            phase_started: now,
            phase: Phase::Authentication,
            fields,
        }
    }

    pub(crate) fn headers(
        &self,
        mut incoming: BTreeMap<String, String>,
    ) -> BTreeMap<String, String> {
        // A caller's case variant must not shadow EMP's correlation identifier.
        incoming.retain(|name, _| !name.eq_ignore_ascii_case("x-emp-request-id"));
        if let Some(id) = self.fields["request_id"].as_str() {
            incoming.insert("X-EMP-Request-ID".into(), id.into());
        }
        incoming
    }

    pub(crate) fn phase(&mut self, phase: Phase) {
        self.close_phase();
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

    fn close_phase(&mut self) {
        let elapsed = self.phase_started.elapsed().as_millis() as u64;
        let field = &mut self.fields["phase_ms"][self.phase.name()];
        *field = json!(field.as_u64().unwrap_or(0).saturating_add(elapsed));
        self.phase_started = Instant::now();
    }

    pub(crate) fn body(&mut self, reasoning: Option<&Value>, stream: Option<&Value>) {
        self.fields["requested_effort"] = json!(reasoning_effort(reasoning));
        if self.fields["transport"] == "http" && crate::util::python_truthy(stream) {
            self.fields["transport"] = json!("sse");
            self.fields["requested_transport"] = json!("sse");
        }
    }

    pub(crate) fn selected(&mut self, route: &ResolvedRoute) {
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
        self.event_type_written(event["type"].as_str().unwrap_or_default(), result)
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
        self.diagnostics
            .journal
            .event("info", "request_finished", &self.fields);
    }
}

/// Fixed vocabulary only. Unknown user text and malformed values are never logged.
pub(crate) fn reasoning_effort(reasoning: Option<&Value>) -> &'static str {
    match reasoning {
        None => "missing",
        Some(Value::Null) => "null",
        Some(Value::Object(reasoning)) => match reasoning.get("effort") {
            None => "missing",
            Some(Value::Null) => "null",
            Some(Value::String(value)) => match value.as_str() {
                "none" => "none",
                "minimal" => "minimal",
                "low" => "low",
                "medium" => "medium",
                "high" => "high",
                "xhigh" => "xhigh",
                "max" => "max",
                "ultra" => "ultra",
                _ => "unknown",
            },
            _ => "invalid_type",
        },
        _ => "invalid_type",
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
    fn effort_is_a_fact_with_no_default_or_content_retention() {
        for (body, expected) in [
            (json!({}), "missing"),
            (json!({"reasoning":null}), "null"),
            (json!({"reasoning":{"effort":null}}), "null"),
            (json!({"reasoning":{"effort":"none"}}), "none"),
            (json!({"reasoning":{"effort":"low"}}), "low"),
            (json!({"reasoning":{"effort":"medium"}}), "medium"),
            (json!({"reasoning":{"effort":"private-effort"}}), "unknown"),
            (
                json!({"reasoning":{"effort":{"secret":"private-value"}}}),
                "invalid_type",
            ),
        ] {
            assert_eq!(reasoning_effort(body.get("reasoning")), expected);
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
