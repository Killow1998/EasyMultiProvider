use super::{
    body::JsonBody,
    error::{EntryError, JsonReply},
    management::ManagementSession,
    proxy::RequestAccess,
};
use crate::{
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::{connection::ConnectionContext, run_blocking},
    services::{
        app_icon::Selection,
        configuration::{Language, update_language},
        observation::diagnostics::schema,
        runtime::compatibility_snapshot,
    },
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::post,
};
use serde_json::{Value, json};
use std::sync::Arc;

#[derive(Clone, Copy)]
pub(crate) enum LocalWriteRoute {
    Language,
    AppIcon,
    UsageScan,
    ClientEvent,
    RuntimeScan,
}

const ROUTES: [(&str, LocalWriteRoute); 5] = [
    ("/api/language", LocalWriteRoute::Language),
    ("/api/app-icon", LocalWriteRoute::AppIcon),
    ("/api/usage/scan", LocalWriteRoute::UsageScan),
    ("/api/client-events", LocalWriteRoute::ClientEvent),
    ("/api/runtime/scan", LocalWriteRoute::RuntimeScan),
];

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    ROUTES.into_iter().fold(Router::new(), |router, (path, _)| {
        router.route(path, post(write))
    })
}

impl LocalWriteRoute {
    pub(crate) fn for_request(request: Request<'_>) -> Option<Self> {
        if request.method != RequestMethod::Post {
            return None;
        }
        let path = request.raw_path();
        ROUTES
            .into_iter()
            .find_map(|(route, operation)| (route == path).then_some(operation))
    }

    fn with_body(self, body: Value) -> Result<LocalWrite, EntryError> {
        Ok(match self {
            Self::Language => {
                let language = match body["language"].as_str() {
                    Some("en") => Language::English,
                    Some("zh-CN") => Language::SimplifiedChinese,
                    Some(_) => {
                        return Err(EntryError::Json(400, "language must be en or zh-CN", None));
                    }
                    None => return Err(EntryError::Json(400, "language is required", None)),
                };
                LocalWrite::Language(language)
            }
            Self::AppIcon => LocalWrite::AppIcon(serde_json::from_value(body).map_err(|_| {
                EntryError::Json(400, "app_icon_invalid", Some("app_icon_invalid"))
            })?),
            Self::UsageScan => LocalWrite::UsageScan,
            Self::ClientEvent => LocalWrite::ClientEvent(ClientEvent::parse(&body)?),
            Self::RuntimeScan => LocalWrite::RuntimeScan,
        })
    }
}

enum LocalWrite {
    Language(Language),
    AppIcon(Selection),
    UsageScan,
    ClientEvent(ClientEvent),
    RuntimeScan,
}

impl LocalWrite {
    fn execute(self, state: &ServerState) -> JsonReply {
        let (status, value) = match self {
            Self::Language(language) => match update_language(state, language) {
                Ok(()) => (200, json!({"language":language.as_str()})),
                Err(error) => return JsonReply::error(400, &error, None),
            },
            Self::AppIcon(selection) => match state.backend.app_icon.save(selection) {
                Ok(value) => (200, value),
                Err(code) => {
                    let status = if code == "app_icon_invalid" { 400 } else { 500 };
                    return JsonReply::error(status, code, Some(code));
                }
            },
            Self::UsageScan => {
                let Some(id) = state.backend.usage.queue_scan(state) else {
                    return JsonReply::error(503, "EMP is shutting down", None);
                };
                let mut status = state.backend.usage.status();
                status["command_id"] = json!(id);
                (202, status)
            }
            Self::ClientEvent(event) => {
                state.backend.diagnostics.journal.event(
                    event.level,
                    "web_client_phase",
                    &event.fields,
                );
                (200, json!({"ok":true}))
            }
            Self::RuntimeScan => (200, compatibility_snapshot(state, true)),
        };
        JsonReply::new(status, value.to_string())
    }
}

struct ClientEvent {
    level: &'static str,
    fields: Value,
}

impl ClientEvent {
    fn parse(body: &Value) -> Result<Self, EntryError> {
        let kind = body["kind"].as_str().unwrap_or("");
        let phase = body["phase"].as_str().unwrap_or("");
        if !["integration_load", "window_error", "unhandled_rejection"].contains(&kind)
            || !["render_completed", "failed"].contains(&phase)
        {
            return Err(EntryError::Json(
                400,
                "invalid web client diagnostic event",
                None,
            ));
        }
        let failure = match body.get("failure_class") {
            None => "none",
            Some(Value::String(failure))
                if [
                    "none",
                    "network",
                    "http_error",
                    "missing_element",
                    "type_error",
                    "syntax_error",
                ]
                .contains(&failure.as_str()) =>
            {
                failure
            }
            _ => "unknown",
        };
        let mut fields = json!({"kind":kind,"phase":phase,"failure_class":failure});
        let page = schema::id(&body["page_id"]);
        if !page.is_empty() {
            fields["page_id"] = json!(page);
        }
        for (field, maximum) in [
            ("duration_ms", 3_600_000),
            ("http_status", 599),
            ("line", 10_000_000),
            ("column", 10_000_000),
        ] {
            let value = schema::integer(&body[field], maximum);
            if !value.is_null() {
                fields[field] = value;
            }
        }
        let level = if phase == "failed" { "warning" } else { "info" };
        Ok(Self { level, fields })
    }
}

impl FromRequest<Arc<ServerState>> for JsonBody<LocalWrite> {
    type Rejection = EntryError;

    async fn from_request(
        mut request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let route = request
            .extensions_mut()
            .remove::<LocalWriteRoute>()
            .ok_or(EntryError::NotFound)?;
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(route.with_body(body)?))
    }
}

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(operation): JsonBody<LocalWrite>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        operation.execute(&state)
    })
    .await
    .map_err(|_| EntryError::internal())
}
