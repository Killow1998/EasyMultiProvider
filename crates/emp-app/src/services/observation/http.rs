//! One content-free receipt around every HTTP handler, including early failures.
use crate::app::ServerState;
use crate::http::request::{Request, RequestMethod};
use serde_json::{Value, json};
use std::io::Write;
use std::sync::Arc;
use std::time::Instant;

pub(crate) struct HttpObservation {
    diagnostics: Arc<emp_state::diagnostics::Diagnostics>,
    ledger: Arc<emp_state::usage::ledger::UsageLedger>,
    events: Arc<crate::services::management_events::ManagementEvents>,
    started: Instant,
    fields: Value,
    pub(crate) model: Option<super::request::RequestObservation>,
}

impl HttpObservation {
    pub(crate) fn new(state: &ServerState) -> Self {
        let fields = json!({"request_id": crate::util::random_hex(8).ok(), "method":"unknown", "path":"/unknown", "status":null, "result":"handler_finished", "transport":"http"});
        state
            .backend
            .diagnostics
            .journal
            .event("info", "http_request_started", &fields);
        Self {
            diagnostics: Arc::clone(&state.backend.diagnostics),
            ledger: Arc::clone(&state.backend.usage.ledger),
            events: Arc::clone(&state.backend.management_events),
            started: Instant::now(),
            fields,
            model: None,
        }
    }

    pub(crate) fn received(&mut self, request: Request<'_>) {
        if request.method == RequestMethod::Post
            && matches!(
                request.raw_path(),
                "/v1/responses" | "/v1/responses/compact"
            )
        {
            self.model = Some(
                super::request::RequestObservation::new(
                    Arc::clone(&self.diagnostics),
                    self.request_id().map(str::to_owned),
                    None,
                    "http",
                    if request.raw_path().ends_with("/compact") {
                        "compact"
                    } else {
                        "responses"
                    },
                )
                .with_ledger(Arc::clone(&self.ledger), Arc::clone(&self.events)),
            );
        }
        self.fields["method"] = json!(match request.method {
            RequestMethod::Get => "GET",
            RequestMethod::Head => "HEAD",
            RequestMethod::Post => "POST",
            RequestMethod::Delete => "DELETE",
        });
        self.fields["path"] = json!(safe_path(request.raw_path()));
        self.fields["declared_bytes"] = json!(
            request
                .header("content-length")
                .and_then(|value| value.parse::<u64>().ok())
        );
        if request
            .header("upgrade")
            .is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        {
            self.fields["transport"] = json!("websocket");
        } else if request.raw_path() == "/api/accounts/events" {
            self.fields["transport"] = json!("sse");
        }
    }

    pub(crate) fn write_response(&mut self, stream: &mut impl Write, bytes: &[u8]) {
        // Cancellation can return an empty buffer before response headers.
        // Writing zero bytes is not evidence of a delivered HTTP response.
        if bytes.is_empty() {
            self.fields["response_bytes"] = json!(0);
            self.fields["result"] = json!("no_response");
            return;
        }
        self.fields["status"] = json!(
            bytes
                .get(..12)
                .and_then(|head| std::str::from_utf8(head).ok())
                .and_then(|head| head.split_whitespace().nth(1)?.parse::<u16>().ok())
        );
        self.fields["response_bytes"] = json!(bytes.len());
        self.fields["result"] = json!(match self.fields["status"].as_u64() {
            Some(400..=499) => "rejected",
            Some(500..=599) => "failed",
            _ => "responded",
        });
        let result = stream.write_all(bytes).and_then(|()| stream.flush());
        if result.is_err() {
            self.failure("write_failed");
        }
        if let Some(model) = &mut self.model {
            if self.fields["status"]
                .as_u64()
                .is_some_and(|status| status >= 400)
                && let Some(index) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
            {
                model.buffered_error(&bytes[index + 4..], result.is_ok());
            }
            model.http_response(&self.fields["status"], result);
        }
    }

    pub(crate) fn request_id(&self) -> Option<&str> {
        self.fields["request_id"].as_str()
    }

    pub(crate) fn failure(&mut self, class: &'static str) {
        self.fields["result"] = json!(class);
    }
}

impl Drop for HttpObservation {
    fn drop(&mut self) {
        self.fields["duration_ms"] = json!(self.started.elapsed().as_millis() as u64);
        if std::thread::panicking() {
            self.fields["result"] = json!("handler_panicked");
        }
        let level = if matches!(
            self.fields["result"].as_str(),
            Some("failed" | "write_failed" | "handler_panicked" | "socket_setup_failed")
        ) {
            "warning"
        } else {
            "info"
        };
        self.diagnostics
            .journal
            .event(level, "http_request_completed", &self.fields);
    }
}

fn safe_path(path: &str) -> String {
    // Unknown paths and user-controlled identifiers never enter the journal.
    if let Some(rest) = path.strip_prefix("/api/accounts/") {
        if matches!(rest, "events" | "import") {
            return path.to_owned();
        }
        let suffix = rest.split_once('/').map(|(_, tail)| tail).unwrap_or("");
        return match suffix {
            "" | "quota" | "quota-reset" | "quota-history" | "models" | "models/refresh" => {
                format!(
                    "/api/accounts/{{account}}{}{}",
                    if suffix.is_empty() { "" } else { "/" },
                    suffix
                )
            }
            _ => "/api/accounts/{account}/unknown".into(),
        };
    }
    if path.starts_with("/v1/live/") {
        return "/v1/live/{call}".into();
    }
    if path.starts_with("/v1/models/") {
        return "/v1/models/{model}".into();
    }
    match path {
        "/"
        | "/index.html"
        | "/healthz"
        | "/v1/models"
        | "/v1/responses"
        | "/v1/responses/compact"
        | "/v1/live"
        | "/v1/alpha/search"
        | "/api/accounts"
        | "/api/capabilities"
        | "/api/catalog/context-preference"
        | "/api/catalog/refresh"
        | "/api/client-events"
        | "/api/config"
        | "/api/diagnostics"
        | "/api/integration"
        | "/api/integration/enable"
        | "/api/integration/reload"
        | "/api/integration/restore"
        | "/api/integration/verify"
        | "/api/migration/export"
        | "/api/migration/export/confirm"
        | "/api/migration/import"
        | "/api/models/audio-test-sound"
        | "/api/models/metadata"
        | "/api/models/vision-test-image"
        | "/api/providers/discover"
        | "/api/quit"
        | "/api/request-limits"
        | "/api/runtime/claude-cli"
        | "/api/runtime/scan"
        | "/api/session"
        | "/api/support-report"
        | "/api/updates"
        | "/api/updates/check"
        | "/api/updates/install"
        | "/api/usage"
        | "/api/usage/scan" => path.into(),
        _ => "/unknown".into(),
    }
}
