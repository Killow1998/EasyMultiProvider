use super::{
    body::JsonBody,
    error::{EntryError, HttpReply, JsonReply},
    management::{ManagementSession, same_origin},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    api::modules,
    app::ServerState,
    http::request::{Request, RequestMethod},
    server::{connection::ConnectionContext, run_blocking},
    services::modules::Action,
};
use axum::{
    Extension, Router,
    extract::{FromRequest, FromRequestParts, State},
    http::request::Parts,
    routing::get,
};
use emp_modules::Module;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const PATH: &str = "/api/modules";

pub(crate) fn matches_write(request: Request<'_>) -> bool {
    request.method == RequestMethod::Post && request.raw_path() == PATH
}

pub(crate) struct ModulePanel(Module);

impl ModulePanel {
    pub(crate) fn for_path(path: &str) -> Option<Self> {
        let id = path.strip_prefix("/assets/module-")?.strip_suffix(".js")?;
        Module::ALL
            .into_iter()
            .find(|module| module.id() == id)
            .map(Self)
    }

    pub(crate) fn require_origin(
        self,
        request: Request<'_>,
        port: u16,
    ) -> Result<Self, EntryError> {
        if same_origin(request, port) {
            Ok(self)
        } else {
            Err(EntryError::NotFound)
        }
    }

    pub(crate) fn read(self, request: Request<'_>, state: &ServerState) -> HttpReply {
        let revision = request.target.split_once('?').and_then(|(_, query)| {
            query
                .split('&')
                .find_map(|part| part.strip_prefix("revision="))
        });
        match state.backend.modules.panel(self.0, revision) {
            Some(source) => HttpReply::new(200, source.into_bytes())
                .with_content_type("text/javascript; charset=utf-8")
                .with_header("Cache-Control", "no-store"),
            None => HttpReply::new(404, Vec::new()).with_content_type("text/plain"),
        }
    }
}

impl FromRequestParts<Arc<ServerState>> for ModulePanel {
    type Rejection = EntryError;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let request = raw_head(&parts.extensions)?.request();
        Self::for_path(request.raw_path())
            .ok_or(EntryError::NotFound)?
            .require_origin(request, state.port)
    }
}

pub(in crate::server) fn routes() -> Router<Arc<ServerState>> {
    let mut router = Router::new().route(PATH, get(read).post(write));
    for module in Module::ALL {
        router = router.route(
            &format!("/assets/module-{}.js", module.id()),
            get(read_panel),
        );
    }
    router
}

async fn read(
    _access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
) -> Result<JsonReply, EntryError> {
    run_blocking(move || modules::read(&state))
        .await
        .map_err(|_| EntryError::internal())
}

async fn read_panel(
    _access: RequestAccess,
    panel: ModulePanel,
    State(state): State<Arc<ServerState>>,
    request: axum::extract::Request,
) -> Result<HttpReply, EntryError> {
    let raw = raw_head(request.extensions())?.clone();
    run_blocking(move || panel.read(raw.request(), &state))
        .await
        .map_err(|_| EntryError::internal())
}

pub(crate) enum ModuleAction {
    CheckUpdates,
    Change {
        module: Module,
        action: Action,
        revision: Option<String>,
    },
}

impl ModuleAction {
    pub(crate) fn parse(body: Value) -> Result<Self, EntryError> {
        if body["action"] == "check_updates" {
            return Ok(Self::CheckUpdates);
        }
        let module = Module::deserialize(&body["id"])
            .map_err(|_| EntryError::Json(400, "Unknown EMP module", None))?;
        let action = Action::deserialize(&body["action"]).map_err(|_| {
            EntryError::Json(
                409,
                "The EMP module could not be changed",
                Some("invalid_module_action"),
            )
        })?;
        Ok(Self::Change {
            module,
            action,
            revision: body["revision"].as_str().map(str::to_owned),
        })
    }

    pub(crate) fn execute(self, state: &ServerState) -> JsonReply {
        let backend = &state.backend;
        let updater = &state.updates.manager;
        let Self::Change {
            module,
            action,
            revision,
        } = self
        else {
            return match backend.modules.check_updates(updater) {
                Ok(updates) => JsonReply::new(200, json!({"updates":updates}).to_string())
                    .with_header("Cache-Control", "no-store"),
                Err(error) => {
                    JsonReply::error(409, "Could not check EMP module updates", Some(&error))
                }
            };
        };
        let change = || {
            backend
                .modules
                .change(module, action, revision.as_deref(), updater, || {
                    if module == Module::Team {
                        backend.team.purge_local(state).map_err(|error| error.1)?;
                    }
                    Ok(())
                })
        };
        let result = if module == Module::Team {
            backend.team.change_module(state, change)
        } else {
            change()
        };
        match result {
            Ok(()) => {
                if matches!(module, Module::Aggregate | Module::Team | Module::Aero)
                    && crate::services::catalog::refresh_catalog(state).is_err()
                {
                    crate::services::account_catalog::request_refresh(state, false);
                }
                modules::read(state)
            }
            Err(error) => {
                JsonReply::error(409, "The EMP module could not be changed", Some(&error))
            }
        }
    }
}

impl FromRequest<Arc<ServerState>> for JsonBody<ModuleAction> {
    type Rejection = EntryError;

    async fn from_request(
        request: axum::extract::Request,
        state: &Arc<ServerState>,
    ) -> Result<Self, EntryError> {
        let JsonBody(body) = JsonBody::<Value>::from_request(request, state).await?;
        Ok(Self(ModuleAction::parse(body)?))
    }
}

async fn write(
    access: RequestAccess,
    _session: ManagementSession,
    State(state): State<Arc<ServerState>>,
    Extension(connection): Extension<ConnectionContext>,
    JsonBody(action): JsonBody<ModuleAction>,
) -> Result<JsonReply, EntryError> {
    connection.retain_request_access(access);
    run_blocking(move || {
        let _connection = connection;
        action.execute(&state)
    })
    .await
    .map_err(|_| EntryError::internal())
}
