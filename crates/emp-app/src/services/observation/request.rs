//! Passive receipts end at the downstream boundary and cannot access routing, retries,
//! accounts or accounting; model outcomes retain their separate upstream boundary.
use crate::services::observation::diagnostics::{Diagnostics, schema};
use crate::services::request_facts::reasoning_effort;
use emp_context::HistoryError;
use emp_domain::ResolvedRoute;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};
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

struct EventError {
    origin: &'static str,
    code: String,
}

struct EventDelivery {
    error: Option<EventError>,
    terminal: Option<&'static str>,
}

impl EventDelivery {
    fn new(event: Option<&Value>, event_type: &str, upstream: bool) -> Self {
        Self {
            error: event.and_then(|event| EventError::from_event(event, upstream)),
            terminal: terminal_outcome(event_type),
        }
    }
}

#[derive(Default)]
struct DeliveryFacts {
    writes_completed: u64,
    write_failed: bool,
    terminal_written: bool,
    downstream_terminal: Option<&'static str>,
    error_origin: Option<&'static str>,
    error_code: Option<String>,
}

impl DeliveryFacts {
    fn written(&mut self, delivered: bool, terminal: bool) {
        if delivered {
            self.writes_completed = self.writes_completed.saturating_add(1);
            self.terminal_written |= terminal;
        } else {
            self.write_failed = true;
        }
    }
}

struct RequestDelivery {
    diagnostics: Arc<Diagnostics>,
    request_id: Value,
    connection_id: Value,
    facts: Mutex<DeliveryFacts>,
}

impl RequestDelivery {
    fn event_error(
        &self,
        facts: &mut DeliveryFacts,
        error: EventError,
        phase: Phase,
        delivered: bool,
    ) {
        self.diagnostics.journal.event(
            "warning",
            "response_error",
            &json!({
                "request_id":self.request_id, "connection_id":self.connection_id,
                "phase":phase.name(), "error_origin":error.origin, "error_code":error.code,
                "delivered":delivered,
            }),
        );
        facts.error_origin = Some(error.origin);
        facts.error_code = Some(error.code);
    }

    fn event(
        &self,
        facts: &mut DeliveryFacts,
        event: EventDelivery,
        phase: Phase,
        delivered: bool,
    ) {
        if let Some(error) = event.error {
            self.event_error(facts, error, phase, delivered);
        }
        if delivered && event.terminal.is_some() {
            facts.downstream_terminal = event.terminal;
        }
        facts.written(delivered, event.terminal.is_some());
    }
}

enum FrameEvents {
    Event(EventDelivery),
    AeroChunk(Vec<EventDelivery>),
}

pub(crate) struct FrameDiagnostics {
    delivery: Arc<RequestDelivery>,
    phase: Phase,
    events: Option<FrameEvents>,
}

impl FrameDiagnostics {
    pub(crate) fn flushed(mut self) {
        self.settle(true);
    }

    fn settle(&mut self, delivered: bool) {
        let Some(events) = self.events.take() else {
            return;
        };
        let mut facts = self.delivery.facts.lock().expect("request delivery");
        match events {
            FrameEvents::Event(event) => self
                .delivery
                .event(&mut facts, event, self.phase, delivered),
            FrameEvents::AeroChunk(events) => {
                facts.written(delivered, false);
                for event in events {
                    self.delivery
                        .event(&mut facts, event, self.phase, delivered);
                }
            }
        }
    }
}

impl Drop for FrameDiagnostics {
    fn drop(&mut self) {
        self.settle(false);
    }
}

