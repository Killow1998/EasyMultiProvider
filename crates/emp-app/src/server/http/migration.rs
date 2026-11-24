use super::{
    body::JsonBody,
    error::{EntryError, HttpReply},
    management::{ManagementSession, consume_export_confirmation, issue_export_confirmation},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    app::ServerState,
    http::request::{Request, RequestMethod, read_json_body},
    server::{connection::ConnectionContext, run_blocking},
    services::migration::{self, ExportOptions, MigrationError, Selection, lan, preview},
    util::system_now,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, State},
    routing::post,
};
use base64::{Engine as _, engine::general_purpose::STANDARD};
use serde_json::{Map, Value, json};
use std::{net::TcpStream, sync::Arc};

const EXPORT: &str = "/api/migration/export";
const LAN_START: &str = "/api/migration/lan/start";

#[derive(Clone, Copy)]
pub(crate) enum MigrationRoute {
    ConfirmExport,
    Export,
    Decode,
    Demo,
    Preview,
    Import,
    LanStart,
    LanStatus,
    LanApprove,
    LanCancel,
    LanReceive,
}

const ROUTES: [(&str, MigrationRoute); 11] = [
    (
        "/api/migration/export/confirm",
        MigrationRoute::ConfirmExport,
    ),
    (EXPORT, MigrationRoute::Export),
    ("/api/migration/decode", MigrationRoute::Decode),
    ("/api/migration/demo", MigrationRoute::Demo),
    ("/api/migration/preview", MigrationRoute::Preview),
    ("/api/migration/import", MigrationRoute::Import),
    (LAN_START, MigrationRoute::LanStart),
    ("/api/migration/lan/status", MigrationRoute::LanStatus),
    ("/api/migration/lan/approve", MigrationRoute::LanApprove),
    ("/api/migration/lan/cancel", MigrationRoute::LanCancel),
    ("/api/migration/lan/receive", MigrationRoute::LanReceive),
];

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    ROUTES
        .into_iter()
        .fold(Router::new(), |router, (path, route)| {
            router.route(path, post(write).layer(Extension(route)))
        })
}

impl MigrationRoute {
    pub(crate) fn for_request(request: Request<'_>) -> Option<Self> {
        if request.method != RequestMethod::Post {
            return None;
        }
        let path = request.raw_path();
        ROUTES
            .iter()
            .find_map(|&(route_path, route)| (path == route_path).then_some(route))
    }

    pub(crate) fn legacy(
        self,
        stream: &mut TcpStream,
        request: Request<'_>,
        prefix: Vec<u8>,
        state: &ServerState,
        now: f64,
    ) -> Result<HttpReply, HttpReply> {
        let ManagementSession(session) =
            ManagementSession::require(request, state, now).map_err(EntryError::reply)?;
        check_permission(state)?;
        let body = read_json_body(stream, request, prefix, state)
            .map_err(|error| EntryError::from(error).reply())?;
        self.parse(body)
            .execute(state, &session, now, request.observation_id)
            .map_err(EntryError::reply)
    }

    fn parse(self, body: Value) -> MigrationCommand {
        let Value::Object(mut fields) = body else {
            unreachable!("migration body is a validated JSON object");
        };
        let string =
            |fields: &mut Map<String, Value>, key| take_string(fields, key).unwrap_or_default();
        match self {
            Self::ConfirmExport => MigrationCommand::ConfirmExport,
            Self::Export => MigrationCommand::Export {
                confirmation: string(&mut fields, "confirmation"),
                password: string(&mut fields, "password"),
                json_reply: fields.contains_key("selections"),
                options: export_options(&mut fields),
            },
            Self::Decode => MigrationCommand::Decode {
                bundle: string(&mut fields, "bundle"),
                password: string(&mut fields, "password"),
            },
            Self::Demo => MigrationCommand::Demo,
            Self::Preview => MigrationCommand::Preview(preview_input(&mut fields)),
            Self::Import if fields.contains_key("token") => {
                MigrationCommand::ImportPreview(preview_input(&mut fields))
            }
            Self::Import => MigrationCommand::ImportBundle {
                bundle: string(&mut fields, "bundle"),
                password: string(&mut fields, "password"),
            },
            Self::LanStart => MigrationCommand::LanStart {
                confirmation: string(&mut fields, "confirmation"),
                address: take_string(&mut fields, "address").filter(|value| !value.is_empty()),
                options: export_options(&mut fields),
            },
            Self::LanStatus => MigrationCommand::Lan(lan::SenderCommand::Status),
            Self::LanApprove => {
                MigrationCommand::Lan(lan::SenderCommand::Approve(string(&mut fields, "code")))
            }
            Self::LanCancel => MigrationCommand::Lan(lan::SenderCommand::Cancel),
            Self::LanReceive => MigrationCommand::LanReceive {
                code: string(&mut fields, "code"),
                receiver: string(&mut fields, "receiver"),
            },
        }
    }
}

fn take_string(fields: &mut Map<String, Value>, key: &str) -> Option<String> {
    match fields.remove(key) {
        Some(Value::String(value)) => Some(value),
        _ => None,
    }
}

fn selection(fields: &mut Map<String, Value>) -> Option<Selection> {
    let Some(Value::Array(items)) = fields.remove("selections") else {
        return None;
    };
    Some(Selection {
        items,
        include_context: fields
            .get("include_context")
            .and_then(Value::as_bool)
            .unwrap_or(true),
        include_rename: fields
            .get("include_rename")
            .and_then(Value::as_bool)
            .unwrap_or(true),
    })
}

