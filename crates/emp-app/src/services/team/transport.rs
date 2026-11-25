use super::{Failure, Result};
use crate::app::ServerState;
use crate::services::configuration::{CommittedChange, after_commit};
use emp_modules::{
    Command, Module,
    team::{Endpoint, MAX_BACKUP_WIRE_BYTES, Method, Operation, Request},
};
use emp_transport::HttpMethod;
use serde::de::DeserializeOwned;
use serde_json::Value;
use std::{collections::BTreeMap, time::Duration};

pub(super) fn policy<T: DeserializeOwned>(state: &ServerState, request: Request) -> Result<T> {
    state
        .backend
        .modules
        .call_typed(Module::Team, Command::Team { request })
        .map_err(|error| {
            let status = match error.as_str() {
                "workspace_login_required" => 401,
                "invalid_workspace_identity" | "invalid_workspace_share" => 502,
                _ => 400,
            };
            Failure(status, error)
        })
}

pub(super) fn plan_endpoint(
    state: &ServerState,
    base: &str,
    operation: Operation,
) -> Result<Endpoint> {
    policy(
        state,
        Request::Endpoint {
            base: base.into(),
            operation,
        },
    )
}

pub(super) fn request(
    state: &ServerState,
    base: &str,
    token: Option<&str>,
    operation: Operation,
    body: Option<&Value>,
) -> Result<Value> {
    let endpoint = plan_endpoint(state, base, operation)?;
    let body = body.map(|body| serde_json::to_vec(body).expect("workspace request"));
    let reply = send(state, &endpoint, token, body)?;
    let valid = match operation {
        Operation::ListMembers => reply["members"].as_array().is_some_and(|members| {
            members
                .iter()
                .all(|member| member["id"].as_str().is_some_and(super::valid_identifier))
        }),
        Operation::Usage | Operation::UsageFiltered => {
            reply["rows"].as_array().is_some_and(|rows| {
                rows.iter().all(|row| {
                    row["member"].as_str().is_some_and(super::valid_identifier)
                        && row["requests"].as_u64().is_some()
                        && row["unknown_cost_requests"].as_u64().is_some()
                        && row["models"].as_array().is_some_and(|models| {
                            models.iter().all(|model| {
                                model["model"].is_string()
                                    && model["unknown_cost_requests"].as_u64().is_some()
                            })
                        })
                })
            })
        }
        Operation::Audit => {
            (reply["before"].is_null() || reply["before"].as_u64().is_some())
                && reply["entries"].as_array().is_some_and(|entries| {
                    entries.iter().all(|entry| {
                        entry["at"].as_i64().is_some()
                            && ["actor", "actor_login", "kind"]
                                .iter()
                                .all(|key| entry[*key].is_string())
                            && ["target", "target_login"]
                                .iter()
                                .all(|key| entry[*key].is_null() || entry[*key].is_string())
                            && entry["details"].is_object()
                    })
                })
        }
        _ => true,
    };
    if !valid {
        return Err(Failure(502, "invalid_workspace_reply".into()));
    }
    Ok(reply)
}

pub(super) fn send(
    state: &ServerState,
    endpoint: &Endpoint,
    token: Option<&str>,
    body: Option<Vec<u8>>,
) -> Result<Value> {
    let mut headers = BTreeMap::from([("Accept".into(), "application/json".into())]);
    if let Some(token) = token {
        headers.insert("Authorization".into(), format!("Bearer {token}"));
    }
    let (method, body) = match (&endpoint.method, body) {
        (Method::Get, None) => (HttpMethod::Get, None),
        (Method::Post, Some(body)) => {
            headers.insert("Content-Type".into(), "application/json".into());
            (HttpMethod::Post, Some(body))
        }
        _ => return Err(Failure(400, "invalid_module_reply".into())),
    };
    let transport = &state.backend.transport;
    let (timeout, limit) = if endpoint.url.ends_with("/workspace/v1/backup") {
        (120, MAX_BACKUP_WIRE_BYTES)
    } else {
        (20, 16 * 1024 * 1024)
    };
    let (status, bytes) = transport
        .runtime
        .block_on(async {
            tokio::time::timeout(Duration::from_secs(timeout), async {
                let response = transport
                    .client
                    .open(method, &endpoint.url, headers, body, false)
                    .await?;
                let status = response.status();
                response
                    .read_limited(limit)
                    .await
                    .map(|bytes| (status, bytes))
            })
            .await
        })
        .map_err(|_| Failure(503, "workspace_timeout".into()))?
        .map_err(|_| Failure(503, "workspace_connection_failed".into()))?;
    let body: Value = serde_json::from_slice(&bytes)
        .map_err(|_| Failure(502, "invalid_workspace_reply".into()))?;
    if !(200..300).contains(&status) {
        let code = body["error"]["code"]
            .as_str()
            .unwrap_or("workspace_rejected_request");
        if status == 401
            && code == "workspace_authentication_required"
            && let Some(token) = token
        {
            let team = &state.backend.team;
            let mut locked = team.store.lock().map_err(|_| Failure::unavailable())?;
            if let Some(store) = locked.as_mut()
                && store
                    .session
                    .as_ref()
                    .is_some_and(|session| session.token == token)
            {
                store.session = None;
                team.write(&state.backend.configuration.vault, store)
                    .map_err(|_| Failure::unavailable())?;
                drop(locked);
                let _ = after_commit(state, CommittedChange::Team);
            }
        }
        return Err(Failure(status, code.into()));
    }
    Ok(body)
}
