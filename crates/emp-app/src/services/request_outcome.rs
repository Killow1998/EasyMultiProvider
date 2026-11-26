use super::activity::{ActivityGuard, ActivityIdentity};
use super::request_facts;
use crate::app::ServerState;
use crate::services::observation::diagnostics::schema;
use emp_domain::ResolvedRoute;
use emp_state::usage::{account_owner, reported_usage};
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Instant;
mod execution;
mod performance;
mod recording;
use recording::{CompletionServices, finish_request};

#[cfg(all(test, feature = "hotpath"))]
pub(crate) fn profile_request_bytes(body: &Value) -> usize {
    request_facts::request_bytes(body)
}

pub(crate) struct RequestGuard<'a> {
    services: CompletionServices<'a>,
    pending: Option<PendingRequest>,
    route_activity: Option<RequestActivity<'a>>,
}

enum RequestActivity<'a> {
    Route(ActivityGuard<'a>),
    History(Arc<ActivityGuard<'a>>),
}

impl RequestActivity<'_> {
    fn guard(&self) -> &ActivityGuard<'_> {
        match self {
            Self::Route(guard) => guard,
            Self::History(guard) => guard,
        }
    }
}

struct PendingRequest {
    identity: Option<ActivityIdentity>,
    started: Instant,
    first_token: Option<Instant>,
    event: Value,
    aggregate_owner: Option<String>,
    models: emp_router::model_observation::ModelObservation,
    source_clock: bool,
    buffered: bool,
    prewarm_cancellation: Option<DeliveryCancellation>,
}

struct RequestOutcome {
    event: Value,
    identity: Option<ActivityIdentity>,
    started: Instant,
    aggregate_owner: Option<String>,
    prewarm_cancellation: Option<DeliveryCancellation>,
}

#[derive(Clone, Copy)]
enum DeliveryCancellation {
    Disconnected,
    Interrupted,
}

impl DeliveryCancellation {
    fn event(self, request_id: &Value) -> Value {
        let (class, reason) = match self {
            Self::Disconnected => ("client_disconnect", None),
            Self::Interrupted => ("client_cancelled", Some("interrupted")),
        };
        json!({"request_id":request_id,"error_class":class,"error_origin":"client",
            "failure_reason":reason,"success":false})
    }
}

impl<'a> RequestGuard<'a> {
    pub(crate) fn native_credentials(
        &mut self,
        route: &ResolvedRoute,
        headers: &BTreeMap<String, String>,
        native_account: Option<&str>,
    ) {
        self.owner(&account_owner(headers));
        if route.provider_id == "codex-native" {
            let scope = native_account
                .and_then(|identity| self.services.team.native_usage_scope(identity))
                .unwrap_or(Value::Null);
            self.pending_mut().event["team_usage"] = scope;
        }
    }

    pub(crate) fn new(
        state: &'a ServerState,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        owner: Option<&str>,
        operation: &str,
    ) -> Self {
        let mut request = Self::deferred(state, route, body, incoming, owner, operation);
        request.track_route();
        request
    }