pub(crate) struct RequestObservation {
    delivery: Arc<RequestDelivery>,
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
            delivery: Arc::new(RequestDelivery {
                diagnostics,
                request_id: fields["request_id"].clone(),
                connection_id: fields["connection_id"].clone(),
                facts: Mutex::default(),
            }),
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
    pub(crate) fn diagnostic_only(&mut self, route: &str) {
        self.ledger = None;
        self.reports_changed = None;
        self.fields["route"] = json!(route);
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
        incoming.retain(|name, _| {
            !name.eq_ignore_ascii_case("x-emp-request-id")
                && !name.eq_ignore_ascii_case("x-emp-probe")
        });
        if let Some(id) = self.fields["request_id"].as_str() {
            incoming.insert("X-EMP-Request-ID".into(), id.into());
        }
        if let Some(kind) = self.fields["operation"].as_str()
            && emp_domain::MODEL_TEST_OPERATIONS.contains(&kind)
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
        self.delivery.diagnostics.journal.event(
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
        if self.fields["transport"] == "http" && stream.is_some_and(emp_domain::json_truthy) {
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
        let mut fields = route_fields(route, &self.delivery.diagnostics);
        self.fields.as_object_mut().unwrap().extend(fields.clone());
        fields.insert("request_id".into(), self.fields["request_id"].clone());
        fields.insert(
            "requested_effort".into(),
            self.fields["requested_effort"].clone(),
        );
        self.delivery.diagnostics.journal.event(
            "info",
            "request_route_selected",
            &Value::Object(fields),
        );
    }

    /// Record local failure independently of socket delivery; upstream JSON and normal
    /// previous-response fallback never establish a history failure.
    pub(crate) fn history_failed(&mut self, error: &HistoryError) {
        self.delivery
            .facts
            .lock()
            .expect("request delivery")
            .error_origin = Some("emp");
        let diagnostic = error.diagnostic();
        let failure = json!({
            "category":diagnostic.category, "reason":diagnostic.reason,
            "phase":self.phase.name(),
        });
        self.fields["history_failure"] = failure;
        self.delivery.diagnostics.journal.event(
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

    /// Preserve control flow and results; a successful local write is no client acknowledgement.
    pub(crate) fn written<T, E>(&mut self, result: Result<T, E>, terminal: bool) -> Result<T, E> {
        self.delivery
            .facts
            .lock()
            .expect("request delivery")
            .written(result.is_ok(), terminal);
        result
    }

    pub(crate) fn event_written<T, E>(
        &mut self,
        event: &Value,
        result: Result<T, E>,
    ) -> Result<T, E> {
        self.record_event_delivery(
            Some(event),
            event["type"].as_str().unwrap_or_default(),
            false,
            result,
        )
    }

    pub(crate) fn upstream_event_written<T, E>(
        &mut self,
        event: &Value,
        result: Result<T, E>,
    ) -> Result<T, E> {
        self.record_event_delivery(
            Some(event),
            event["type"].as_str().unwrap_or_default(),
            true,
            result,
        )
    }

    pub(crate) fn frame_event(
        &self,
        event: Option<&Value>,
        event_type: &str,
        upstream: bool,
    ) -> FrameDiagnostics {
        FrameDiagnostics {
            delivery: self.delivery.clone(),
            phase: self.phase,
            events: Some(FrameEvents::Event(EventDelivery::new(
                event, event_type, upstream,
            ))),
        }
    }

    pub(crate) fn aero_chunk(
        &self,
        events: Vec<serde_json::Map<String, Value>>,
    ) -> FrameDiagnostics {
        let events = events
            .into_iter()
            .map(|event| {
                let event = Value::Object(event);
                EventDelivery::new(
                    Some(&event),
                    event["type"].as_str().unwrap_or_default(),
                    false,
                )
            })
            .collect();
        FrameDiagnostics {
            delivery: self.delivery.clone(),
            phase: self.phase,
            events: Some(FrameEvents::AeroChunk(events)),
        }
    }

    pub(crate) fn buffered_error(&mut self, body: &[u8], delivered: bool) {
        if body.len() <= 64 * 1024
            && let Ok(event) = serde_json::from_slice(body)
            && let Some(error) = EventError::from_event(&event, false)
        {
            let mut facts = self.delivery.facts.lock().expect("request delivery");
            self.delivery
                .event_error(&mut facts, error, self.phase, delivered);
        }
    }

    fn record_event_delivery<T, E>(
        &mut self,
        event: Option<&Value>,
        event_type: &str,
        upstream: bool,
        result: Result<T, E>,
    ) -> Result<T, E> {
        let mut facts = self.delivery.facts.lock().expect("request delivery");
        self.delivery.event(
            &mut facts,
            EventDelivery::new(event, event_type, upstream),
            self.phase,
            result.is_ok(),
        );
        result
    }

    pub(crate) fn http_response(&mut self, status: &Value, result: std::io::Result<()>) {
        self.fields["transport"] = json!("http");
        self.fields["response_status"] = status.clone();
        let _ = self.written(result, true);
    }
}

impl EventError {
    fn from_event(event: &Value, upstream: bool) -> Option<Self> {
        let error = event
            .get("error")
            .or_else(|| event.get("response")?.get("error"))?;
        if !error.is_object() {
            return None;
        }
        let origin = if upstream {
            "upstream"
        } else {
            ["emp", "upstream", "transport", "claude_cli", "client"]
                .into_iter()
                .find(|origin| error["origin"].as_str() == Some(*origin))
                .unwrap_or("unknown")
        };
        Some(Self {
            origin,
            code: schema::id(&error["code"]),
        })
    }
}

fn terminal_outcome(event_type: &str) -> Option<&'static str> {
    match event_type {
        "response.completed" => Some("completed"),
        "response.incomplete" => Some("incomplete"),
        "response.failed" => Some("failed"),
        "error" => Some("error"),
        _ => None,
    }
}

impl RequestObservation {
    fn finish(&mut self) {
        if let Some(continuation) = &self.continuation {
            self.fields["native_continuation"] =
                serde_json::to_value(continuation).expect("content-free continuation facts");
            self.fields["native_continuation"]["byte_scope"] = json!("websocket_text_payload");
        }
        self.close_phase();
        self.fields["last_phase"] = json!(self.phase.name());
        self.fields["duration_ms"] = json!(self.started.elapsed().as_millis() as u64);
        self.fields["handler_panicked"] = json!(std::thread::panicking());
        {
            let facts = self.delivery.facts.lock().expect("request delivery");
            self.fields["writes_completed"] = json!(facts.writes_completed);
            self.fields["write_failed"] = json!(facts.write_failed);
            self.fields["terminal_written"] = json!(facts.terminal_written);
            self.fields["downstream_terminal"] =
                json!(facts.downstream_terminal.unwrap_or("unknown"));
            if let Some(origin) = facts.error_origin {
                self.fields["error_origin"] = json!(origin);
            }
            if let Some(code) = &facts.error_code {
                self.fields["error_code"] = json!(code);
            }
            self.fields["delivery"] = json!(if facts.write_failed {
                "write_failed"
            } else if facts.terminal_written {
                "terminal_written"
            } else if facts.writes_completed > 0 {
                "partial"
            } else {
                "not_observed"
            });
        }
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
                Err(_) => self.delivery.diagnostics.journal.event(
                    "error",
                    "call_delivery_save_failed",
                    &json!({"request_id":id}),
                ),
            }
        }
        self.delivery
            .diagnostics
            .journal
            .event("info", "request_finished", &self.fields);
    }
}

impl Drop for RequestObservation {
    fn drop(&mut self) {
        if self.fields["transport"] == "websocket" {
            tokio::task::block_in_place(|| self.finish());
        } else {
            self.finish();
        }
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
    let safe = schema::route_record(&event, &diagnostics.journal);
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
mod tests;