fn export_options(fields: &mut Map<String, Value>) -> ExportOptions {
    ExportOptions {
        groups: match fields.remove("groups") {
            Some(Value::Array(groups)) => Some(groups),
            _ => None,
        },
        selection: selection(fields),
    }
}

fn preview_input(fields: &mut Map<String, Value>) -> preview::PreviewInput {
    preview::PreviewInput {
        token: take_string(fields, "token").unwrap_or_default(),
        selection: selection(fields),
        preference_choices: fields.remove("preference_choices").unwrap_or_default(),
    }
}

enum MigrationCommand {
    ConfirmExport,
    Export {
        confirmation: String,
        password: String,
        json_reply: bool,
        options: ExportOptions,
    },
    Decode {
        bundle: String,
        password: String,
    },
    Demo,
    Preview(preview::PreviewInput),
    ImportBundle {
        bundle: String,
        password: String,
    },
    ImportPreview(preview::PreviewInput),
    LanStart {
        confirmation: String,
        address: Option<String>,
        options: ExportOptions,
    },
    Lan(lan::SenderCommand),
    LanReceive {
        code: String,
        receiver: String,
    },
}

impl MigrationCommand {
    fn execute(
        self,
        state: &ServerState,
        session: &str,
        now: f64,
        request_id: Option<&str>,
    ) -> Result<HttpReply, EntryError> {
        let lan = matches!(
            self,
            Self::LanStart { .. } | Self::Lan(_) | Self::LanReceive { .. }
        );
        let result = match self {
            Self::ConfirmExport => return issue_export_confirmation(state, now),
            Self::Export {
                confirmation,
                password,
                json_reply,
                options,
            } => {
                consume_export_confirmation(state, &confirmation, EXPORT, now)?;
                let reply = match migration::export(request_id, state, &password, &options) {
                    Ok(exported) if json_reply => HttpReply::new(
                        200,
                        json!({
                            "bundle": STANDARD.encode(exported.bundle),
                            "summary": exported.summary,
                        })
                        .to_string(),
                    ),
                    Ok(exported) => HttpReply::new(200, exported.bundle)
                        .with_content_type("application/octet-stream")
                        .with_header("Cache-Control", "no-store")
                        .with_header("Content-Disposition", "attachment; filename=\"EMP.emp\"")
                        .with_header("X-EMP-Export-Summary", exported.summary.to_string()),
                    Err(error) => migration_error(error),
                };
                return Ok(reply);
            }
            Self::Decode { bundle, password } => {
                preview::decode(state, session, &bundle, &password)
            }
            Self::Demo => preview::demo(state, session),
            Self::Preview(input) => preview::selected(
                request_id,
                state,
                session,
                preview::PreviewAction::Review,
                input,
            ),
            Self::ImportBundle { bundle, password } => {
                migration::import(request_id, state, &bundle, &password)
            }
            Self::ImportPreview(input) => preview::selected(
                request_id,
                state,
                session,
                preview::PreviewAction::Import,
                input,
            ),
            Self::LanStart {
                confirmation,
                address,
                options,
            } => {
                consume_export_confirmation(state, &confirmation, LAN_START, now)?;
                lan::send(state, lan::SenderCommand::Start { address, options })
            }
            Self::Lan(command) => lan::send(state, command),
            Self::LanReceive { code, receiver } => lan::receive(state, session, &code, &receiver),
        };
        Ok(match result {
            Ok(body) => {
                let reply = HttpReply::new(200, body.to_string());
                if lan {
                    reply.with_header("Cache-Control", "no-store")
                } else {
                    reply
                }
            }
            Err(error) => migration_error(error),
        })
    }
}

fn migration_error(error: MigrationError) -> HttpReply {
    match error {
        MigrationError::Invalid { message, code } => HttpReply::error(400, &message, code),
        MigrationError::Internal => EntryError::internal().reply(),
    }
}

struct MigrationRequest {
    session: String,
    now: f64,
    command: MigrationCommand,
}

impl FromRequest<Arc<ServerState>> for JsonBody<MigrationRequest> {
    type Rejection = HttpReply;

    async fn from_request(
        request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, HttpReply> {
        let raw = raw_head(request.extensions()).map_err(EntryError::reply)?;
        let access =
            RequestAccess::require(raw.request(), state, None).map_err(EntryError::reply)?;
        let now = system_now();
        let ManagementSession(session) =
            ManagementSession::require(raw.request(), state, now).map_err(EntryError::reply)?;
        let route = *request
            .extensions()
            .get::<MigrationRoute>()
            .expect("migration route");
        let connection = request
            .extensions()
            .get::<ConnectionContext>()
            .expect("connection context")
            .clone();
        connection.retain_request_access(access);
        let permission_state = state.clone();
        run_blocking(move || {
            let _connection = connection;
            check_permission(&permission_state)
        })
        .await
        .map_err(|_| EntryError::internal().reply())??;
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state)
            .await
            .map_err(EntryError::reply)?;
        Ok(Self(MigrationRequest {
            session,
            now,
            command: route.parse(body),
        }))
    }
}

fn check_permission(state: &ServerState) -> Result<(), HttpReply> {
    state.backend.team.check_migration(state).map_err(
        |crate::services::team::Failure(status, code)| {
            let message = if code == "workspace_migration_permission_required" {
                "Service migration requires a team administrator"
            } else {
                "Could not confirm team permissions for service migration"
            };
            HttpReply::error(status, message, Some(&code))
        },
    )
}

async fn write(
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(MigrationRequest {
        session,
        now,
        command,
    }): JsonBody<MigrationRequest>,
) -> Result<HttpReply, EntryError> {
    run_blocking(move || command.execute(&state, &session, now, connection.request_id()))
        .await
        .map_err(|_| EntryError::internal())?
}