    pub(crate) fn deferred(
        state: &'a ServerState,
        route: &ResolvedRoute,
        body: &Value,
        incoming: &BTreeMap<String, String>,
        owner: Option<&str>,
        operation: &str,
    ) -> Self {
        let backend = &state.backend;
        let services = CompletionServices {
            activity: &backend.activity,
            aggregate: &backend.aggregate_availability,
            auto_review: &backend.auto_review,
            diagnostics: &backend.diagnostics,
            events: &backend.management_events,
            memory: &backend.memory_status,
            team: &backend.team,
            usage: &backend.usage,
        };
        let memory_kind = request_facts::memory_kind(body, incoming);
        let operation = if operation == "responses" && body["generate"] == false {
            "prewarm"
        } else if operation == "responses" {
            incoming
                .get("X-EMP-Probe")
                .map(String::as_str)
                .or(memory_kind)
                .unwrap_or(operation)
        } else {
            operation
        };
        if operation == "responses" {
            backend
                .transport
                .image_routes
                .remember(route, body, incoming);
        }
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
            .and_then(|body| emp_context::request_history_anchor(body, incoming).ok())
            .and_then(|anchor| anchor.turn_id)
            .unwrap_or_default();
        let mut event = json!({"route":operation,"usage_category":category,"usage_owner":owner,"upstream_model":route.upstream_model,"route_model":route.requested_model,"usage_turn":turn,"service_tier":body["service_tier"].as_str().filter(|s|!s.is_empty()).unwrap_or("default"),
            "provider_id":route.provider_id,"model_id":route.requested_model,"client_model":body["model"],"resolved_protocol":route.protocol,"dialect":route.dialect,"route_source":route.source,"endpoint_fingerprint":route.endpoint_fingerprint,"deployment_identity":route.deployment_identity,
            "transport":if body["stream"]==true{"sse"}else{"http"},"protocol_decision":"explicit","fallback_reason":"none","model_trace_source":"emp_dispatch","request_bytes":request_facts::request_bytes(body),"performance_schema":5,
            "requested_service_tier":body["service_tier"].as_str().filter(|value|!value.is_empty() && value.chars().count()<=32),
            "reported_service_tier":null,"reported_effort":null,"speed_mode":"unknown"});
        event
            .as_object_mut()
            .unwrap()
            .extend(request_facts::facts(body, incoming, memory_kind));
        if let Some(source) = route.model.value().get("_emp_aggregate_source") {
            event["aggregate_model"] = json!(route.requested_model);
            event["aggregate_source"] = source.clone();
        }
        if operation == "decisions" {
            event["parent_thread_id"] = event["thread_id"].clone();
            event["session_id"] = Value::Null;
            event["thread_id"] = Value::Null;
            event["session_kind"] = json!("decision");
        }
        if emp_domain::MODEL_TEST_OPERATIONS.contains(&operation) {
            event["session_id"] = Value::Null;
            event["thread_id"] = Value::Null;
            event["parent_thread_id"] = Value::Null;
            event["session_kind"] = json!("probe");
        }
        if event["request_id"].is_null() {
            event["request_id"] = json!(crate::util::random_hex(8).ok());
        }
        event["attempts"] = json!(0);
        event["retries"] = json!([]);
        event["attempt_routes"] = json!([]);
        event["requested_effort"] = json!(request_facts::reasoning_effort(
            body.get("reasoning"),
            body.get("input"),
            if memory_kind.is_some() {
                None
            } else {
                body.get("client_metadata")
            }
        ));
        if let Some(team) = services.team.usage_scope(state, route) {
            event["team_usage"] = team;
        }
        event["selected_name"] = route
            .model
            .value()
            .get("display_name")
            .cloned()
            .unwrap_or(Value::Null);
        event["provider_name"] = route
            .provider
            .value()
            .get("name")
            .cloned()
            .unwrap_or(Value::Null);
        let record = schema::route_record(&event, &services.diagnostics.journal);
        if let Some(prior) = services.diagnostics.continuation_recovery_from(&record) {
            event["continuation_recovery_from"] = json!(prior);
        }
        let started_event = if operation == "prewarm" {
            "model_prewarm_started"
        } else {
            "model_operation_started"
        };
        services.diagnostics.journal.event("info", started_event, &json!({
            "request_id":record["request_id"], "protocol":record["protocol"], "transport":record["transport"],
            "provider_id":record["provider_id"], "model_id":record["model_id"], "route":operation,
        }));
        let pending = PendingRequest {
            aggregate_owner: route
                .model
                .value()
                .get("_emp_aggregate_owner")
                .and_then(Value::as_str)
                .map(str::to_owned),
            identity: ActivityIdentity::from_route(route),
            started: Instant::now(),
            first_token: None,
            event,
            models: Default::default(),
            source_clock: false,
            buffered: crate::services::claude_cli::selected(route),
            prewarm_cancellation: None,
        };
        Self {
            services,
            pending: Some(pending),
            route_activity: None,
        }
    }

    pub(crate) fn track_route(&mut self) {
        let identity = self.pending_mut().identity.clone();
        self.route_activity = Some(RequestActivity::Route(
            self.services.activity.begin(identity),
        ));
        self.publish_active();
    }

    pub(super) fn track_history_route(&mut self, batch: &mut Option<Arc<ActivityGuard<'a>>>) {
        let guard = batch.get_or_insert_with(|| {
            let identity = self.pending_mut().identity.clone();
            Arc::new(self.services.activity.begin(identity))
        });
        self.route_activity = Some(RequestActivity::History(Arc::clone(guard)));
        self.publish_active();
    }

    fn publish_active(&self) {
        if let Some(pending) = &self.pending {
            pending.publish_active(self.route_activity.as_ref());
        }
    }

    fn pending_mut(&mut self) -> &mut PendingRequest {
        self.pending.as_mut().expect("request is still executing")
    }

    pub(crate) fn observe(&mut self, event: &Value) {
        self.observe_event(event, None);
    }
    pub(crate) fn image_credit(&mut self, cost_nanos: Option<i64>) {
        self.pending_mut().event["image_credit_cost_nanos"] = json!(cost_nanos);
    }
    pub(crate) fn observe_upstream(&mut self, event: &Value) {
        self.observe_event(event, Some("upstream"));
    }
    fn observe_event(&mut self, event: &Value, origin: Option<&str>) {
        if let Some(pending) = &mut self.pending
            && pending.observe_event(self.route_activity.as_ref(), event, origin)
        {
            self.finish();
        }
    }

    pub(crate) fn started_at(mut self, started: Instant) -> Self {
        self.pending_mut().started = started;
        self
    }

