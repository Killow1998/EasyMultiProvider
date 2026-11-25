use super::{
    body::JsonBody,
    error::{EntryError, HttpReply, JsonReply},
    management::{read_route, same_origin, write_route},
    proxy::RequestAccess,
    raw_head,
};
use crate::{
    app::ServerState,
    http::request::Request,
    server::run_blocking,
    services::{
        configuration::{CommittedChange, after_commit},
        modules::Action,
    },
};
use axum::{
    Router,
    extract::{FromRequest, FromRequestParts, State},
    http::request::Parts,
    routing::{MethodFilter, get},
};
use emp_modules::Module;
use serde::Deserialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub(crate) const PATH: &str = "/api/modules";

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
    let write = write_route(MethodFilter::POST, |action: ModuleAction, state, _| {
        action.execute(state)
    });
    let mut router = Router::new().route(PATH, read_route(module_status).merge(write));
    for module in Module::ALL {
        router = router.route(
            &format!("/assets/module-{}.js", module.id()),
            get(read_panel),
        );
    }
    router
}

fn module_status(state: &ServerState) -> JsonReply {
    JsonReply::ok_json(&json!({
        "modules":state.backend.modules.status(),
        "migration_allowed":!state.backend.team.migration_restricted(),
    }))
    .with_header("Cache-Control", "no-store")
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
        let updater = &state.lifecycle.updates().manager;
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
                let _ = after_commit(state, CommittedChange::Module(module));
                module_status(state)
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