    pub(crate) fn raw_event(&mut self, event: &Value) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        pending.source_clock = true;
        pending.models.observe(event, !pending.buffered);
        pending.first_token = pending.models.first_output;
        pending.update_model_facts();
        self.publish_active();
    }

    pub(crate) fn upstream_observation(
        &mut self,
        observation: &emp_router::model_observation::ModelObservation,
    ) {
        let Some(pending) = &mut self.pending else {
            return;
        };
        pending.source_clock = true;
        pending.first_token = observation.first_output;
        pending.models.last_content = observation.last_content;
        if pending.models != *observation {
            pending.models = observation.clone();
            pending.update_model_facts();
            self.publish_active();
        }
    }

    pub(crate) fn transport(mut self, transport: &str) -> Self {
        self.set_transport(transport);
        self
    }

    pub(crate) fn websocket(&mut self, started: Instant) {
        self.pending_mut().started = started;
        self.set_transport("websocket");
    }

    fn set_transport(&mut self, transport: &str) {
        let pending = self.pending_mut();
        pending.event["transport"] = json!(transport);
        self.publish_active();
    }

    pub(crate) fn status(&mut self, status: u16, error: &str) {
        if let Some(pending) = &mut self.pending {
            pending.status(status, error);
        }
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
        let journal = &self.services.diagnostics.journal;
        let pending = self.pending_mut();
        pending.event["error_origin"] = json!(crate::services::error_origin::router(error));
        pending.status(error.status(), error.error_class().as_str());
        pending.event["failure_reason"] = json!(error.failure_reason());
        pending.event["retry_after_seconds"] = json!(error.retry_after_seconds());
        if error.kind() == emp_router::RouterErrorKind::Upstream {
            pending.upstream_error_detail(journal, &error.to_string());
        }
    }

    pub(crate) fn native_error(&mut self, error: &emp_router::native_http::NativeHttpError) {
        let journal = &self.services.diagnostics.journal;
        let pending = self.pending_mut();
        pending.event["error_origin"] = error.body["error"]["origin"].clone();
        pending.status(
            error.status,
            error.body["error"]["type"]
                .as_str()
                .unwrap_or_else(|| emp_transport::status_error_class(Some(error.status)).as_str()),
        );
        pending.event["failure_reason"] = error.body["error"]["failure_reason"].clone();
        pending.event["retry_after_seconds"] =
            json!(error.body["error"]["retry_after_seconds"].as_u64());
        pending.event["error_code"] = error.body["error"]
            .get("upstream_code")
            .unwrap_or(&error.body["error"]["code"])
            .clone();
        if pending.event["error_origin"] == "upstream" {
            pending.upstream_error_detail(
                journal,
                error.body["error"]["message"].as_str().unwrap_or_default(),
            );
        }
    }

    pub(crate) fn disconnected(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.event["error_origin"] = json!("client");
            pending.event["status"] = Value::Null;
            pending.event["error_class"] = json!("client_disconnect");
            pending.event["success"] = json!(false);
        }
    }

    pub(crate) fn disconnected_before_delivery(&mut self) {
        self.record_cancellation_before_delivery(DeliveryCancellation::Disconnected);
        self.disconnected();
    }

    fn record_cancellation_before_delivery(&mut self, cancellation: DeliveryCancellation) {
        let pending = self.pending_mut();
        if pending.event["route"] == "prewarm" {
            pending.prewarm_cancellation = Some(cancellation);
        }
        let event = cancellation.event(&pending.event["request_id"]);
        self.services
            .diagnostics
            .journal
            .event("info", "model_request_cancelled", &event);
    }

    pub(crate) fn drained(&mut self) {
        let pending = self.pending_mut();
        pending.event["error_origin"] = json!("emp");
        pending.event["status"] = Value::Null;
        pending.event["error_class"] = json!("server_draining");
        pending.event["success"] = json!(false);
    }

    pub(crate) fn interrupted(&mut self) {
        if let Some(pending) = &mut self.pending {
            pending.event["error_origin"] = json!("client");
            pending.event["status"] = json!(200);
            pending.event["error_class"] = json!("client_cancelled");
            pending.event["failure_reason"] = json!("interrupted");
            pending.event["response_status"] = json!("incomplete");
            pending.event["success"] = json!(false);
            self.finish();
        }
    }

    pub(crate) fn finish(&mut self) {
        if let Some(pending) = self.pending.take() {
            tokio::task::block_in_place(|| finish_request(&self.services, pending.freeze()));
        }
    }
}

impl Drop for RequestGuard<'_> {
    fn drop(&mut self) {
        self.finish();
    }
}

impl PendingRequest {
    fn observe_event(
        &mut self,
        activity: Option<&RequestActivity<'_>>,
        event: &Value,
        origin: Option<&str>,
    ) -> bool {
        let now = Instant::now();
        let response = event
            .get("response")
            .filter(|value| value.is_object())
            .unwrap_or(event);
        let kind = event["type"].as_str().unwrap_or("");
        let terminal = crate::services::events::terminal_stream_event(event);
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
        if !self.source_clock {
            self.models
                .observe(event, !self.buffered && self.event["transport"] != "http");
            self.first_token = self.models.first_output;
            if self.event["dialect"] == "codex_native" && !terminal {
                self.update_model_facts();
                self.publish_active(activity);
            }
        }
        if !kind.is_empty() && self.event.get("upstream_first_event_ms").is_none() {
            self.event["upstream_first_event_ms"] =
                json!(now.duration_since(self.started).as_millis() as u64);
        }
        self.event
            .as_object_mut()
            .unwrap()
            .extend(reported_usage(event));
        let preserve_native_http_boundary =
            kind.is_empty() && self.event["dialect"] == "codex_native";
        if preserve_native_http_boundary {
        } else if response["status"] == "completed" || kind == "response.completed" {
            self.status(200, "none");
        } else if response["status"] == "incomplete" || kind == "response.incomplete" {
            self.event["error_origin"] = json!(if response["incomplete_details"]["reason"]
                == "interrupted"
            {
                "client"
            } else {
                "upstream"
            });
            self.status(
                200,
                match response["incomplete_details"]["reason"].as_str() {
                    Some("max_output_tokens") => "output_limit",
                    Some("content_filter") => "content_filter",
                    Some("interrupted") => "client_cancelled",
                    _ => "stream_incomplete",
                },
            );
        } else if response["status"] == "failed" || matches!(kind, "response.failed" | "error") {
            let error = &response["error"];
            self.event["error_origin"] = json!(
                origin
                    .or_else(|| error["origin"].as_str().filter(|origin| matches!(
                        *origin,
                        "emp" | "upstream" | "transport" | "claude_cli" | "client"
                    )))
                    .unwrap_or(if self.buffered {
                        "claude_cli"
                    } else {
                        "upstream"
                    })
            );
            self.event["error_code"] = json!(schema::id(&error["code"]));
            self.event["failure_reason"] = json!(schema::id(&error["failure_reason"]));
            self.status(
                event["status"]
                    .as_u64()
                    .or_else(|| error["status"].as_u64())
                    .filter(|status| (400..=599).contains(status))
                    .unwrap_or(502) as u16,
                emp_transport::normalize_error_class(
                    error.get("error_class").and_then(Value::as_str),
                    emp_transport::FailureClass::StreamError,
                )
                .as_str(),
            );
        }
        if terminal {
            self.event["terminal_event_observed"] = json!(true);
            self.event["phase"] = json!("terminal_validation");
        }
        terminal
    }
    fn update_model_facts(&mut self) {
        let mut projected = self
            .models
            .project(self.event["upstream_model"].as_str().unwrap_or_default());
        if projected["model_name_status"] == "same"
            && self.event["client_model"].as_str().is_some_and(|selected| {
                !selected.is_empty() && Some(selected) != self.event["upstream_model"].as_str()
            })
        {
            projected["model_name_status"] = json!("mapped");
        }
        self.event
            .as_object_mut()
            .unwrap()
            .extend(projected.as_object().unwrap().clone());
    }
    fn status(&mut self, status: u16, error: &str) {
        self.event["status"] = json!(status);
        self.event["error_class"] = json!(error);
        self.event["success"] = json!((200..300).contains(&status) && error == "none");
    }
    fn upstream_error_detail(&self, journal: &emp_storage::diagnostics::Journal, message: &str) {
        journal.event("warn", "upstream_request_rejected", &json!({
            "request_id": self.event["request_id"], "provider_id": self.event["provider_id"],
            "upstream_model": self.event["upstream_model"], "status": self.event["status"],
            "error_code": self.event["error_code"], "failure_reason": self.event["failure_reason"],
            "upstream_message": message,
        }));
    }
    fn freeze(mut self) -> RequestOutcome {
        let duration_ms = self.started.elapsed().as_millis() as u64;
        self.event["duration_ms"] = json!(duration_ms);
        if self.event["route"] != "prewarm" {
            self.record_performance(duration_ms);
            self.update_model_facts();
            if let Some(identity) = &self.identity {
                self.event["account_id"] = json!(identity.account_id);
            }
        }
        RequestOutcome {
            event: self.event,
            identity: self.identity,
            started: self.started,
            aggregate_owner: self.aggregate_owner,
            prewarm_cancellation: self.prewarm_cancellation,
        }
    }

    fn publish_active(&self, activity: Option<&RequestActivity<'_>>) {
        if let Some(activity) = activity
            && self.event["route"] != "prewarm"
        {
            activity.guard().observe_request(&self.event);
        }
    }
}
